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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

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
    /// A CUDA sidecar started on PTT-down. It owns VRAM only while the
    /// hotkey is held, then gets consumed (or killed) on PTT-up.
    prepared: Mutex<Option<PreparedSidecar>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidecarSpec {
    backend: String,
    sidecar: PathBuf,
    model_path: PathBuf,
    language: String,
}

struct RunningSidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_thread: Option<std::thread::JoinHandle<std::io::Result<String>>>,
    stderr_thread: Option<std::thread::JoinHandle<String>>,
}

struct PreparedSidecar {
    spec: SidecarSpec,
    running: RunningSidecar,
    spawned_at: Instant,
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
            prepared: Mutex::new(None),
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

    /// Start the configured CUDA sidecar when PTT is pressed. The sidecar
    /// initializes its Whisper context before it starts consuming stdin, so
    /// this overlaps model loading with microphone capture. Nothing is kept
    /// alive in idle: recording.rs always consumes or discards this child on
    /// the matching PTT release.
    pub fn prepare(&self) -> Result<()> {
        // This fast path is deliberately Linux-only until it has been tuned
        // and regression-tested independently on the other platforms.
        if !cfg!(target_os = "linux") {
            return Ok(());
        }

        // Do not load a second GPU context while an HTTP/explicit request is
        // already using the serialized inference path. A missed prewarm only
        // falls back to the normal fresh spawn on PTT-up.
        let _inference_guard = match self.inference_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::info!("CUDA prewarm phase=skipped inference already active");
                return Ok(());
            }
        };

        // A previous PTT cycle must never leave a CUDA model resident. This
        // also handles a backend/model/language switch between two presses.
        self.discard_prepared("superseded by a new PTT press");

        let spec = self.current_spec(&self.language)?;

        if spec.backend != "cuda" {
            tracing::debug!(
                "CUDA prewarm phase=skipped backend={} model={:?}",
                spec.backend,
                spec.model_path
            );
            return Ok(());
        }

        let start = Instant::now();
        tracing::info!(
            "CUDA prewarm phase=spawn model={:?} language={} sidecar={:?}",
            spec.model_path,
            spec.language,
            spec.sidecar
        );
        let running = RunningSidecar::spawn(&spec, true, true, false)?;
        let prepared = PreparedSidecar {
            spec,
            running,
            spawned_at: Instant::now(),
        };
        *self.prepared.lock().unwrap() = Some(prepared);
        tracing::info!("CUDA prewarm phase=spawned elapsed={:?}", start.elapsed());
        Ok(())
    }

    /// Release a prepared CUDA child without transcribing. This is used for
    /// capture failures and very short PTT taps so VRAM is returned promptly.
    pub fn discard_prepared(&self, reason: &str) {
        let prepared = self.prepared.lock().unwrap().take();
        if let Some(prepared) = prepared {
            tracing::info!(
                "CUDA prewarm phase=discard reason={} held_for={:?}",
                reason,
                prepared.spawned_at.elapsed()
            );
            prepared.running.terminate(reason);
        }
    }

    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let _inference_guard = self.inference_lock.lock().unwrap();
        let total_start = Instant::now();
        let spec = match self.current_spec(&self.language) {
            Ok(spec) => spec,
            Err(error) => {
                self.discard_prepared("PTT release could not resolve current configuration");
                return Err(error);
            }
        };

        let prepared = self.prepared.lock().unwrap().take();
        if let Some(prepared) = prepared {
            if prepared.spec == spec {
                let held_for = prepared.spawned_at.elapsed();
                tracing::info!(
                    "CUDA prewarm phase=reuse held_for={:?} samples={}",
                    held_for,
                    samples.len()
                );
                let timeout = transcription_timeout(samples.len());
                let attempt_start = Instant::now();
                let text = match prepared.running.finish(samples, timeout) {
                    Ok(text) => text,
                    Err(prewarm_error) => {
                        // spawn() only proves that the process was created;
                        // CUDA/model initialization happens asynchronously
                        // while recording. If it died in that interval, make
                        // one normal cold attempt so a transient prewarm
                        // failure does not throw away the spoken phrase.
                        tracing::warn!(
                            "CUDA prewarm phase=failed; retrying fresh sidecar: {:#}",
                            prewarm_error
                        );
                        let retry_timeout = timeout.saturating_sub(attempt_start.elapsed());
                        let retry = if retry_timeout.is_zero() {
                            Err(anyhow::anyhow!(
                                "prepared sidecar exhausted the transcription deadline"
                            ))
                        } else {
                            RunningSidecar::spawn(&spec, true, false, false)
                                .and_then(|running| running.finish(samples, retry_timeout))
                        };
                        match retry {
                            Ok(text) => text,
                            Err(retry_error) => {
                                anyhow::bail!(
                                    "prepared CUDA sidecar failed: {prewarm_error:#}; fresh retry failed: {retry_error:#}"
                                );
                            }
                        }
                    }
                };
                tracing::info!(
                    "CUDA prewarm phase=complete total={:?} chars={}",
                    total_start.elapsed(),
                    text.chars().count()
                );
                return Ok(text);
            }

            tracing::info!(
                "CUDA prewarm phase=discard-mismatch prepared_backend={} current_backend={} prepared_model={:?} current_model={:?} prepared_language={} current_language={}",
                prepared.spec.backend,
                spec.backend,
                prepared.spec.model_path,
                spec.model_path,
                prepared.spec.language,
                spec.language,
            );
            prepared
                .running
                .terminate("PTT release configuration mismatch");
        }

        tracing::info!(
            "transcribe phase=fresh-spawn backend={} model={:?} language={}",
            spec.backend,
            spec.model_path,
            spec.language
        );
        let text = RunningSidecar::spawn(&spec, true, false, false)?
            .finish(samples, transcription_timeout(samples.len()))?;
        tracing::info!(
            "transcribe phase=fresh-complete total={:?} chars={}",
            total_start.elapsed(),
            text.chars().count()
        );
        Ok(text)
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
        // The PTT prewarm is intentionally private to the currently-held
        // hotkey. An API request must not run a second GPU sidecar alongside
        // it, so it evicts the prepared child before its own fresh request.
        self.discard_prepared("superseded by HTTP or explicit-language request");
        let total_start = Instant::now();
        let request_language = language.unwrap_or(&self.language);
        let spec = self.current_spec(request_language)?;
        tracing::info!(
            "transcribe phase=http-or-explicit-language backend={} model={:?} sidecar={:?} language={}",
            spec.backend,
            spec.model_path,
            spec.sidecar,
            spec.language
        );
        let text = RunningSidecar::spawn(&spec, false, false, false)?
            .finish(samples, transcription_timeout(samples.len()))?;
        tracing::info!(
            "transcribe phase=http-or-explicit-language-complete total={:?} chars={}",
            total_start.elapsed(),
            text.chars().count()
        );
        Ok(text)
    }

    /// Segment timestamps are measured on the uploaded audio clock. The
    /// sidecar only switches its output protocol for this explicit request.
    pub fn transcribe_timed_with_language(
        &self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<String> {
        let _inference_guard = self.inference_lock.lock().unwrap();
        self.discard_prepared("superseded by timed HTTP request");
        let spec = self.current_spec(language.unwrap_or(&self.language))?;
        RunningSidecar::spawn(&spec, false, false, true)?
            .finish(samples, transcription_timeout(samples.len()))
    }

    fn current_spec(&self, language: &str) -> Result<SidecarSpec> {
        let choice = self.backend_choice.lock().unwrap().clone();
        let (backend, sidecar) = resolve_sidecar(&choice)?;
        let model_path = self.model_path.lock().unwrap().clone();
        if !model_path.exists() {
            anyhow::bail!(
                "model file not found: {:?} — open Models from the tray to download one",
                model_path
            );
        }
        Ok(SidecarSpec {
            backend,
            sidecar,
            model_path,
            language: language.to_string(),
        })
    }
}

