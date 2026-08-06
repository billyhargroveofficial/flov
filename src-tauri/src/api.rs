//! Local/OpenAI-compatible HTTP API.
//!
//! The endpoint intentionally shares the same `Transcriber` as the desktop
//! recording loop. Inference is serialized inside `Transcriber`, so a local
//! hotkey recording and an uploaded file cannot load two GPU models at once.

use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::Emitter;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::ServerConfig;
use crate::{audio, hotkey, postprocess, stats, transcribe};

const MAX_REQUEST_THREADS: usize = 8;
const MIN_AUDIO_SAMPLES: usize = 1_600;

#[derive(Clone)]
pub struct ApiRuntime {
    /// Desktop event sink for the Settings/Stats UI. `None` in headless
    /// mode, where no Tauri app exists and emitting would have no target.
    pub app: Option<tauri::AppHandle>,
    pub transcriber: Arc<transcribe::Transcriber>,
    pub post_processor: Arc<Mutex<Option<Arc<postprocess::PostProcessor>>>>,
    pub stats: Arc<stats::Stats>,
    pub active_mode: Arc<AtomicU8>,
    /// Desktop runs the microphone push-to-talk loop behind the recording
    /// endpoints; headless mode has none, so those endpoints must report
    /// unavailable instead of pretending to capture audio.
    pub recording_supported: bool,
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

impl ApiError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(500, error.to_string())
    }
}

struct ApiResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl ApiResponse {
    fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn text(status: u16, value: String) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: value.into_bytes(),
        }
    }
}

struct Upload {
    audio: Vec<u8>,
    filename: Option<String>,
    content_type: Option<String>,
    language: Option<String>,
    response_format: String,
    postprocess: Option<bool>,
}

struct DecodedAudio {
    samples: Vec<f32>,
    duration_seconds: f64,
}

struct MultipartPart {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
}

struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Bind and spawn the API server. Binding happens synchronously so a bad
/// address or an occupied port is visible immediately in the application log.
///
/// Returns the actually bound address (`127.0.0.1:17432` for the default
/// config, an ephemeral port when `bind` ends in `:0`), or `None` when the
/// server is disabled in the config.
pub fn spawn(config: ServerConfig, runtime: ApiRuntime) -> Result<Option<SocketAddr>> {
    if !config.enabled {
        tracing::info!("HTTP API disabled");
        return Ok(None);
    }

    let bind: SocketAddr = config
        .bind
        .parse()
        .with_context(|| format!("invalid server.bind '{}'", config.bind))?;
    if !bind.ip().is_loopback() && config.api_key.trim().is_empty() {
        anyhow::bail!(
            "server.api_key is required when server.bind is not a loopback address ({bind})"
        );
    }
    if config.max_body_mb == 0 {
        anyhow::bail!("server.max_body_mb must be greater than zero");
    }
    if config.max_audio_seconds == 0 {
        anyhow::bail!("server.max_audio_seconds must be greater than zero");
    }

    let server = Server::http(bind)
        .map_err(|e| anyhow::anyhow!("failed to bind HTTP API on {bind}: {e}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .context("HTTP API bound a non-IP address")?;
    let runtime = Arc::new(runtime);
    let in_flight = Arc::new(AtomicUsize::new(0));
    tracing::info!(
        "HTTP API listening on http://{bound} (OpenAI endpoint: /v1/audio/transcriptions)"
    );

    std::thread::Builder::new()
        .name("flov-http-listener".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                if active > MAX_REQUEST_THREADS {
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    respond(
                        request,
                        error_response(ApiError::new(
                            503,
                            "server is busy; retry after the current transcription",
                        )),
                    );
                    continue;
                }

                let runtime = runtime.clone();
                let config = config.clone();
                let guard = InFlightGuard(in_flight.clone());
                let spawn_result = std::thread::Builder::new()
                    .name("flov-http-request".into())
                    .spawn(move || {
                        let _guard = guard;
                        handle_request(request, &config, &runtime);
                    });
                if let Err(e) = spawn_result {
                    // The guard was moved into the failed Builder closure and
                    // is dropped with it, restoring the counter.
                    tracing::error!("failed to spawn HTTP request worker: {}", e);
                }
            }
        })
        .context("spawn HTTP API listener")?;

    Ok(Some(bound))
}

