// Transcription via a swappable sidecar binary.
//
// The previous in-process whisper-rs link is replaced by a Command::spawn
// of `flov-whisper-<backend>.exe` that lives next to flov.exe. The protocol
// is documented in crates/flov-whisper-cpu/src/main.rs — args carry model
// + language, stdin carries raw f32 LE PCM, stdout returns text.
//
// Each transcription resolves the active sidecar fresh, so the tray menu
// can switch backends at runtime without recreating the Transcriber.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Backends tried, in order, when the user picked "auto" (or did not pick).
/// First one whose binary exists next to flov.exe wins.
pub const BACKEND_PRIORITY: &[&str] = &["cuda", "vulkan", "metal", "cpu"];

fn backend_bin_name(backend: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("flov-whisper-{}.exe", backend)
    } else {
        format!("flov-whisper-{}", backend)
    }
}

fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe failed")?;
    Ok(exe
        .parent()
        .context("current_exe has no parent")?
        .to_path_buf())
}

/// Names of backends whose sidecar binary exists next to flov.exe right now.
/// Used by the tray menu to grey-out unavailable choices.
///
/// CUDA also needs the NVIDIA driver runtime — `nvcuda.dll` ships with the
/// NVIDIA display driver and lives in `System32`. Without it the sidecar
/// would crash on startup, so we hide CUDA on machines where it's missing
/// (Intel-only laptops, AMD GPUs with no NVIDIA hardware, etc.) instead
/// of letting the user pick a backend that can't possibly work.
pub fn available_backends() -> Vec<String> {
    let dir = match exe_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    BACKEND_PRIORITY
        .iter()
        .filter(|b| dir.join(backend_bin_name(b)).exists())
        .filter(|b| **b != "cuda" || cuda_runtime_present())
        .map(|b| (*b).to_string())
        .collect()
}

#[cfg(target_os = "windows")]
fn cuda_runtime_present() -> bool {
    let sys = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    sys.join(r"System32\nvcuda.dll").exists()
}

#[cfg(not(target_os = "windows"))]
fn cuda_runtime_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        // libcuda belongs to the display driver, not the CUDA toolkit. The
        // character device is a cheap and reliable signal that the NVIDIA
        // driver is loaded in the current Linux session.
        std::path::Path::new("/dev/nvidiactl").exists()
            || std::path::Path::new("/proc/driver/nvidia/version").exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub struct Transcriber {
    model_path: Arc<Mutex<PathBuf>>,
    language: String,
    /// Shared with the tray menu; updated when the user picks a backend.
    backend_choice: Arc<Mutex<String>>,
    /// GPU sidecars can reserve most of the device while loading a model.
    /// Serialize local hotkey and HTTP requests so simultaneous calls cannot
    /// race into OOM or contend on the same accelerator.
    inference_lock: Mutex<()>,
}

impl Transcriber {
    pub fn new(
        model_path: Arc<Mutex<PathBuf>>,
        language: String,
        backend_choice: Arc<Mutex<String>>,
    ) -> Result<Self> {
        // Existence is checked per-transcribe so the user can launch the app
        // before downloading a model and pick one from the Models window.
        let preview = model_path.lock().unwrap().clone();
        tracing::info!("Whisper model: {:?}", preview);
        Ok(Self {
            model_path,
            language,
            backend_choice,
            inference_lock: Mutex::new(()),
        })
    }

    /// Lightweight pre-flight: does the configured model file exist on
    /// disk right now? Used before starting a recording so we can show
    /// "no model" before the user wastes a sentence into the void.
    pub fn has_model(&self) -> bool {
        self.model_path.lock().unwrap().exists()
    }