impl Drop for Transcriber {
    fn drop(&mut self) {
        if let Some(prepared) = self.prepared.get_mut().unwrap().take() {
            prepared.running.terminate("transcriber dropped");
        }
    }
}

impl RunningSidecar {
    fn spawn(
        spec: &SidecarSpec,
        ptt_request: bool,
        ptt_prewarm: bool,
        segments_json: bool,
    ) -> Result<Self> {
        let mut cmd = Command::new(&spec.sidecar);
        cmd.arg("--model")
            .arg(&spec.model_path)
            .arg("--language")
            .arg(&spec.language)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if segments_json {
            cmd.arg("--segments-json");
        }
        if cfg!(target_os = "linux") && ptt_request && spec.backend == "cuda" {
            // These defaults are tuned for low-latency local dictation on the
            // current Ryzen 5950X / RTX 3080 Ti. Explicit user environment
            // values always win, which keeps every knob independently A/B
            // testable without a rebuild.
            set_default_child_env(&mut cmd, "FLOV_WHISPER_THREADS", "8");
            if ptt_prewarm {
                set_default_child_env(&mut cmd, "FLOV_PTT_PREWARM", "1");
            }
        }

        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn sidecar: {:?}", spec.sidecar))?;

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

        let stdin = child.stdin.take().context("sidecar stdin missing")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        })
    }

    fn finish(mut self, samples: &[f32], timeout: Duration) -> Result<String> {
        let total_start = Instant::now();
        let mut stdin = self.stdin.take().context("sidecar stdin missing")?;
        let write_start = Instant::now();
        let (status, write_result, timed_out) = std::thread::scope(|scope| -> Result<_> {
            let stdin_thread = scope.spawn(move || {
                let result = write_samples_to_stdin(&mut stdin, samples);
                // Dropping stdin closes the pipe → sidecar's read_to_end returns.
                drop(stdin);
                result
            });

            let status = self
                .child
                .wait_timeout(timeout)
                .context("sidecar wait with timeout failed")?;
            let timed_out = status.is_none();
            let status = match status {
                Some(status) => status,
                None => {
                    tracing::error!("sidecar timed out after {:?}; killing process", timeout);
                    let _ = self.child.kill();
                    self.child
                        .wait()
                        .context("sidecar wait after kill failed")?
                }
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
            let stdout_text = self
                .stdout_thread
                .take()
                .expect("sidecar stdout reader missing")
                .join()
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or_default();
            let stderr_text = self
                .stderr_thread
                .take()
                .expect("sidecar stderr reader missing")
                .join()
                .unwrap_or_default();
            anyhow::bail!(
                "sidecar timed out after {:?}; stdout: {}; stderr: {}",
                timeout,
                stdout_text.trim(),
                stderr_text.trim()
            );
        }

        let stdout_buf = self
            .stdout_thread
            .take()
            .expect("sidecar stdout reader missing")
            .join()
            .map_err(|_| anyhow::anyhow!("sidecar stdout reader panicked"))?
            .context("failed to read sidecar stdout")?;
        let stderr_text = self
            .stderr_thread
            .take()
            .expect("sidecar stderr reader missing")
            .join()
            .unwrap_or_default();

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
        log_sidecar_diagnostics(&stderr_text);
        tracing::info!("transcription took {:?}", total_start.elapsed());
        tracing::info!("sidecar phase=finished elapsed={:?}", total_start.elapsed());
        Ok(stdout_buf.trim().to_string())
    }

    fn terminate(mut self, reason: &str) {
        // Closing stdin first lets a sidecar which already completed loading
        // exit naturally. Kill is still required while CUDA is initializing.
        drop(self.stdin.take());
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = self.child.kill() {
                    tracing::debug!("sidecar prewarm kill failed reason={}: {}", reason, error);
                }
                if let Err(error) = self.child.wait() {
                    tracing::debug!("sidecar prewarm wait failed reason={}: {}", reason, error);
                }
            }
            Err(error) => tracing::debug!(
                "sidecar prewarm try_wait failed reason={}: {}",
                reason,
                error
            ),
        }
        if let Some(stdout_thread) = self.stdout_thread.take() {
            let _ = stdout_thread.join();
        }
        if let Some(stderr_thread) = self.stderr_thread.take() {
            if let Ok(stderr_text) = stderr_thread.join() {
                log_sidecar_diagnostics(&stderr_text);
            }
        }
        tracing::info!("CUDA prewarm phase=released reason={}", reason);
    }
}