fn handle_request(mut request: Request, config: &ServerConfig, runtime: &ApiRuntime) {
    let started = Instant::now();
    let method = request.method().clone();
    let url = request.url().to_string();
    let remote = request
        .remote_addr()
        .map(ToString::to_string)
        .unwrap_or_else(|| "unknown".into());

    let response = match route(&mut request, config, runtime) {
        Ok(response) => response,
        Err(error) => error_response(error),
    };
    let status = response.status;
    respond(request, response);
    tracing::info!(
        "HTTP {} {} from {} -> {} in {:?}",
        method.as_str(),
        path_only(&url),
        remote,
        status,
        started.elapsed()
    );
}

fn route(
    request: &mut Request,
    config: &ServerConfig,
    runtime: &ApiRuntime,
) -> std::result::Result<ApiResponse, ApiError> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = path_only(&url);

    if method == Method::Get && matches!(path, "/health" | "/v1/health") {
        let recording = health_recording(
            runtime.recording_supported,
            runtime.active_mode.load(Ordering::SeqCst),
        );
        return Ok(ApiResponse::json(
            200,
            json!({
                "status": "ok",
                "model_loaded": runtime.transcriber.has_model(),
                "recording": recording,
            }),
        ));
    }

    authorize(request, &config.api_key)?;

    match (method, path) {
        (Method::Get, "/v1/models") => Ok(ApiResponse::json(
            200,
            json!({
                "object": "list",
                "data": [{
                    "id": "flov-whisper",
                    "object": "model",
                    "owned_by": "local"
                }]
            }),
        )),
        (Method::Get, "/v1/recording") => {
            require_recording(runtime.recording_supported)?;
            Ok(recording_state(runtime))
        }
        (Method::Post, "/v1/recording/start") => {
            require_recording(runtime.recording_supported)?;
            runtime
                .active_mode
                .store(hotkey::MODE_TRANSCRIBE, Ordering::SeqCst);
            Ok(recording_state(runtime))
        }
        (Method::Post, "/v1/recording/stop") => {
            require_recording(runtime.recording_supported)?;
            runtime
                .active_mode
                .store(hotkey::MODE_IDLE, Ordering::SeqCst);
            Ok(recording_state(runtime))
        }
        (Method::Post, "/v1/audio/transcriptions") => {
            transcribe_upload(request, &url, config, runtime)
        }
        (Method::Options, _) => Ok(ApiResponse {
            status: 204,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }),
        _ => Err(ApiError::new(404, "endpoint not found")),
    }
}

/// What `/health` reports for `recording`. Headless mode has no
/// microphone cycle, so the flag stays false there even if the mode
/// byte is flipped. Pure function so the semantics are unit-tested
/// without a `Transcriber`, `Stats`, or a bound port.
fn health_recording(recording_supported: bool, active_mode: u8) -> bool {
    recording_supported && active_mode != hotkey::MODE_IDLE
}

/// Recording endpoints must not pretend to capture audio when the process
/// has no microphone loop (headless mode). Fail loudly instead. Pure for
/// the same reason as `health_recording`.
fn require_recording(recording_supported: bool) -> std::result::Result<(), ApiError> {
    if recording_supported {
        Ok(())
    } else {
        Err(ApiError::new(
            503,
            "recording endpoints are unavailable in headless mode; upload audio to POST /v1/audio/transcriptions instead",
        ))
    }
}

fn recording_state(runtime: &ApiRuntime) -> ApiResponse {
    let recording = runtime.active_mode.load(Ordering::SeqCst) != hotkey::MODE_IDLE;
    ApiResponse::json(200, json!({ "recording": recording }))
}

fn authorize(request: &Request, configured_key: &str) -> std::result::Result<(), ApiError> {
    let configured_key = configured_key.trim();
    if configured_key.is_empty() {
        return Ok(());
    }
    let supplied = header_value(request, "Authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(configured_key.as_bytes(), supplied.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::new(401, "invalid or missing bearer token"))
    }
}

