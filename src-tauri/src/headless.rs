//! Headless transcription service mode (`flov --headless-server`).
//!
//! Serves the same OpenAI-compatible HTTP API as the desktop app but skips
//! every Tauri/GTK/Wayland, tray, hotkey, and microphone step, so it runs
//! without a display server (systemd user service, SSH session, container).
//!
//! The flag is handled in `main.rs` before `flov_lib::run()` precisely
//! because `tauri::Builder` would initialize GTK and panic with
//! "Failed to initialize GTK" when `DISPLAY`/`WAYLAND_DISPLAY` are unset.
//! Nothing in this module touches Tauri, GTK, cpal, or evdev.

use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::{api, config, hotkey, postprocess, stats, transcribe};

pub const FLAG: &str = "--headless-server";

/// Whether the command line explicitly requests the headless service.
/// Pure function so CLI parsing stays regression-tested.
pub fn is_requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == FLAG)
}

/// Handle `--headless-server` when present. Mirrors
/// `control::run_if_requested`: `Ok(true)` means the flag was consumed and
/// `main` must return. Errors bubble up so `main` exits non-zero — an
/// explicitly requested headless service must never degrade into an
/// active-but-dead process.
pub fn run_if_requested() -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    if !is_requested(&args) {
        return Ok(false);
    }
    run()?;
    Ok(true)
}

fn run() -> Result<()> {
    crate::init_logging();
    tracing::info!("flov starting (headless server mode)");

    let cfg = config::Config::load().context("config load failed")?;
    if !cfg.server.enabled {
        anyhow::bail!(
            "[server].enabled is false but {FLAG} was requested; \
             set enabled = true in flov.toml or run the desktop app instead"
        );
    }

    // Same construction order as the desktop `run()`: shared Config,
    // Transcriber, optional PostProcessor, Stats. Inference stays
    // serialized inside `Transcriber` exactly like on the desktop.
    let backend_choice = Arc::new(Mutex::new(cfg.backend.choice.clone()));
    let model_path = Arc::new(Mutex::new(cfg.whisper.model_path.clone()));
    let available_backends = transcribe::available_backends();
    tracing::info!(
        "available backends: {:?}; configured choice: {}",
        available_backends,
        cfg.backend.choice
    );
    let transcriber = Arc::new(
        transcribe::Transcriber::new(model_path, cfg.whisper.language.clone(), backend_choice)
            .context("whisper init failed")?,
    );

    let initial_pp = if cfg.openrouter.api_key.is_empty() {
        None
    } else {
        Some(Arc::new(postprocess::PostProcessor::new(
            cfg.openrouter.api_key.clone(),
            cfg.openrouter.model.clone(),
            cfg.openrouter.system_prompt.clone(),
        )))
    };
    let post_processor = Arc::new(Mutex::new(initial_pp));
    let stats = Arc::new(stats::Stats::open().context("stats open failed")?);

    let bound = api::spawn(
        cfg.server.clone(),
        api::ApiRuntime {
            app: None,
            transcriber,
            post_processor,
            stats,
            active_mode: Arc::new(AtomicU8::new(hotkey::MODE_IDLE)),
            recording_supported: false,
        },
    )
    .context("HTTP API did not start")?
    .context("HTTP API is disabled in the config")?;

    tracing::info!("headless transcription service ready on http://{bound}");

    // Stay in the foreground for systemd (Type=simple). `systemctl stop`
    // delivers SIGTERM, whose default disposition terminates the process.
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn headless_flag_is_recognized() {
        assert!(is_requested(&args(&[FLAG])));
        assert!(is_requested(&args(&["flov", FLAG])));
        assert!(is_requested(&args(&[FLAG, "--extra"])));
    }

    #[test]
    fn unrelated_args_do_not_trigger_headless() {
        assert!(!is_requested(&args(&[])));
        assert!(!is_requested(&args(&["--record-start"])));
        assert!(!is_requested(&args(&["--record-stop"])));
        assert!(!is_requested(&args(&["--server-health"])));
        assert!(!is_requested(&args(&["--headless"])));
        assert!(!is_requested(&args(&["--server"])));
    }
}
