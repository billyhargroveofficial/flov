// flov-whisper-cuda — NVIDIA-CUDA transcription sidecar.
// Same protocol as flov-whisper-cpu — see crates/flov-whisper-cpu/src/main.rs.
// The only difference is that whisper.cpp here was compiled with the CUDA
// backend, so transcription runs on the GPU.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

struct Args {
    model: PathBuf,
    language: String,
}

fn parse_args() -> Result<Args> {
    let mut model: Option<PathBuf> = None;
    let mut language = String::from("ru");
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--model" => {
                model = Some(PathBuf::from(
                    iter.next().context("--model requires a value")?,
                ));
            }
            "--language" => {
                language = iter.next().context("--language requires a value")?;
            }
            other => bail!("unknown argument: {}", other),
        }
    }
    let model = model.context("--model is required")?;
    if !model.exists() {
        bail!("model file not found: {}", model.display());
    }
    Ok(Args { model, language })
}

fn main() {
    if let Err(e) = run() {
        let _ = writeln!(std::io::stderr(), "flov-whisper-cuda error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let total_start = Instant::now();
    let args = parse_args()?;
    let flash_attn = cuda_flash_attention_enabled();
    let model_bytes = std::fs::metadata(&args.model)
        .with_context(|| format!("failed to stat model file: {}", args.model.display()))?
        .len();

    let model_load_start = Instant::now();
    let mut context_params = WhisperContextParameters::default();
    context_params.flash_attn(flash_attn);
    let ctx = WhisperContext::new_with_params(
        args.model.to_str().context("invalid model path")?,
        context_params,
    )
    .context("failed to load whisper model")?;
    log_phase("model_load", model_load_start.elapsed());

    // Allocate Whisper's KV caches and CUDA compute buffers before waiting
    // for PCM. Flov starts this process on PTT-down, so both model upload and
    // state creation overlap microphone capture instead of extending the
    // release-to-text tail.
    let state_create_start = Instant::now();
    let mut state = ctx.create_state().context("failed to create state")?;
    log_phase("state_create", state_create_start.elapsed());

    let threads = inference_threads();
    let ptt_prewarm = env_flag("FLOV_PTT_PREWARM", false);
    let warmup_enabled = ptt_prewarm && env_flag("FLOV_CUDA_WARMUP", true);
    if warmup_enabled {
        // Exercise CUDA module loading, cuBLAS handles and the main Whisper
        // graphs while the user is still speaking. A tiny context is enough
        // to fault in the kernels; the real request below replaces all state
        // because no_context=true.
        const WARMUP_SAMPLES: usize = 3_200; // 200 ms; Whisper requires >=100 ms.
        let warmup_pcm = [0.0f32; WARMUP_SAMPLES];
        let mut warmup_params = full_params(&args.language, threads, Some(256));
        warmup_params.set_max_tokens(1);
        let warmup_start = Instant::now();
        state
            .full(warmup_params, &warmup_pcm)
            .context("CUDA warmup failed")?;
        log_phase("warmup", warmup_start.elapsed());
    }

    let stdin_decode_start = Instant::now();
    let mut buf = Vec::with_capacity(64 * 1024);
    std::io::stdin()
        .lock()
        .read_to_end(&mut buf)
        .context("failed to read stdin")?;

    if buf.len() % 4 != 0 {
        bail!("stdin byte length {} is not a multiple of 4", buf.len());
    }
    let samples: Vec<f32> = buf
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    log_phase("stdin_decode", stdin_decode_start.elapsed());
    let long_form = samples.len() > LONG_FORM_THRESHOLD_SAMPLES;
    let audio_ctx = requested_audio_context(samples.len())?;
    eprintln!(
        "flov-whisper-cuda memory_inputs model_bytes={model_bytes} stdin_bytes={} decoded_samples={} decoded_bytes={} audio_ms={} flash_attn={flash_attn} threads={threads} audio_ctx={audio_ctx:?} ptt_prewarm={ptt_prewarm} warmup={warmup_enabled} long_form={long_form}",
        buf.len(),
        samples.len(),
        samples.len() * std::mem::size_of::<f32>(),
        samples.len() as u64 * 1_000 / 16_000,
    );

    let params = if long_form {
        long_form_params(&args.language, threads)
    } else {
        full_params(&args.language, threads, audio_ctx)
    };

    let full_start = Instant::now();
    state
        .full(params, &samples)
        .context("transcription failed")?;
    log_phase("full", full_start.elapsed());

    let extract_start = Instant::now();
    let n = state.full_n_segments();
    let mut text = String::new();
    for i in 0..n {
        if let Some(segment) = state.get_segment(i) {
            if let Ok(s) = segment.to_str_lossy() {
                text.push_str(&s);
            }
        }
    }
    log_phase("extract", extract_start.elapsed());

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.trim().as_bytes())
        .context("failed to write stdout")?;
    out.flush().ok();
    log_phase("total", total_start.elapsed());
    Ok(())
}

/// Flash Attention is a substantial CUDA speed-up for the normal
/// transcription path. Default it on only for the Linux target tuned here;
/// other platforms can opt in after their own regression run.
fn cuda_flash_attention_enabled() -> bool {
    env_flag("FLOV_CUDA_FLASH_ATTN", cfg!(target_os = "linux"))
}

// Whisper decodes audio in 30-second windows. The PTT-tuned params below
// (single_segment + no_timestamps + greedy) silently drop words at every
// window boundary once the input is longer than one window, which HTTP
// voice messages routinely are.
const LONG_FORM_THRESHOLD_SAMPLES: usize = 16_000 * 30;

// Reference long-form decode for inputs over one whisper window: timestamp
// tokens let whisper.cpp seek to the last complete segment instead of hopping
// fixed 30-second strides, and beam search recovers boundary words that
// greedy decoding loses. Latency-insensitive: only the HTTP path feeds audio
// this long. FLOV_WHISPER_AUDIO_CTX is intentionally not applied — a shrunk
// encoder context is a PTT latency trick and corrupts multi-window decoding.
fn long_form_params<'a>(language: &'a str, threads: i32) -> FullParams<'a, 'static> {
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_n_threads(threads);
    params.set_translate(false);
    params.set_no_context(false);
    params.set_single_segment(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_timestamps(false);
    params.set_temperature_inc(if env_flag("FLOV_WHISPER_TEMPERATURE_FALLBACK", true) {
        0.2
    } else {
        0.0
    });
    params.set_language(Some(language));
    params
}

fn full_params<'a>(
    language: &'a str,
    threads: i32,
    audio_ctx: Option<i32>,
) -> FullParams<'a, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_translate(false);
    params.set_no_context(true);
    params.set_single_segment(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_timestamps(true);
    // Preserve whisper.cpp's quality-oriented fallback. It normally costs
    // nothing, but retries pathological high-compression decoder loops such
    // as the same short sentence repeated many times. Keep an environment
    // opt-out for isolated benchmarks only.
    params.set_temperature_inc(if env_flag("FLOV_WHISPER_TEMPERATURE_FALLBACK", true) {
        0.2
    } else {
        0.0
    });
    if let Some(audio_ctx) = audio_ctx {
        params.set_audio_ctx(audio_ctx);
    }
    params.set_language(Some(language));
    params
}