fn constant_time_eq(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .iter()
        .zip(supplied)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn transcribe_upload(
    request: &mut Request,
    url: &str,
    config: &ServerConfig,
    runtime: &ApiRuntime,
) -> std::result::Result<ApiResponse, ApiError> {
    if !runtime.transcriber.has_model() {
        return Err(ApiError::new(
            503,
            "Whisper model is not configured; open Settings -> Models",
        ));
    }

    let upload = read_upload(request, url, config.max_body_mb)?;
    if !matches!(
        upload.response_format.as_str(),
        "" | "json" | "verbose_json" | "text"
    ) {
        return Err(ApiError::bad_request(format!(
            "unsupported response_format '{}'; use json, verbose_json, or text",
            upload.response_format
        )));
    }
    let language = validate_language(upload.language.as_deref())?;
    let should_postprocess = upload.postprocess.unwrap_or(config.postprocess);
    let post_processor = if should_postprocess {
        Some(
            runtime
                .post_processor
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "postprocess=true but OpenRouter is not configured in flov settings",
                    )
                })?,
        )
    } else {
        None
    };
    let decoded = decode_audio(
        &upload.audio,
        upload.filename.as_deref(),
        upload.content_type.as_deref(),
        config.max_audio_seconds,
    )
    .map_err(|e| ApiError::bad_request(format!("audio decode failed: {e:#}")))?;

    let raw_text = runtime
        .transcriber
        .transcribe_with_language(&decoded.samples, language.as_deref())
        .map_err(|e| ApiError::internal(format!("transcription failed: {e:#}")))?;

    if !raw_text.is_empty() {
        runtime
            .stats
            .record(raw_text.chars().count() as u64, decoded.duration_seconds);
        // Headless mode has no AppHandle; stats are still persisted above.
        if let Some(app) = &runtime.app {
            let _ = app.emit("stats-updated", ());
        }
    }

    let text = if let Some(processor) = post_processor {
        processor
            .process(&raw_text)
            .map_err(|e| ApiError::internal(format!("postprocess failed: {e:#}")))?
    } else {
        raw_text
    };

    match upload.response_format.as_str() {
        "text" => Ok(ApiResponse::text(200, text)),
        "verbose_json" => Ok(ApiResponse::json(
            200,
            json!({
                "task": "transcribe",
                "language": language
                    .as_deref()
                    .unwrap_or_else(|| runtime.transcriber.default_language()),
                "duration": decoded.duration_seconds,
                "text": text,
                "segments": [],
            }),
        )),
        "json" | "" => Ok(ApiResponse::json(200, json!({ "text": text }))),
        _ => unreachable!("response_format validated before transcription"),
    }
}

fn read_upload(
    request: &mut Request,
    url: &str,
    max_body_mb: u64,
) -> std::result::Result<Upload, ApiError> {
    let max_bytes_u64 = max_body_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| ApiError::bad_request("server.max_body_mb is too large"))?;
    let max_bytes = usize::try_from(max_bytes_u64)
        .map_err(|_| ApiError::bad_request("server.max_body_mb is too large"))?;
    if request
        .body_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(ApiError::new(
            413,
            format!("request body exceeds {max_body_mb} MiB"),
        ));
    }

    let content_type = header_value(request, "Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();
    let mut body = Vec::with_capacity(request.body_length().unwrap_or(0).min(max_bytes));
    request
        .as_reader()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut body)
        .map_err(ApiError::internal)?;
    if body.len() > max_bytes {
        return Err(ApiError::new(
            413,
            format!("request body exceeds {max_body_mb} MiB"),
        ));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("empty request body"));
    }

    let query = parse_query(url);
    if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        let boundary = content_type_param(&content_type, "boundary")
            .ok_or_else(|| ApiError::bad_request("multipart boundary is missing"))?;
        let parts = parse_multipart(&body, &boundary)?;
        let file = parts
            .iter()
            .find(|part| part.name == "file" || part.name == "audio")
            .ok_or_else(|| ApiError::bad_request("multipart field 'file' is required"))?;
        if file.data.is_empty() {
            return Err(ApiError::bad_request("uploaded audio file is empty"));
        }

        let field = |name: &str| -> Option<String> {
            parts
                .iter()
                .find(|part| part.name == name && part.filename.is_none())
                .and_then(|part| {
                    if part.data.len() > 8 * 1024 {
                        None
                    } else {
                        std::str::from_utf8(&part.data)
                            .ok()
                            .map(|value| value.trim().to_string())
                    }
                })
        };

        Ok(Upload {
            audio: file.data.clone(),
            filename: file.filename.clone(),
            content_type: file.content_type.clone(),
            language: field("language").or_else(|| query_value(&query, "language")),
            response_format: field("response_format")
                .or_else(|| query_value(&query, "response_format"))
                .unwrap_or_else(|| "json".into()),
            postprocess: field("postprocess")
                .or_else(|| query_value(&query, "postprocess"))
                .map(|value| parse_bool(&value))
                .transpose()?,
        })
    } else {
        Ok(Upload {
            audio: body,
            filename: header_value(request, "X-Filename").map(str::to_string),
            content_type: Some(
                content_type
                    .split(';')
                    .next()
                    .unwrap_or("application/octet-stream")
                    .trim()
                    .to_string(),
            ),
            language: query_value(&query, "language"),
            response_format: query_value(&query, "response_format")
                .unwrap_or_else(|| "json".into()),
            postprocess: query_value(&query, "postprocess")
                .map(|value| parse_bool(&value))
                .transpose()?,
        })
    }
}

