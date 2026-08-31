// flov-whisper-vulkan — Vulkan transcription sidecar (cross-vendor GPU).
// Same protocol as the other sidecars — see crates/flov-whisper-cpu/src/main.rs.
// whisper.cpp here is compiled with the Vulkan backend, which works on AMD,
// Intel iGPU, and NVIDIA (slower than CUDA on NVIDIA but a safe fallback).

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use flov_tuning::{apply_decoding_tuning, quality_mode, vad_filter_samples};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const LONG_FORM_THRESHOLD_SAMPLES: usize = 16_000 * 30;

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
        let _ = writeln!(std::io::stderr(), "flov-whisper-vulkan error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;

    let ctx = WhisperContext::new_with_params(
        args.model.to_str().context("invalid model path")?,
        WhisperContextParameters::default(),
    )
    .context("failed to load whisper model")?;

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

    let threads = num_cpus::get() as i32;
    let samples = vad_filter_samples(samples, threads)?;
    // An input whose speech VAD rejected entirely has nothing to decode, and
    // decoding it anyway is exactly how hallucinated text appears.
    if samples.is_empty() {
        eprintln!("flov-whisper-vulkan vad_result=no_speech");
        return Ok(());
    }

    let mut state = ctx.create_state().context("failed to create state")?;
    // Whisper decodes audio in 30-second windows. The PTT-tuned fast path
    // (single_segment + no timestamps + greedy) drops words at every window
    // boundary on longer inputs, so audio over one window switches to the
    // reference long-form decode: timestamp tokens drive the window seek and
    // beam search recovers boundary words that greedy decoding loses.
    // Quality mode spends the beam-search decode on short clips too;
    // the hotkey path leaves it off to keep release-to-text latency.
    let long_form = samples.len() > LONG_FORM_THRESHOLD_SAMPLES || quality_mode();
    let mut params = if long_form {
        FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        })
    } else {
        FullParams::new(SamplingStrategy::Greedy { best_of: 1 })
    };
    params.set_n_threads(threads);
    params.set_translate(false);
    params.set_no_context(!long_form);
    params.set_single_segment(!long_form);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_language(Some(&args.language));

    apply_decoding_tuning(&mut params);

    state
        .full(params, &samples)
        .context("transcription failed")?;

    let n = state.full_n_segments();
    let mut text = String::new();
    for i in 0..n {
        if let Some(segment) = state.get_segment(i) {
            if let Ok(s) = segment.to_str_lossy() {
                text.push_str(&s);
            }
        }
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.trim().as_bytes())
        .context("failed to write stdout")?;
    out.flush().ok();
    Ok(())
}
