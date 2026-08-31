//! Decoding tuning shared by every `flov-whisper-*` sidecar.
//!
//! All knobs are environment-driven and default to plain whisper.cpp
//! behaviour, so a sidecar that links this crate transcribes exactly as before
//! until an operator opts in. The hotkey path stays latency-tuned; the
//! headless HTTP service turns the quality knobs on.

use anyhow::{anyhow, Result};
use std::time::Instant;
use whisper_rs::{
    FullParams, WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
};

pub const WHISPER_SAMPLE_RATE: usize = 16_000;

/// Whether the caller should force the beam-search long-form decode.
///
/// The sub-window fast path decodes greedily for hotkey latency. On the
/// server that trade is usually not worth it — but measure per corpus: on
/// short, noisy clips beam search can hallucinate where greedy does not.
pub fn quality_mode() -> bool {
    env_flag("FLOV_WHISPER_QUALITY", false)
}

/// Applies the opt-in decoder knobs to an already-built parameter set.
pub fn apply_decoding_tuning(params: &mut FullParams<'_, '_>) {
    // Whisper conditions on this text before decoding, which is how domain
    // names survive ("OpenClaw", not "опенклау"). It is the model's own
    // prompt, not a rewrite pass over the finished transcript.
    if let Some(prompt) = env_text("FLOV_WHISPER_PROMPT") {
        params.set_initial_prompt(&prompt);
    }
    // Non-speech tokens are what turn a wordless clip into "[музыка]".
    if env_flag("FLOV_WHISPER_SUPPRESS_NST", false) {
        params.set_suppress_nst(true);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_NO_SPEECH_THOLD") {
        params.set_no_speech_thold(value);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_ENTROPY_THOLD") {
        params.set_entropy_thold(value);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_LOGPROB_THOLD") {
        params.set_logprob_thold(value);
    }
    // VAD is deliberately absent here: `whisper_full_params.vad` is only
    // honoured by `whisper_full()`, and the sidecars decode through
    // `whisper_full_with_state()`. See `vad_filter_samples`.
}

/// Keeps only the speech whisper should decode.
///
/// Returns the input untouched when `FLOV_WHISPER_VAD_MODEL` is unset, and an
/// empty buffer when Silero finds no speech at all — callers must treat that
/// as "no transcript" instead of decoding, because a wordless window is
/// exactly where whisper invents subtitle credits from its training data.
pub fn vad_filter_samples(samples: Vec<f32>, threads: i32) -> Result<Vec<f32>> {
    let Some(model_path) = env_text("FLOV_WHISPER_VAD_MODEL") else {
        return Ok(samples);
    };
    let started = Instant::now();

    let mut context_params = WhisperVadContextParams::new();
    context_params.set_n_threads(threads);
    context_params.set_use_gpu(env_flag("FLOV_WHISPER_VAD_GPU", false));
    let mut vad = WhisperVadContext::new(&model_path, context_params)
        .map_err(|err| anyhow!("failed to load VAD model {model_path}: {err}"))?;

    let overlap_seconds = env_f32("FLOV_WHISPER_VAD_SAMPLES_OVERLAP").unwrap_or(0.1);
    let mut vad_params = WhisperVadParams::new();
    vad_params.set_samples_overlap(overlap_seconds);
    if let Some(value) = env_f32("FLOV_WHISPER_VAD_THRESHOLD") {
        vad_params.set_threshold(value);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_VAD_SPEECH_PAD_MS") {
        vad_params.set_speech_pad(value as i32);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_VAD_MIN_SPEECH_MS") {
        vad_params.set_min_speech_duration(value as i32);
    }
    if let Some(value) = env_f32("FLOV_WHISPER_VAD_MIN_SILENCE_MS") {
        vad_params.set_min_silence_duration(value as i32);
    }

    let segments = vad
        .segments_from_samples(vad_params, &samples)
        .map_err(|err| anyhow!("VAD segmentation failed: {err}"))?;
    let count = segments.num_segments();

    // Mirrors whisper.cpp's own filtering in `whisper_full()`: every segment
    // but the last is extended by the overlap, and segments are joined with
    // 100 ms of silence so the decoder still hears sentence boundaries.
    let overlap_samples = (overlap_seconds * WHISPER_SAMPLE_RATE as f32) as usize;
    let silence = vec![0.0f32; WHISPER_SAMPLE_RATE / 10];
    let mut filtered: Vec<f32> = Vec::new();
    for index in 0..count {
        let Some(segment) = segments.get_segment(index) else {
            continue;
        };
        let start = cs_to_samples(segment.start).min(samples.len());
        let mut end = cs_to_samples(segment.end);
        if index < count - 1 {
            end += overlap_samples;
        }
        let end = end.min(samples.len());
        if end <= start {
            continue;
        }
        if !filtered.is_empty() {
            filtered.extend_from_slice(&silence);
        }
        filtered.extend_from_slice(&samples[start..end]);
    }

    eprintln!(
        "flov-whisper vad segments={count} input_ms={} kept_ms={} elapsed_ms={:.3}",
        ms_from_samples(samples.len()),
        ms_from_samples(filtered.len()),
        started.elapsed().as_secs_f64() * 1_000.0,
    );
    Ok(filtered)
}

/// VAD segment timestamps are centiseconds, matching `samples_to_cs` in whisper.cpp.
fn cs_to_samples(centiseconds: f32) -> usize {
    ((centiseconds / 100.0) * WHISPER_SAMPLE_RATE as f32)
        .round()
        .max(0.0) as usize
}

fn ms_from_samples(samples: usize) -> u64 {
    samples as u64 * 1_000 / WHISPER_SAMPLE_RATE as u64
}

pub fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) if matches!(value.as_str(), "0" | "false" | "FALSE" | "no" | "NO") => false,
        Ok(value) if matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES") => true,
        Ok(_) | Err(_) => default,
    }
}

pub fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn env_f32(name: &str) -> Option<f32> {
    env_text(name).and_then(|value| value.parse::<f32>().ok())
}

#[cfg(test)]
mod tests {
    use super::{cs_to_samples, ms_from_samples, WHISPER_SAMPLE_RATE};

    #[test]
    fn centiseconds_convert_to_sample_offsets() {
        assert_eq!(cs_to_samples(0.0), 0);
        assert_eq!(cs_to_samples(100.0), WHISPER_SAMPLE_RATE);
        assert_eq!(cs_to_samples(-5.0), 0);
    }

    #[test]
    fn sample_counts_convert_to_milliseconds() {
        assert_eq!(ms_from_samples(WHISPER_SAMPLE_RATE), 1_000);
        assert_eq!(ms_from_samples(0), 0);
    }
}