    pub fn default_language(&self) -> &str {
        &self.language
    }

    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        self.transcribe_with_language(samples, None)
    }

    /// Transcribe with an optional per-request ISO language code. The desktop
    /// hotkey uses the configured default; the HTTP API mirrors OpenAI's
    /// `language` form field through this override.
    pub fn transcribe_with_language(
        &self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<String> {
        let _inference_guard = self.inference_lock.lock().unwrap();
        let total_start = Instant::now();
        let choice = self.backend_choice.lock().unwrap().clone();
        let (backend, sidecar) = resolve_sidecar(&choice)?;
        let model_path = self.model_path.lock().unwrap().clone();
        if !model_path.exists() {
            anyhow::bail!(
                "model file not found: {:?} — open Models from the tray to download one",
                model_path
            );
        }
        tracing::info!(
            "transcribe via {} | model={:?} | sidecar={:?} | language={}",
            backend,
            model_path,
            sidecar,
            language.unwrap_or(&self.language)
        );

        let mut cmd = Command::new(&sidecar);
        cmd.arg("--model")
            .arg(&model_path)
            .arg("--language")
            .arg(language.unwrap_or(&self.language))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn sidecar: {:?}", sidecar))?;

        let mut stdout = child.stdout.take().context("sidecar stdout missing")?;
        let stdout_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            stdout.read_to_string(&mut buf).map(|_| buf)
        });

        let mut stderr = child.stderr.take().context("sidecar stderr missing")?;
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        });

        let timeout = transcription_timeout(samples.len());
        let mut stdin = child.stdin.take().context("sidecar stdin missing")?;
        let write_start = Instant::now();
        let (status, write_result, timed_out) = std::thread::scope(|scope| -> Result<_> {
            let stdin_thread = scope.spawn(move || {
                let result = write_samples_to_stdin(&mut stdin, samples);
                // Dropping stdin closes the pipe → sidecar's read_to_end returns.
                drop(stdin);
                result
            });

            let mut timed_out = false;
            let status = loop {
                if let Some(status) = child.try_wait().context("sidecar wait failed")? {
                    break status;
                }
                if total_start.elapsed() > timeout {
                    timed_out = true;
                    tracing::error!(
                        "sidecar timed out after {:?}; killing process",
                        total_start.elapsed()
                    );
                    let _ = child.kill();
                    break child.wait().context("sidecar wait after kill failed")?;
                }
                std::thread::sleep(Duration::from_millis(50));
            };

            let write_result = stdin_thread
                .join()
                .map_err(|_| anyhow::anyhow!("sidecar stdin writer panicked"))?;
            Ok((status, write_result, timed_out))
        })?;
        tracing::debug!(
            "sidecar stdin writer finished after {:?} for {} samples",
            write_start.elapsed(),
            samples.len()
        );

        if timed_out {
            let stdout_text = stdout_thread
                .join()
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or_default();
            let stderr_text = stderr_thread.join().unwrap_or_default();
            anyhow::bail!(
                "sidecar timed out after {:?}; stdout: {}; stderr: {}",
                timeout,
                stdout_text.trim(),
                stderr_text.trim()
            );
        }

        let stdout_buf = stdout_thread
            .join()
            .map_err(|_| anyhow::anyhow!("sidecar stdout reader panicked"))?
            .context("failed to read sidecar stdout")?;
        let stderr_text = stderr_thread.join().unwrap_or_default();

        if let Err(e) = write_result {
            anyhow::bail!(
                "failed to write samples to sidecar: {:#}; stderr: {}",
                e,
                stderr_text.trim()
            );
        }
        if !status.success() {
            anyhow::bail!(
                "sidecar exited with {:?}; stderr: {}",
                status.code(),
                stderr_text.trim()
            );
        }
        if !stderr_text.trim().is_empty() {
            tracing::debug!("sidecar stderr: {}", stderr_text.trim());
        }
        tracing::info!("transcription took {:?}", total_start.elapsed());
        Ok(stdout_buf.trim().to_string())
    }
}

fn write_samples_to_stdin<W: Write>(stdin: &mut W, samples: &[f32]) -> Result<()> {
    const CHUNK_SAMPLES: usize = 4096;

    let mut buf = Vec::with_capacity(CHUNK_SAMPLES * 4);
    for chunk in samples.chunks(CHUNK_SAMPLES) {
        buf.clear();
        for sample in chunk {
            buf.extend_from_slice(&sample.to_le_bytes());
        }
        stdin
            .write_all(&buf)
            .context("failed to write samples to sidecar")?;
    }
    Ok(())
}

fn transcription_timeout(sample_count: usize) -> Duration {
    let audio_secs = sample_count as f64 / crate::audio::TRANSCRIBE_SAMPLE_RATE as f64;
    let scaled = Duration::from_secs_f64((audio_secs * 12.0).max(30.0));
    scaled.min(Duration::from_secs(10 * 60))
}

/// Picks the sidecar to spawn given a user choice.
/// - "auto" → first available in BACKEND_PRIORITY
/// - "cuda" / "vulkan" / "metal" / "cpu" → that one specifically (errors if
///   missing — the menu greys out missing ones, so this only fires if the
///   binary was deleted between startup and the click).
/// - FLOV_BACKEND env var, when set, overrides the choice — useful for
///   one-off comparisons without touching the menu.
fn resolve_sidecar(choice: &str) -> Result<(String, PathBuf)> {
    let dir = exe_dir()?;
    let effective = std::env::var("FLOV_BACKEND").unwrap_or_else(|_| choice.to_string());

    if effective != "auto" {
        let candidate = dir.join(backend_bin_name(&effective));
        if !candidate.exists() {
            anyhow::bail!(
                "backend '{}' selected but {:?} not found",
                effective,
                candidate
            );
        }
        if effective == "cuda" && !cuda_runtime_present() {
            anyhow::bail!("CUDA backend selected but no active NVIDIA driver was detected");
        }
        return Ok((effective, candidate));
    }

    let mut tried = Vec::new();
    for backend in BACKEND_PRIORITY {
        let candidate = dir.join(backend_bin_name(backend));
        if candidate.exists() && (*backend != "cuda" || cuda_runtime_present()) {
            return Ok(((*backend).to_string(), candidate));
        }
        tried.push(candidate);
    }
    anyhow::bail!("no whisper sidecar found in {:?}; tried {:?}", dir, tried);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_samples_to_stdin_serializes_little_endian_f32() {
        let mut out = Vec::new();

        write_samples_to_stdin(&mut out, &[1.0, -2.5]).unwrap();

        let expected = [1.0f32.to_le_bytes(), (-2.5f32).to_le_bytes()].concat();
        assert_eq!(out, expected);
    }

    #[test]
    fn transcription_timeout_has_minimum_for_short_audio() {
        assert_eq!(transcription_timeout(0), Duration::from_secs(30));
        assert_eq!(
            transcription_timeout(crate::audio::TRANSCRIBE_SAMPLE_RATE as usize),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn transcription_timeout_scales_with_audio_length() {
        let samples = crate::audio::TRANSCRIBE_SAMPLE_RATE as usize * 5;

        assert_eq!(transcription_timeout(samples), Duration::from_secs(60));
    }

    #[test]
    fn transcription_timeout_is_capped() {
        let samples = crate::audio::TRANSCRIBE_SAMPLE_RATE as usize * 120;

        assert_eq!(transcription_timeout(samples), Duration::from_secs(10 * 60));
    }
}