impl Drop for RunningSidecar {
    fn drop(&mut self) {
        // Child::drop deliberately does not reap or terminate the child. An
        // error while writing/waiting must therefore still release a loading
        // CUDA context instead of leaving it resident until app exit.
        drop(self.stdin.take());
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        if let Some(stdout_thread) = self.stdout_thread.take() {
            let _ = stdout_thread.join();
        }
        if let Some(stderr_thread) = self.stderr_thread.take() {
            let _ = stderr_thread.join();
        }
    }
}

fn set_default_child_env(cmd: &mut Command, name: &str, value: &str) {
    if std::env::var_os(name).is_none() {
        cmd.env(name, value);
    }
}

fn log_sidecar_diagnostics(stderr_text: &str) {
    for line in stderr_text.lines().filter(|line| {
        line.starts_with("flov-whisper-cuda timing")
            || line.starts_with("flov-whisper-cuda memory_inputs")
    }) {
        tracing::info!("{}", line);
    }
    if !stderr_text.trim().is_empty() {
        tracing::debug!("sidecar stderr: {}", stderr_text.trim());
    }
}

fn write_samples_to_stdin<W: Write>(stdin: &mut W, samples: &[f32]) -> Result<()> {
    #[cfg(target_endian = "little")]
    {
        // Every supported desktop target is little-endian and the sidecar
        // protocol is raw f32 LE. Borrow the existing PCM allocation instead
        // of formatting every sample into a second staging Vec.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                samples.as_ptr().cast::<u8>(),
                std::mem::size_of_val(samples),
            )
        };
        stdin
            .write_all(bytes)
            .context("failed to write samples to sidecar")?;
        Ok(())
    }

    #[cfg(target_endian = "big")]
    {
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

    #[test]
    fn prewarm_snapshot_requires_exact_backend_model_and_language_match() {
        let base = SidecarSpec {
            backend: "cuda".into(),
            sidecar: PathBuf::from("/opt/flov-whisper-cuda"),
            model_path: PathBuf::from("/models/base.bin"),
            language: "ru".into(),
        };
        let mut changed_model = base.clone();
        changed_model.model_path = PathBuf::from("/models/small.bin");
        let mut changed_language = base.clone();
        changed_language.language = "en".into();
        let mut changed_backend = base.clone();
        changed_backend.backend = "cpu".into();

        assert_eq!(base, base.clone());
        assert_ne!(base, changed_model);
        assert_ne!(base, changed_language);
        assert_ne!(base, changed_backend);
    }
}
