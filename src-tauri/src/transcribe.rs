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
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Backends tried, in order, when the user picked "auto" (or did not pick).
/// First one whose binary exists next to flov.exe wins.
pub const BACKEND_PRIORITY: &[&str] = &["cuda", "vulkan", "metal", "cpu"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendCandidate {
    name: String,
    sidecar: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    Auto,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidecarPlan {
    mode: SelectionMode,
    candidates: Vec<BackendCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendUnavailable {
    MissingBinary(PathBuf),
    MissingCudaRuntime,
}

impl BackendUnavailable {
    fn user_message(&self) -> String {
        match self {
            Self::MissingBinary(path) => format!("{:?} not found", path),
            Self::MissingCudaRuntime => {
                "CUDA runtime unavailable: nvcuda.dll was not found in System32".into()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarFailureKind {
    Spawn,
    MissingRuntime,
    RuntimeInit,
    Model,
    Io,
    Timeout,
    Exit,
}

impl SidecarFailureKind {
    fn allows_auto_fallback(self) -> bool {
        matches!(self, Self::Spawn | Self::MissingRuntime | Self::RuntimeInit)
    }
}

#[derive(Debug, Clone)]
struct SidecarInvocationError {
    backend: String,
    sidecar: PathBuf,
    kind: SidecarFailureKind,
    detail: String,
}

impl SidecarInvocationError {
    fn new(
        backend: &str,
        sidecar: &Path,
        kind: SidecarFailureKind,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            backend: backend.to_string(),
            sidecar: sidecar.to_path_buf(),
            kind,
            detail: detail.into(),
        }
    }

    fn allows_auto_fallback(&self) -> bool {
        self.kind.allows_auto_fallback()
    }

    fn summary(&self) -> String {
        format!("{} {:?}: {}", self.backend, self.kind, self.detail)
    }
}

impl fmt::Display for SidecarInvocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} sidecar failed ({:?}) at {:?}: {}",
            self.backend, self.kind, self.sidecar, self.detail
        )
    }
}

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
    let cuda_available = cuda_runtime_present();
    BACKEND_PRIORITY
        .iter()
        .filter_map(|backend| backend_candidate_in_dir(&dir, backend, cuda_available).ok())
        .map(|candidate| candidate.name)
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
    false
}

pub struct Transcriber {
    model_path: Arc<Mutex<PathBuf>>,
    language: String,
    /// Shared with the tray menu; updated when the user picks a backend.
    backend_choice: Arc<Mutex<String>>,
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
        })
    }

    /// Lightweight pre-flight: does the configured model file exist on
    /// disk right now? Used before starting a recording so we can show
    /// "no model" before the user wastes a sentence into the void.
    pub fn has_model(&self) -> bool {
        self.model_path.lock().unwrap().exists()
    }

    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let total_start = Instant::now();
        let choice = self.backend_choice.lock().unwrap().clone();
        let plan = resolve_sidecar_plan(&choice)?;
        let model_path = self.model_path.lock().unwrap().clone();
        if !model_path.is_file() {
            anyhow::bail!(
                "model file not found: {:?} — open Models from the tray to download one",
                model_path
            );
        }

        let mut fallback_errors = Vec::new();
        for candidate in &plan.candidates {
            tracing::info!(
                "transcribe via {} | model={:?} | sidecar={:?}",
                candidate.name,
                model_path,
                candidate.sidecar
            );
            match run_sidecar(candidate, &model_path, &self.language, samples, total_start) {
                Ok(text) => {
                    tracing::info!(
                        "transcription via {} took {:?}",
                        candidate.name,
                        total_start.elapsed()
                    );
                    return Ok(text);
                }
                Err(e) if plan.mode == SelectionMode::Auto && e.allows_auto_fallback() => {
                    tracing::warn!(
                        "auto backend {} failed with fallback-safe error: {}",
                        candidate.name,
                        e
                    );
                    fallback_errors.push(e);
                }
                Err(e) => anyhow::bail!("{}", e),
            }
        }

        let summaries = fallback_errors
            .iter()
            .map(SidecarInvocationError::summary)
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("all auto whisper backends failed: {}", summaries)
    }
}

