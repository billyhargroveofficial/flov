// Tauri commands for the Models window.
//
// Download runs on a worker thread so the command returns immediately; the
// frontend listens for `model-progress` events to update the bar.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::models::{self, ModelInfo};

#[derive(Clone)]
pub struct ModelState {
    /// Currently active model (read by Transcriber on every transcribe).
    pub model_path: Arc<Mutex<PathBuf>>,
    /// Active downloads (id → cancel flag — Phase 2).
    pub in_flight: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Serialize)]
struct ProgressEvent {
    id: String,
    downloaded: u64,
    total: u64,
    done: bool,
    error: Option<String>,
}

#[tauri::command]
pub fn list_models(state: State<ModelState>) -> Vec<ModelInfo> {
    let active = state.model_path.lock().unwrap().clone();
    models::list(Some(&active))
}

#[tauri::command]
pub fn delete_model(id: String) -> Result<(), String> {
    models::delete_file(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_active_model(id: String, state: State<ModelState>) -> Result<(), String> {
    let path = models::entry_local_path(&id).map_err(|e| e.to_string())?;
    if !path.exists() {
        return Err(format!("model '{}' is not downloaded", id));
    }
    {
        let mut guard = state.model_path.lock().unwrap();
        *guard = path.clone();
    }
    crate::config::Config::write_model_path(&path).map_err(|e| e.to_string())?;
    tracing::info!("active model set to {} ({:?})", id, path);
    Ok(())
}

/// Kick off a download on a worker thread. Returns immediately; progress
/// flows over `model-progress` events keyed by `id`.
#[tauri::command]
pub fn download_model(id: String, state: State<ModelState>, app: AppHandle) -> Result<(), String> {
    let url = models::entry_url(&id).map_err(|e| e.to_string())?;
    let dest = models::entry_local_path(&id).map_err(|e| e.to_string())?;
    let expected_size = models::entry_size(&id).map_err(|e| e.to_string())?;

    {
        let mut guard = state.in_flight.lock().unwrap();
        if guard.iter().any(|x| x == &id) {
            return Err(format!("download already in progress: {}", id));
        }
        guard.push(id.clone());
    }
    let in_flight = state.in_flight.clone();
    let model_path_state = state.model_path.clone();

    std::thread::spawn(move || {
        let result = run_download(&id, &url, &dest, expected_size, &app);
        if let Err(e) = &result {
            tracing::error!("download {} failed: {}", id, e);
            emit_model_progress(
                &app,
                ProgressEvent {
                    id: id.clone(),
                    downloaded: 0,
                    total: expected_size,
                    done: true,
                    error: Some(e.to_string()),
                },
            );
        } else {
            tracing::info!("download {} complete", id);

            // If this is the only model on disk, auto-activate it so the
            // user can immediately start dictating without going back to
            // the picker. Skip if they already have a different active
            // model — they presumably know what they want.
            let downloaded_now: Vec<_> = models::list(None)
                .into_iter()
                .filter(|m| m.downloaded)
                .collect();
            let already_active_path = model_path_state.lock().unwrap().clone();
            let already_active_exists = already_active_path.exists();
            if downloaded_now.len() == 1 || !already_active_exists {
                if let Ok(path) = models::entry_local_path(&id) {
                    if path.exists() {
                        *model_path_state.lock().unwrap() = path.clone();
                        if let Err(e) = crate::config::Config::write_model_path(&path) {
                            tracing::warn!("auto-activate write_model_path failed: {}", e);
                        } else {
                            tracing::info!("auto-activated newly downloaded model {}", id);
                        }
                    }
                }
            }

            emit_model_progress(
                &app,
                ProgressEvent {
                    id: id.clone(),
                    downloaded: expected_size,
                    total: expected_size,
                    done: true,
                    error: None,
                },
            );
        }
        let mut guard = in_flight.lock().unwrap();
        guard.retain(|x| x != &id);
    });

    Ok(())
}

fn run_download(
    id: &str,
    url: &str,
    dest: &std::path::Path,
    expected_size: u64,
    app: &AppHandle,
) -> anyhow::Result<()> {
    use anyhow::Context;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {:?}", parent))?;
    }
    let tmp = dest.with_extension("part");

    tracing::info!("download {} -> {:?}", url, tmp);
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {}", url))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(expected_size);

    let mut reader = resp.into_body().into_reader();
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("create {:?}", tmp))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_emit = std::time::Instant::now();

    loop {
        let n = reader.read(&mut buf).context("read response")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("write file")?;
        downloaded += n as u64;
        // Throttle progress events to ~10/s; UI doesn't need finer.
        if last_emit.elapsed() > std::time::Duration::from_millis(100) {
            emit_model_progress(
                app,
                ProgressEvent {
                    id: id.to_string(),
                    downloaded,
                    total,
                    done: false,
                    error: None,
                },
            );
            last_emit = std::time::Instant::now();
        }
    }

    file.sync_all().context("fsync")?;
    drop(file);
    std::fs::rename(&tmp, dest).with_context(|| format!("rename {:?} -> {:?}", tmp, dest))?;
    Ok(())
}

fn emit_model_progress(app: &AppHandle, event: ProgressEvent) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.emit("model-progress", event);
    }
}

#[tauri::command]
pub fn show_models_window(app: AppHandle) -> Result<(), String> {
    crate::ui::open_settings_window(&app).map_err(|e| e.to_string())
}