fn parse_bool(value: &str) -> std::result::Result<bool, ApiError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ApiError::bad_request(format!(
            "invalid boolean value '{value}'"
        ))),
    }
}

fn validate_language(value: Option<&str>) -> std::result::Result<Option<String>, ApiError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > 16
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::bad_request(
            "language must be a short ISO code such as 'ru' or 'en'",
        ));
    }
    Ok(Some(value.to_string()))
}

fn parse_multipart(
    body: &[u8],
    boundary: &str,
) -> std::result::Result<Vec<MultipartPart>, ApiError> {
    if boundary.is_empty() || boundary.len() > 200 || boundary.contains(['\r', '\n']) {
        return Err(ApiError::bad_request("invalid multipart boundary"));
    }
    let marker = format!("--{boundary}").into_bytes();
    let delimiter = [b"\r\n".as_slice(), marker.as_slice()].concat();
    if !body.starts_with(&marker) {
        return Err(ApiError::bad_request("malformed multipart body"));
    }

    let mut cursor = marker.len();
    let mut parts = Vec::new();
    loop {
        if body.get(cursor..cursor + 2) == Some(b"--") {
            break;
        }
        if body.get(cursor..cursor + 2) != Some(b"\r\n") {
            return Err(ApiError::bad_request("malformed multipart boundary"));
        }
        cursor += 2;

        let header_end_rel = find_bytes(&body[cursor..], b"\r\n\r\n")
            .ok_or_else(|| ApiError::bad_request("multipart headers are incomplete"))?;
        let header_end = cursor + header_end_rel;
        let headers = std::str::from_utf8(&body[cursor..header_end])
            .map_err(|_| ApiError::bad_request("multipart headers are not UTF-8"))?;
        let data_start = header_end + 4;
        let next_rel = find_bytes(&body[data_start..], &delimiter)
            .ok_or_else(|| ApiError::bad_request("multipart closing boundary is missing"))?;
        let data_end = data_start + next_rel;

        let mut disposition = None;
        let mut content_type = None;
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                return Err(ApiError::bad_request("malformed multipart header"));
            };
            if name.trim().eq_ignore_ascii_case("Content-Disposition") {
                disposition = Some(value.trim());
            } else if name.trim().eq_ignore_ascii_case("Content-Type") {
                content_type = Some(value.trim().to_string());
            }
        }
        let disposition = disposition
            .ok_or_else(|| ApiError::bad_request("multipart part has no Content-Disposition"))?;
        let name = content_type_param(disposition, "name")
            .ok_or_else(|| ApiError::bad_request("multipart part has no name"))?;
        let filename = content_type_param(disposition, "filename").map(safe_filename);

        parts.push(MultipartPart {
            name,
            filename,
            content_type,
            data: body[data_start..data_end].to_vec(),
        });
        if parts.len() > 32 {
            return Err(ApiError::bad_request("too many multipart fields"));
        }

        cursor = data_end + delimiter.len();
    }
    Ok(parts)
}