fn run_sidecar(
    candidate: &BackendCandidate,
    model_path: &Path,
    language: &str,
    samples: &[f32],
    total_start: Instant,
) -> std::result::Result<String, SidecarInvocationError> {
    let backend = candidate.name.as_str();
    let sidecar = candidate.sidecar.as_path();
    let mut cmd = Command::new(sidecar);
    cmd.arg("--model")
        .arg(model_path)
        .arg("--language")
        .arg(language)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().map_err(|e| {
        SidecarInvocationError::new(
            backend,
            sidecar,
            classify_spawn_error(&e),
            format!("failed to spawn: {e}"),
        )
    })?;

    let mut stdout = child.stdout.take().ok_or_else(|| {
        SidecarInvocationError::new(backend, sidecar, SidecarFailureKind::Io, "stdout missing")
    })?;
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).map(|_| buf)
    });

    let mut stderr = child.stderr.take().ok_or_else(|| {
        SidecarInvocationError::new(backend, sidecar, SidecarFailureKind::Io, "stderr missing")
    })?;
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let timeout = transcription_timeout(samples.len());
    let mut stdin = child.stdin.take().ok_or_else(|| {
        SidecarInvocationError::new(backend, sidecar, SidecarFailureKind::Io, "stdin missing")
    })?;
    let write_start = Instant::now();
    let (status, write_result, timed_out) = std::thread::scope(|scope| {
        let stdin_thread = scope.spawn(move || {
            let result = write_samples_to_stdin(&mut stdin, samples);
            // Dropping stdin closes the pipe, allowing the sidecar read loop to finish.
            drop(stdin);
            result
        });

        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    return Err(SidecarInvocationError::new(
                        backend,
                        sidecar,
                        SidecarFailureKind::Io,
                        format!("wait failed: {e}"),
                    ));
                }
            }
            if total_start.elapsed() > timeout {
                timed_out = true;
                tracing::error!(
                    "sidecar {} timed out after {:?}; killing process",
                    backend,
                    total_start.elapsed()
                );
                let _ = child.kill();
                break child.wait().map_err(|e| {
                    SidecarInvocationError::new(
                        backend,
                        sidecar,
                        SidecarFailureKind::Io,
                        format!("wait after kill failed: {e}"),
                    )
                })?;
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let write_result = stdin_thread.join().map_err(|_| {
            SidecarInvocationError::new(
                backend,
                sidecar,
                SidecarFailureKind::Io,
                "stdin writer panicked",
            )
        })?;
        Ok((status, write_result, timed_out))
    })?;
    tracing::debug!(
        "sidecar {} stdin writer finished after {:?} for {} samples",
        backend,
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
        return Err(SidecarInvocationError::new(
            backend,
            sidecar,
            SidecarFailureKind::Timeout,
            format!(
                "timed out after {:?}; stdout: {}; stderr: {}",
                timeout,
                stdout_text.trim(),
                stderr_text.trim()
            ),
        ));
    }

    let stdout_buf = stdout_thread
        .join()
        .map_err(|_| {
            SidecarInvocationError::new(
                backend,
                sidecar,
                SidecarFailureKind::Io,
                "stdout reader panicked",
            )
        })?
        .map_err(|e| {
            SidecarInvocationError::new(
                backend,
                sidecar,
                SidecarFailureKind::Io,
                format!("stdout read failed: {e}"),
            )
        })?;
    let stderr_text = stderr_thread.join().unwrap_or_default();

    if let Err(e) = write_result {
        let kind = classify_write_error(backend, &stderr_text);
        return Err(SidecarInvocationError::new(
            backend,
            sidecar,
            kind,
            format!(
                "failed to write samples: {e:#}; stderr: {}",
                stderr_text.trim()
            ),
        ));
    }
    if !status.success() {
        let kind = classify_exit_error(backend, status, &stderr_text);
        return Err(SidecarInvocationError::new(
            backend,
            sidecar,
            kind,
            format!(
                "exited with {:?}; stderr: {}",
                status.code(),
                stderr_text.trim()
            ),
        ));
    }
    if !stderr_text.trim().is_empty() {
        tracing::debug!("sidecar {} stderr: {}", backend, stderr_text.trim());
    }
    Ok(stdout_buf.trim().to_string())
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