fn inference_threads() -> i32 {
    std::env::var("FLOV_WHISPER_THREADS")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| num_cpus::get() as i32)
}

fn requested_audio_context(sample_count: usize) -> Result<Option<i32>> {
    let Ok(value) = std::env::var("FLOV_WHISPER_AUDIO_CTX") else {
        return Ok(None);
    };
    if value.eq_ignore_ascii_case("auto") {
        // One encoder token covers 20 ms (320 samples). Leave 128 tokens of
        // right context, align for the padded Flash-Attention path, and never
        // exceed large-v3's native 1500-token/30-second context.
        return Ok(Some(auto_audio_context(sample_count)));
    }
    let parsed = value
        .parse::<i32>()
        .with_context(|| format!("invalid FLOV_WHISPER_AUDIO_CTX={value:?}"))?;
    if !(1..=1500).contains(&parsed) {
        bail!("FLOV_WHISPER_AUDIO_CTX must be 1..=1500 or 'auto'");
    }
    Ok(Some(parsed))
}

fn auto_audio_context(sample_count: usize) -> i32 {
    let needed = sample_count.div_ceil(320).saturating_add(128);
    needed.div_ceil(128).saturating_mul(128).clamp(256, 1500) as i32
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) if matches!(value.as_str(), "0" | "false" | "FALSE" | "no" | "NO") => false,
        Ok(value) if matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES") => true,
        Ok(_) | Err(_) => default,
    }
}

fn log_phase(phase: &str, elapsed: Duration) {
    eprintln!(
        "flov-whisper-cuda timing phase={phase} elapsed_ms={:.3}",
        elapsed.as_secs_f64() * 1_000.0
    );
}

#[cfg(test)]
mod tests {
    use super::auto_audio_context;

    #[test]
    fn automatic_audio_context_scales_and_clamps() {
        assert_eq!(auto_audio_context(0), 256);
        assert_eq!(auto_audio_context(16_000 * 3), 384);
        assert_eq!(auto_audio_context(16_000 * 30), 1500);
        assert_eq!(auto_audio_context(usize::MAX), 1500);
    }
}
