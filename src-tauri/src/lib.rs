// Domain modules from existing flov.
pub mod audio;
pub mod config;
pub mod hotkey;
pub mod input;
pub mod models;
pub mod models_cmd;
pub mod paths;
pub mod postprocess;
pub mod state_cmd;
pub mod stats;
pub mod transcribe;

mod recording;
mod tray;
mod ui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tauri::Manager;

#[cfg(target_os = "windows")]
fn configure_webview2_environment() {
    // WebView2's default background is white and can flash for one frame
    // before CSS/default-background API settings apply. Microsoft documents
    // WEBVIEW2_DEFAULT_BACKGROUND_COLOR as the earliest hook for this.
    std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "00000000");
}

#[cfg(not(target_os = "windows"))]
fn configure_webview2_environment() {}

/// Open `flov.log` next to the running exe (not CWD — CWD changes when
/// the app is launched from elsewhere) in append mode so logs survive
/// across restarts. Wrap the File in a Mutex so the writer is `Sync` and
/// every record actually flushes to the OS buffer (the previous
/// `with_writer(file)` setup was both truncating on every start AND
/// failing to write reliably). Defaults to INFO level when RUST_LOG is
/// unset.
fn init_logging() {
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use tracing_subscriber::EnvFilter;

    // Per-platform user data dir (see `paths.rs`): on Windows the
    // installer puts the exe under %LOCALAPPDATA% which is writable so
    // exe-relative would work too — but on macOS the .app bundle is
    // read-only after code-signing and we must write to
    // `~/Library/Application Support/com.flov.app/`. Falling back to
    // the binary's directory keeps behaviour sane if `HOME` is unset.
    let log_path: PathBuf = paths::log_path().unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("flov.log")))
            .unwrap_or_else(|| PathBuf::from("flov.log"))
    });

    let file = OpenOptions::new().create(true).append(true).open(&log_path);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    match file {
        Ok(f) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(Mutex::new(f))
                .with_ansi(false)
                .with_target(false)
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    configure_webview2_environment();
    init_logging();
    tracing::info!("flov starting (Tauri)");

    let cfg = config::Config::load().expect("config load failed");
    let audio_device = Arc::new(Mutex::new(cfg.audio.device.clone()));
    let audio_capture = Arc::new(audio::AudioCaptureManager::new(
        cfg.audio.sample_rate,
        audio_device.clone(),
    ));

    // Shared mutable backend + model — written by the tray/Models window,
    // read by the Transcriber on every transcribe() call so a switch takes
    // effect on the next press of Ctrl+Win, no restart required.
    let backend_choice = Arc::new(Mutex::new(cfg.backend.choice.clone()));
    let model_path = Arc::new(Mutex::new(cfg.whisper.model_path.clone()));
    let available_backends = transcribe::available_backends();
    tracing::info!(
        "available backends: {:?}; configured choice: {}",
        available_backends,
        cfg.backend.choice
    );

    let transcriber = Arc::new(
        transcribe::Transcriber::new(
            model_path.clone(),
            cfg.whisper.language.clone(),
            backend_choice.clone(),
        )
        .expect("whisper init failed"),
    );

    let model_state = models_cmd::ModelState {
        model_path: model_path.clone(),
        in_flight: Arc::new(Mutex::new(Vec::new())),
    };

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
    let pp_settings = Arc::new(Mutex::new(state_cmd::PostprocessSettings {
        api_key: cfg.openrouter.api_key.clone(),
        model: cfg.openrouter.model.clone(),
        system_prompt: cfg.openrouter.system_prompt.clone(),
    }));
    let postprocess_enabled = Arc::new(AtomicBool::new(false));

    let stats = Arc::new(stats::Stats::open().expect("stats open failed"));

    let hotkey_state = hotkey::HotkeyState::new();
    let active_mode = hotkey_state.active_mode.clone();
    let is_recording = hotkey_state.is_recording.clone();
    let _hook = hotkey::install_hook(hotkey_state).expect("hotkey hook failed");

    // Set the initial hotkey definition; Tauri command can swap it later.
    // Fallback combo is per-platform so Mac users don't get Ctrl+Cmd (=
    // collides with system shortcuts like Ctrl+Cmd+Q lock screen).
    #[cfg(target_os = "macos")]
    let fallback_combo = "Cmd+Alt";
    #[cfg(not(target_os = "macos"))]
    let fallback_combo = "Ctrl+Win";
    let initial_def = hotkey::HotkeyDef::parse(&cfg.hotkey.combo).unwrap_or_else(|e| {
        tracing::warn!(
            "invalid hotkey '{}': {}, falling back to {}",
            cfg.hotkey.combo,
            e,
            fallback_combo
        );
        hotkey::HotkeyDef::parse(fallback_combo).unwrap()
    });
    tracing::info!("hotkey: {}", initial_def.combo);
    hotkey::set_hotkey_def(initial_def);
    let hotkey_combo = Arc::new(Mutex::new(cfg.hotkey.combo.clone()));

    let app_state = state_cmd::AppState {
        backend_choice: backend_choice.clone(),
        available_backends: available_backends.clone(),
        postprocess_enabled: postprocess_enabled.clone(),
        post_processor: post_processor.clone(),
        pp_settings: pp_settings.clone(),
        hotkey_combo: hotkey_combo.clone(),
        audio_device: audio_device.clone(),
        stats: stats.clone(),
    };

    let stats_for_loop = stats.clone();
    let sample_rate_for_loop = audio_capture.output_sample_rate();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(model_state)
        .manage(app_state)
        .setup(move |app| {
            // LSUIElement=true в Info.plist скрывает Dock-иконку, но
            // когда Tauri показывает webview window (pill или Settings),
            // AppKit поднимает activation policy до .regular и иконка
            // вылезает в Dock на время сессии. Явно фиксируем .accessory
            // чтобы окна показывались без Dock-иконки.
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let app_handle = app.handle().clone();
            let window = app.get_webview_window("main").expect("main window missing");

            // Click-through is set both via Tauri (works on Windows) and as a
            // belt-and-suspenders Win32 fallback, mirroring the verified POC.
            let _ = window.set_ignore_cursor_events(true);
            #[cfg(target_os = "windows")]
            {
                ui::force_click_through(&window);
                ui::set_window_alpha(&window, 0);
                if let Err(e) = window.show() {
                    tracing::warn!("initial pill window.show() failed: {}", e);
                }
                ui::force_repaint(&window);
            }

            tray::setup(&app_handle)?;

            // Spawn the recording orchestration thread; it owns the recorder
            // loop and emits state/amplitude events to the webview.
            let audio = audio_capture.clone();
            let transcriber = transcriber.clone();
            let active_mode = active_mode.clone();
            let is_recording = is_recording.clone();
            let post_processor = post_processor.clone();
            let postprocess_enabled = postprocess_enabled.clone();
            let stats = stats_for_loop.clone();
            let app_for_thread = app_handle.clone();
            let sample_rate = sample_rate_for_loop;

            recording::spawn_state_watchdog(is_recording.clone(), active_mode.clone());
            recording::spawn_webview_reloader(
                app_handle.clone(),
                is_recording.clone(),
                active_mode.clone(),
            );
            recording::spawn_recording_loop(recording::RecordingRuntime {
                app: app_for_thread,
                audio,
                transcriber,
                active_mode,
                is_recording,
                post_processor,
                postprocess_enabled,
                stats,
                sample_rate,
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ui::hide_window,
            ui::repaint_window,
            ui::pill_frontend_ready,
            models_cmd::list_models,
            models_cmd::download_model,
            models_cmd::delete_model,
            models_cmd::set_active_model,
            models_cmd::show_models_window,
            state_cmd::get_backend_state,
            state_cmd::set_backend_choice,
            state_cmd::get_postprocess_state,
            state_cmd::set_postprocess_enabled,
            state_cmd::get_postprocess_config,
            state_cmd::set_postprocess_config,
            state_cmd::get_hotkey,
            state_cmd::set_hotkey,
            state_cmd::list_audio_inputs,
            state_cmd::set_audio_input,
            state_cmd::get_stats,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
