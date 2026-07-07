// Tauri commands that expose / mutate the app's runtime state to the
// Settings window. Keeps the Settings UI a thin client over Rust state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::postprocess::PostProcessor;
use crate::stats::{Stats, StatsFile};

#[derive(Clone)]
pub struct AppState {
    pub backend_choice: Arc<Mutex<String>>,
    pub available_backends: Vec<String>,
    pub postprocess_enabled: Arc<AtomicBool>,
    /// `None` when no API key — the Settings form fills this in.
    pub post_processor: Arc<Mutex<Option<Arc<PostProcessor>>>>,
    /// Latest known settings (mirrors flov.toml `[openrouter]`). UI reads
    /// these into its form; saving rebuilds the processor above.
    pub pp_settings: Arc<Mutex<PostprocessSettings>>,
    /// Current key combo string for the global hotkey (e.g. "Ctrl+Win").
    pub hotkey_combo: Arc<Mutex<String>>,
    /// Selected microphone (cpal device name). `None` → system default.
    /// Changing this takes effect on the next recording; AudioCaptureManager
    /// resolves the selected/default device for each hotkey cycle.
    pub audio_device: Arc<Mutex<Option<String>>>,
    pub stats: Arc<Stats>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PostprocessSettings {
    pub api_key: String,
    pub model: String,
    pub system_prompt: String,
}

#[derive(Serialize)]
pub struct BackendStateView {
    pub choice: String,
    pub available: Vec<String>,
}

#[derive(Serialize)]
pub struct PostprocessStateView {
    pub available: bool,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct PostprocessConfigView {
    pub available: bool,
    pub enabled: bool,
    pub api_key: String,
    pub model: String,
    pub system_prompt: String,
}

#[tauri::command]
pub fn get_backend_state(state: State<AppState>) -> BackendStateView {
    BackendStateView {
        choice: state.backend_choice.lock().unwrap().clone(),
        available: state.available_backends.clone(),
    }
}

#[tauri::command]
pub fn set_backend_choice(choice: String, state: State<AppState>) -> Result<(), String> {
    if choice != "auto" && !state.available_backends.iter().any(|b| b == &choice) {
        return Err(format!(
            "backend '{}' is not available on this machine",
            choice
        ));
    }
    {
        let mut guard = state.backend_choice.lock().unwrap();
        *guard = choice.clone();
    }
    crate::config::Config::write_backend_choice(&choice).map_err(|e| e.to_string())?;
    tracing::info!("backend selected: {}", choice);
    Ok(())
}

#[tauri::command]
pub fn get_postprocess_state(state: State<AppState>) -> PostprocessStateView {
    let available = state.post_processor.lock().unwrap().is_some();
    PostprocessStateView {
        available,
        enabled: state.postprocess_enabled.load(Ordering::SeqCst),
    }
}

#[tauri::command]
pub fn set_postprocess_enabled(enabled: bool, state: State<AppState>) -> Result<(), String> {
    let available = state.post_processor.lock().unwrap().is_some();
    if enabled && !available {
        return Err("Configure the OpenRouter API key first".into());
    }
    state.postprocess_enabled.store(enabled, Ordering::SeqCst);
    tracing::info!("postprocess toggled: {}", enabled);
    Ok(())
}

#[tauri::command]
pub fn get_postprocess_config(state: State<AppState>) -> PostprocessConfigView {
    let s = state.pp_settings.lock().unwrap().clone();
    let available = state.post_processor.lock().unwrap().is_some();
    PostprocessConfigView {
        available,
        enabled: state.postprocess_enabled.load(Ordering::SeqCst),
        api_key: s.api_key,
        model: s.model,
        system_prompt: s.system_prompt,
    }
}

#[tauri::command]
pub fn set_postprocess_config(
    api_key: String,
    model: String,
    system_prompt: String,
    state: State<AppState>,
) -> Result<(), String> {
    crate::config::Config::write_openrouter_field("api_key", &api_key)
        .map_err(|e| e.to_string())?;
    crate::config::Config::write_openrouter_field("model", &model).map_err(|e| e.to_string())?;
    crate::config::Config::write_openrouter_field("system_prompt", &system_prompt)
        .map_err(|e| e.to_string())?;

    let new_processor = if api_key.trim().is_empty() {
        None
    } else {
        Some(Arc::new(PostProcessor::new(
            api_key.clone(),
            model.clone(),
            system_prompt.clone(),
        )))
    };
    *state.post_processor.lock().unwrap() = new_processor;
    *state.pp_settings.lock().unwrap() = PostprocessSettings {
        api_key,
        model,
        system_prompt,
    };

    if state.post_processor.lock().unwrap().is_none() {
        state.postprocess_enabled.store(false, Ordering::SeqCst);
    }
    tracing::info!("postprocess config saved");
    Ok(())
}

#[derive(Serialize)]
pub struct HotkeyView {
    pub combo: String,
}

#[tauri::command]
pub fn get_hotkey(state: State<AppState>) -> HotkeyView {
    HotkeyView {
        combo: state.hotkey_combo.lock().unwrap().clone(),
    }
}

#[tauri::command]
pub fn set_hotkey(combo: String, state: State<AppState>) -> Result<(), String> {
    let def = crate::hotkey::HotkeyDef::parse(&combo)?;
    crate::config::Config::write_hotkey_combo(&combo).map_err(|e| e.to_string())?;
    crate::hotkey::set_hotkey_def(def);
    *state.hotkey_combo.lock().unwrap() = combo.clone();
    tracing::info!("hotkey changed to {}", combo);
    Ok(())
}

#[derive(Serialize)]
pub struct AudioInputsView {
    /// Currently-connected input device names from cpal/WASAPI.
    pub devices: Vec<String>,
    /// Saved choice; `None` means "use system default".
    pub selected: Option<String>,
}

#[tauri::command]
pub fn list_audio_inputs(state: State<AppState>) -> AudioInputsView {
    AudioInputsView {
        devices: crate::audio::list_input_devices(),
        selected: state.audio_device.lock().unwrap().clone(),
    }
}

#[tauri::command]
pub fn set_audio_input(device: Option<String>, state: State<AppState>) -> Result<(), String> {
    // Empty / null = revert to system default.
    let cleaned = device.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    });
    crate::config::Config::write_audio_device(cleaned.as_deref().unwrap_or(""))
        .map_err(|e| e.to_string())?;
    *state.audio_device.lock().unwrap() = cleaned.clone();
    tracing::info!(
        "audio device selected: {:?} (takes effect on next recording)",
        cleaned
    );
    Ok(())
}

#[tauri::command]
pub fn get_stats(state: State<AppState>) -> StatsFile {
    state.stats.snapshot()
}