fn content_type_param(value: &str, wanted: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case(wanted) {
            return None;
        }
        let value = value.trim();
        Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .replace("\\\"", "\""),
        )
    })
}

fn safe_filename(filename: String) -> String {
    filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&filename)
        .to_string()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_audio(
    encoded: &[u8],
    filename: Option<&str>,
    content_type: Option<&str>,
    max_audio_seconds: u64,
) -> Result<DecodedAudio> {
    let mut hint = Hint::new();
    if let Some(extension) = filename
        .and_then(|name| std::path::Path::new(name).extension())
        .and_then(|extension| extension.to_str())
    {
        hint.with_extension(extension);
    }
    if let Some(content_type) = content_type {
        hint.mime_type(content_type.split(';').next().unwrap_or(content_type));
    }

    let source =
        MediaSourceStream::new(Box::new(Cursor::new(encoded.to_vec())), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("unsupported or malformed audio container")?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .context("audio stream has no decodable track")?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("unsupported audio codec")?;

    let mut source_rate = None;
    let mut mono = Vec::<f32>::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                anyhow::bail!("chained audio streams are not supported")
            }
            Err(error) => return Err(error).context("failed to read audio packet"),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(message)) => {
                tracing::warn!("skipping malformed audio packet: {}", message);
                continue;
            }
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error).context("failed to decode audio packet"),
        };
        let spec = *decoded.spec();
        let rate = spec.rate;
        if rate == 0 {
            anyhow::bail!("decoded audio has an invalid sample rate");
        }
        match source_rate {
            None => source_rate = Some(rate),
            Some(previous) if previous != rate => {
                anyhow::bail!("sample-rate changes inside one upload are not supported")
            }
            _ => {}
        }
        let channels = spec.channels.count();
        if channels == 0 {
            anyhow::bail!("decoded audio has no channels");
        }

        let mut interleaved = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        interleaved.copy_interleaved_ref(decoded);
        mono.reserve(interleaved.samples().len() / channels);
        for frame in interleaved.samples().chunks(channels) {
            let sample = frame.iter().copied().sum::<f32>() / channels as f32;
            mono.push(if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            });
        }
        if mono.len() as u64 > max_audio_seconds.saturating_mul(rate as u64) {
            anyhow::bail!("decoded audio exceeds {max_audio_seconds} seconds");
        }
    }

    let source_rate = source_rate.context("audio decoder produced no samples")?;
    let samples = if source_rate == audio::TRANSCRIBE_SAMPLE_RATE {
        mono
    } else {
        audio::resample(&mono, source_rate, audio::TRANSCRIBE_SAMPLE_RATE)
    };
    if samples.len() < MIN_AUDIO_SAMPLES {
        anyhow::bail!("audio is shorter than 100 ms");
    }
    let duration_seconds = samples.len() as f64 / audio::TRANSCRIBE_SAMPLE_RATE as f64;
    if duration_seconds > max_audio_seconds as f64 {
        anyhow::bail!("decoded audio exceeds {max_audio_seconds} seconds");
    }

    Ok(DecodedAudio {
        samples,
        duration_seconds,
    })
}

