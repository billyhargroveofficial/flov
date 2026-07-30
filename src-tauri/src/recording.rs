use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tauri::{Emitter, Manager};

use crate::{audio, hotkey, input, postprocess, stats, transcribe, tray, ui};

pub struct RecordingRuntime {
    pub app: tauri::AppHandle,
    /// `None` keeps the HTTP transcription service usable on a machine
    /// without a local capture device.
    pub recorder: Option<Arc<audio::AudioRecorder>>,
    pub transcriber: Arc<transcribe::Transcriber>,
    pub active_mode: Arc<AtomicU8>,
    pub is_recording: Arc<AtomicBool>,
    pub post_processor: Arc<Mutex<Option<Arc<postprocess::PostProcessor>>>>,
    pub postprocess_enabled: Arc<AtomicBool>,
    pub stats: Arc<stats::Stats>,
    pub sample_rate: u32,
}

struct RecordingCycleGuard;

impl RecordingCycleGuard {
    fn start() -> Self {
        ui::set_recording_cycle_active(true);
        Self
    }
}

impl Drop for RecordingCycleGuard {
    fn drop(&mut self) {
        ui::set_recording_cycle_active(false);
    }
}

pub fn spawn_recording_loop(runtime: RecordingRuntime) {
    std::thread::Builder::new()
        .name("flov-recording-loop".into())
        .spawn(move || recording_loop(runtime))
        .expect("spawn recording loop");
}

pub fn spawn_state_watchdog(is_recording: Arc<AtomicBool>, active_mode: Arc<AtomicU8>) {
    std::thread::Builder::new()
        .name("flov-state-watchdog".into())
        .spawn(move || {
            let mut stuck_ticks = 0u8;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let recording = is_recording.load(Ordering::SeqCst);
                let mode = active_mode.load(Ordering::SeqCst);
                if recording && mode == hotkey::MODE_IDLE {
                    stuck_ticks = stuck_ticks.saturating_add(1);
                    if stuck_ticks >= 3 {
                        tracing::warn!(
                            "watchdog: is_recording stuck true with mode=IDLE for ~6s, resetting"
                        );
                        is_recording.store(false, Ordering::SeqCst);
                        stuck_ticks = 0;
                    }
                } else {
                    stuck_ticks = 0;
                }
            }
        })
        .expect("spawn watchdog thread");
}

pub fn spawn_webview_reloader(
    app: tauri::AppHandle,
    is_recording: Arc<AtomicBool>,
    active_mode: Arc<AtomicU8>,
) {
    std::thread::Builder::new()
        .name("flov-webview-reloader".into())
        .spawn(move || {
            const RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);
            loop {
                std::thread::sleep(RELOAD_INTERVAL);
                if is_recording.load(Ordering::SeqCst) {
                    tracing::info!("webview reload skipped: recording in progress");
                    continue;
                }
                if active_mode.load(Ordering::SeqCst) != hotkey::MODE_IDLE {
                    tracing::info!("webview reload skipped: hotkey mode active");
                    continue;
                }
                if ui::frontend_reload_in_progress() {
                    tracing::info!("webview reload skipped: previous reload still pending");
                    continue;
                }
                if ui::overlay_active() {
                    tracing::info!("webview reload skipped: pill logically active");
                    continue;
                }
                if !ui::overlay_quiet_for(std::time::Duration::from_secs(5)) {
                    tracing::info!("webview reload skipped: pill was active recently");
                    continue;
                }
                let Some(window) = app.get_webview_window("main") else {
                    tracing::warn!("webview reload: main window missing");
                    continue;
                };
                let previous_epoch = ui::mark_frontend_reload_started();
                #[cfg(target_os = "windows")]
                ui::set_window_alpha(&window, 0);
                match window.eval("location.reload()") {
                    Ok(_) => {
                        tracing::info!("webview reload triggered");
                        if ui::wait_for_frontend_ready_after(
                            previous_epoch,
                            std::time::Duration::from_secs(10),
                        ) {
                            tracing::info!("webview reload completed");
                        } else {
                            tracing::warn!(
                                "webview reload did not report ready within 10s; will wait on next show"
                            );
                        }
                    }
                    Err(e) => {
                        ui::mark_frontend_reload_finished();
                        tracing::warn!("webview reload failed: {}", e);
                    }
                }
            }
        })
        .expect("spawn webview reloader thread");
}