fn backend_candidate_in_dir(
    dir: &Path,
    backend: &str,
    cuda_runtime_available: bool,
) -> std::result::Result<BackendCandidate, BackendUnavailable> {
    let candidate = dir.join(backend_bin_name(backend));
    if !candidate.exists() {
        return Err(BackendUnavailable::MissingBinary(candidate));
    }
    if backend == "cuda" && !cuda_runtime_available {
        return Err(BackendUnavailable::MissingCudaRuntime);
    }
    Ok(BackendCandidate {
        name: backend.to_string(),
        sidecar: candidate,
    })
}

/// Picks the sidecar candidates to spawn given a user choice.
/// - "auto" → first available in BACKEND_PRIORITY
/// - "cuda" / "vulkan" / "metal" / "cpu" → that one specifically (errors if
///   missing — the menu greys out missing ones, so this only fires if the
///   binary was deleted between startup and the click).
/// - FLOV_BACKEND env var, when set, overrides the choice — useful for
///   one-off comparisons without touching the menu.
fn resolve_sidecar_plan(choice: &str) -> Result<SidecarPlan> {
    let dir = exe_dir()?;
    let effective = std::env::var("FLOV_BACKEND").unwrap_or_else(|_| choice.to_string());
    resolve_sidecar_plan_in_dir(&dir, &effective, cuda_runtime_present())
}

fn resolve_sidecar_plan_in_dir(
    dir: &Path,
    effective: &str,
    cuda_runtime_available: bool,
) -> Result<SidecarPlan> {
    if effective != "auto" {
        if !BACKEND_PRIORITY.contains(&effective) {
            anyhow::bail!(
                "unknown backend '{}'; expected one of {:?} or auto",
                effective,
                BACKEND_PRIORITY
            );
        }
        let candidate =
            backend_candidate_in_dir(dir, effective, cuda_runtime_available).map_err(|reason| {
                anyhow::anyhow!(
                    "backend '{}' selected but {}",
                    effective,
                    reason.user_message()
                )
            })?;
        return Ok(SidecarPlan {
            mode: SelectionMode::Explicit,
            candidates: vec![candidate],
        });
    }

    let mut candidates = Vec::new();
    let mut unavailable = Vec::new();
    for backend in BACKEND_PRIORITY {
        match backend_candidate_in_dir(dir, backend, cuda_runtime_available) {
            Ok(candidate) => candidates.push(candidate),
            Err(reason) => unavailable.push(format!("{} ({})", backend, reason.user_message())),
        }
    }
    if candidates.is_empty() {
        anyhow::bail!(
            "no usable whisper sidecar found in {:?}; unavailable: {}",
            dir,
            unavailable.join(", ")
        );
    }
    Ok(SidecarPlan {
        mode: SelectionMode::Auto,
        candidates,
    })
}

fn classify_spawn_error(error: &std::io::Error) -> SidecarFailureKind {
    #[cfg(target_os = "windows")]
    {
        const ERROR_MOD_NOT_FOUND: i32 = 126;
        const ERROR_PROC_NOT_FOUND: i32 = 127;
        if matches!(
            error.raw_os_error(),
            Some(ERROR_MOD_NOT_FOUND | ERROR_PROC_NOT_FOUND)
        ) {
            return SidecarFailureKind::MissingRuntime;
        }
    }
    SidecarFailureKind::Spawn
}

fn classify_write_error(backend: &str, stderr_text: &str) -> SidecarFailureKind {
    let text_kind = classify_sidecar_text(backend, stderr_text);
    if text_kind != SidecarFailureKind::Exit {
        return text_kind;
    }
    if backend != "cpu" {
        SidecarFailureKind::RuntimeInit
    } else {
        SidecarFailureKind::Io
    }
}