fn parse_query(url: &str) -> Vec<(String, String)> {
    let Some((_, query)) = url.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

fn query_value(query: &[(String, String)], wanted: &str) -> Option<String> {
    query
        .iter()
        .find(|(name, _)| name == wanted)
        .map(|(_, value)| value.clone())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'+' => {
                output.push(b' ');
                cursor += 1;
            }
            b'%' if cursor + 2 < bytes.len() => {
                let hi = hex_value(bytes[cursor + 1]);
                let lo = hex_value(bytes[cursor + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    output.push((hi << 4) | lo);
                    cursor += 3;
                } else {
                    output.push(bytes[cursor]);
                    cursor += 1;
                }
            }
            byte => {
                output.push(byte);
                cursor += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn path_only(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn header_value<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn error_response(error: ApiError) -> ApiResponse {
    ApiResponse::json(
        error.status,
        json!({
            "error": {
                "message": error.message,
                "type": if error.status >= 500 {
                    "server_error"
                } else {
                    "invalid_request_error"
                }
            }
        }),
    )
}

fn respond(request: Request, response: ApiResponse) {
    let content_type = Header::from_bytes("Content-Type", response.content_type)
        .expect("static Content-Type is ASCII");
    let response = Response::from_data(response.body)
        .with_status_code(StatusCode(response.status))
        .with_header(content_type);
    if let Err(e) = request.respond(response) {
        tracing::warn!("failed to write HTTP response: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_multipart_upload() {
        let boundary = "flov-test-boundary";
        let body = [
            format!("--{boundary}\r\n").into_bytes(),
            b"Content-Disposition: form-data; name=\"language\"\r\n\r\nru\r\n".to_vec(),
            format!("--{boundary}\r\n").into_bytes(),
            b"Content-Disposition: form-data; name=\"file\"; filename=\"voice.wav\"\r\n".to_vec(),
            b"Content-Type: audio/wav\r\n\r\n".to_vec(),
            vec![0, 1, 2, 0xff, 3],
            format!("\r\n--{boundary}--\r\n").into_bytes(),
        ]
        .concat();

        let parts = parse_multipart(&body, boundary).unwrap();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "language");
        assert_eq!(parts[0].data, b"ru");
        assert_eq!(parts[1].name, "file");
        assert_eq!(parts[1].filename.as_deref(), Some("voice.wav"));
        assert_eq!(parts[1].data, vec![0, 1, 2, 0xff, 3]);
    }

    #[test]
    fn decodes_pcm_wav_and_resamples_to_whisper_rate() {
        let wav = test_wav(48_000, 4_800);

        let decoded = decode_audio(&wav, Some("test.wav"), Some("audio/wav"), 60).unwrap();

        assert!((decoded.duration_seconds - 0.1).abs() < 0.001);
        assert_eq!(decoded.samples.len(), 1_600);
        assert!(decoded.samples.iter().any(|sample| sample.abs() > 0.1));
    }

    #[test]
    fn rejects_invalid_language() {
        assert!(validate_language(Some("../../ru")).is_err());
        assert_eq!(validate_language(Some("ru-RU")).unwrap().unwrap(), "ru-RU");
    }

    #[test]
    fn query_percent_decoding_works() {
        let query = parse_query("/v1/audio/transcriptions?language=pt-BR&x=hello+world");

        assert_eq!(query_value(&query, "language").as_deref(), Some("pt-BR"));
        assert_eq!(query_value(&query, "x").as_deref(), Some("hello world"));
    }

    #[test]
    fn recording_routes_are_unavailable_without_microphone_loop() {
        // Headless mode must not pretend to control a microphone cycle.
        let error = require_recording(false).expect_err("headless must refuse recording");
        assert_eq!(error.status, 503);
        assert!(error.message.contains("/v1/audio/transcriptions"));

        require_recording(true).expect("desktop supports the recording cycle");
    }

    #[test]
    fn health_reports_recording_false_in_headless_mode() {
        // The mode byte can flip (a desktop /v1/recording/start stores
        // MODE_TRANSCRIBE), but headless health must stay false.
        assert!(!health_recording(false, hotkey::MODE_TRANSCRIBE));
        assert!(!health_recording(false, hotkey::MODE_IDLE));

        assert!(!health_recording(true, hotkey::MODE_IDLE));
        assert!(health_recording(true, hotkey::MODE_TRANSCRIBE));
    }

    #[test]
    fn error_bodies_keep_openai_error_shape() {
        let server_error: serde_json::Value =
            serde_json::from_slice(&error_response(ApiError::new(503, "busy")).body).unwrap();
        assert_eq!(server_error["error"]["type"], "server_error");
        assert_eq!(server_error["error"]["message"], "busy");

        let client_error: serde_json::Value =
            serde_json::from_slice(&error_response(ApiError::new(401, "denied")).body).unwrap();
        assert_eq!(client_error["error"]["type"], "invalid_request_error");
    }

    fn test_wav(sample_rate: u32, sample_count: usize) -> Vec<u8> {
        let data_len = (sample_count * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for index in 0..sample_count {
            let phase = index as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32;
            let sample = (phase.sin() * i16::MAX as f32 * 0.5) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }
}