fn recording_loop(runtime: RecordingRuntime) {
    let RecordingRuntime {
        app,
        recorder,
        transcriber,
        active_mode,
        is_recording,
        post_processor,
        postprocess_enabled,
        stats,
        sample_rate,
    } = runtime;

    loop {
        while active_mode.load(Ordering::SeqCst) == hotkey::MODE_IDLE {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        is_recording.store(true, Ordering::SeqCst);
        let _cycle_guard = RecordingCycleGuard::start();
        let cycle_start = Instant::now();
        tracing::info!("recording start");

        let model_present = transcriber.has_model();
        if !model_present {
            show_pill_window(&app, true);
            emit_transcribe_error(&app, "Скачай модель: Settings → Models");
            tracing::warn!("hotkey pressed but no model is configured");
            is_recording.store(false, Ordering::SeqCst);
            wait_for_hotkey_release(&active_mode);
            continue;
        }

        let Some(recorder) = recorder.as_ref() else {
            show_pill_window(&app, true);
            emit_transcribe_error(&app, "Микрофон недоступен — проверь PipeWire/ALSA");
            tracing::warn!("hotkey pressed but no local audio input is available");
            is_recording.store(false, Ordering::SeqCst);
            wait_for_hotkey_release(&active_mode);
            continue;
        };

        show_pill_window(&app, true);
        emit_state(&app, ui::PillState::Recording);
        tray::set_state(&app, tray::TrayState::Recording);

        let active_for_record = active_mode.clone();
        let app_for_spec = app.clone();
        let record_start = Instant::now();
        let samples = match recorder.record_while_with_spectrum(
            move || active_for_record.load(Ordering::SeqCst) != hotkey::MODE_IDLE,
            move |spec| {
                let _ = app_for_spec.emit("audio-spectrum", spec);
            },
        ) {
            Ok(samples) => samples,
            Err(e) => {
                tracing::error!("audio recording failed: {:#}", e);
                emit_transcribe_error(&app, &format!("Audio error: {e:#}"));
                tray::set_state(&app, tray::TrayState::Idle);
                is_recording.store(false, Ordering::SeqCst);
                wait_for_hotkey_release(&active_mode);
                continue;
            }
        };

        is_recording.store(false, Ordering::SeqCst);
        tracing::info!(
            "recording stop in {:?}, samples={}",
            record_start.elapsed(),
            samples.len()
        );

        if samples.len() < 1600 {
            emit_state(&app, ui::PillState::Idle);
            tray::set_state(&app, tray::TrayState::Idle);
            continue;
        }

        emit_state(&app, ui::PillState::Transcribing);
        tray::set_state(&app, tray::TrayState::Transcribing);

        let transcribe_start = Instant::now();
        let raw_text = match transcriber.transcribe(&samples) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("transcribe failed: {}", e);
                let msg = if e.to_string().contains("model file not found") {
                    "Скачай модель: Settings → Models".to_string()
                } else {
                    format!("Transcribe error: {}", e)
                };
                emit_transcribe_error(&app, &msg);
                tray::set_state(&app, tray::TrayState::Idle);
                continue;
            }
        };
        tracing::info!(
            "recording loop: transcribe returned in {:?}, chars={}",
            transcribe_start.elapsed(),
            raw_text.chars().count()
        );
        if raw_text.is_empty() {
            emit_state(&app, ui::PillState::Idle);
            tray::set_state(&app, tray::TrayState::Idle);
            continue;
        }
        tracing::info!("transcript: {}", raw_text);

        let chars = raw_text.chars().count() as u64;
        let seconds = samples.len() as f64 / sample_rate as f64;
        stats.record(chars, seconds);
        let _ = app.emit("stats-updated", ());

        let pp_snapshot: Option<Arc<postprocess::PostProcessor>> =
            post_processor.lock().unwrap().clone();

        let final_text = if postprocess_enabled.load(Ordering::SeqCst) {
            if let Some(p) = pp_snapshot.as_ref() {
                match p.process(&raw_text) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(
                            "postprocess failed, falling back to raw transcript: {:#}",
                            e
                        );
                        raw_text
                    }
                }
            } else {
                tracing::warn!("postprocess enabled but no API key configured");
                raw_text
            }
        } else {
            raw_text
        };

        input::type_text(&final_text);
        emit_state(&app, ui::PillState::Idle);
        tray::set_state(&app, tray::TrayState::Idle);
        tracing::info!(
            "recording cycle complete in {:?}, final_chars={}",
            cycle_start.elapsed(),
            final_text.chars().count()
        );
    }
}

fn emit_state(app: &tauri::AppHandle, state: ui::PillState) {
    ui::set_pill_state(state);
    let _ = app.emit("state-changed", state);
}

fn emit_transcribe_error(app: &tauri::AppHandle, message: &str) {
    ui::set_pill_error(message);
    let _ = app.emit("transcribe-error", message);
}

fn wait_for_hotkey_release(active_mode: &AtomicU8) {
    while active_mode.load(Ordering::SeqCst) != hotkey::MODE_IDLE {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn show_pill_window(app: &tauri::AppHandle, wait_for_reload: bool) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("pill window missing — webview may have crashed");
        return;
    };

    #[cfg(target_os = "windows")]
    ui::position_at_cursor_monitor(&window);
    #[cfg(target_os = "windows")]
    ui::set_window_alpha(&window, 0);
    if let Err(e) = window.show() {
        tracing::warn!("pill window.show() failed: {}", e);
    }
    #[cfg(target_os = "windows")]
    {
        ui::force_click_through(&window);
    }

    if wait_for_reload && ui::frontend_reload_in_progress() {
        tracing::info!("waiting for pill frontend reload before emitting recording state");
        if !ui::wait_for_frontend_reload_if_needed(std::time::Duration::from_millis(750)) {
            tracing::warn!("pill frontend reload still pending; emitting recording state anyway");
        }
    }

    #[cfg(target_os = "windows")]
    ui::force_repaint(&window);
}