fn classify_exit_error(backend: &str, status: ExitStatus, stderr_text: &str) -> SidecarFailureKind {
    if is_missing_runtime_status(status) {
        return SidecarFailureKind::MissingRuntime;
    }
    classify_sidecar_text(backend, stderr_text)
}

fn classify_sidecar_text(backend: &str, stderr_text: &str) -> SidecarFailureKind {
    let lower = stderr_text.to_ascii_lowercase();
    let has_runtime_word = [
        "nvcuda",
        "cublas",
        "cudart",
        "ggml_cuda",
        "ggml_vulkan",
        "ggml_metal",
        "cuda driver",
        "cuda init",
        "vkcreate",
        "vk_",
        "vulkan init",
        "vulkan device",
        "metal init",
        "metal device",
        "gpu",
        "no device",
        "device not found",
        "driver",
        "dll",
        "dyld",
        "symbol not found",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let has_failure_word = [
        "failed",
        "failure",
        "error",
        "not found",
        "unavailable",
        "unsupported",
        "cannot",
        "could not",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    if backend != "cpu" && has_runtime_word && has_failure_word {
        return SidecarFailureKind::RuntimeInit;
    }
    if lower.contains("model file not found")
        || lower.contains("invalid model path")
        || lower.contains("failed to load whisper model")
        || lower.contains("invalid model")
        || lower.contains("gguf")
    {
        return SidecarFailureKind::Model;
    }
    if lower.contains("vcruntime")
        || lower.contains("msvcp")
        || lower.contains("dll was not found")
        || lower.contains("shared library")
    {
        return SidecarFailureKind::MissingRuntime;
    }
    SidecarFailureKind::Exit
}

fn is_missing_runtime_status(status: ExitStatus) -> bool {
    #[cfg(target_os = "windows")]
    {
        const STATUS_DLL_NOT_FOUND: i32 = 0xC000_0135u32 as i32;
        const STATUS_ENTRYPOINT_NOT_FOUND: i32 = 0xC000_0139u32 as i32;
        if matches!(
            status.code(),
            Some(STATUS_DLL_NOT_FOUND | STATUS_ENTRYPOINT_NOT_FOUND)
        ) {
            return true;
        }
    }
    let _ = status;
    false
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
    fn auto_skips_cuda_when_runtime_is_missing() {
        let dir = unique_test_dir("auto-skips-cuda");
        std::fs::write(dir.join(backend_bin_name("cuda")), b"").unwrap();
        std::fs::write(dir.join(backend_bin_name("vulkan")), b"").unwrap();
        std::fs::write(dir.join(backend_bin_name("cpu")), b"").unwrap();

        let plan = resolve_sidecar_plan_in_dir(&dir, "auto", false).unwrap();

        let names: Vec<_> = plan.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["vulkan", "cpu"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_cuda_without_runtime_has_actionable_error() {
        let dir = unique_test_dir("explicit-cuda-no-runtime");
        std::fs::write(dir.join(backend_bin_name("cuda")), b"").unwrap();

        let err = resolve_sidecar_plan_in_dir(&dir, "cuda", false).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("backend 'cuda' selected"));
        assert!(msg.contains("CUDA runtime unavailable"));
        assert!(msg.contains("nvcuda.dll"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_init_errors_are_fallback_safe() {
        let kind = classify_sidecar_text(
            "cuda",
            "ggml_cuda_init: failed to initialize CUDA driver: no device",
        );

        assert_eq!(kind, SidecarFailureKind::RuntimeInit);
        assert!(kind.allows_auto_fallback());
    }

    #[test]
    fn corrupt_model_errors_do_not_fallback() {
        let kind = classify_sidecar_text(
            "cuda",
            "flov-whisper-cuda error: failed to load whisper model: invalid model magic",
        );

        assert_eq!(kind, SidecarFailureKind::Model);
        assert!(!kind.allows_auto_fallback());
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flov-transcribe-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
