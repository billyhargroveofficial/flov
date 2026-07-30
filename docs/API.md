# Flov transcription API

Flov exposes an OpenAI-compatible, non-streaming audio transcription
endpoint:

```text
POST /v1/audio/transcriptions
```

The server uses the same selected model/backend as the desktop application.
Desktop and API requests share one inference lock so two CUDA/Vulkan model
loads cannot race each other.

## Quick start

The default listener is `127.0.0.1:17432`:

```bash
curl -sS http://127.0.0.1:17432/health

curl -sS http://127.0.0.1:17432/v1/audio/transcriptions \
  -F model=flov-whisper \
  -F language=ru \
  -F response_format=json \
  -F file=@speech.wav
```

Response:

```json
{"text":"Распознанный текст."}
```

`model` is accepted for OpenAI client compatibility; Flov uses the active
model selected in Settings.

Supported `response_format` values:

- `json` (default): `{"text":"..."}`;
- `verbose_json`: includes `task`, `language`, `duration`, `text`, and an
  empty `segments` array;
- `text`: UTF-8 `text/plain`.

The optional Flov extension `postprocess=true` sends the transcript through
the OpenRouter cleanup configured in Settings:

```bash
curl -sS http://127.0.0.1:17432/v1/audio/transcriptions \
  -F file=@speech.mp3 \
  -F postprocess=true \
  -F response_format=text
```

Decoded formats include PCM/float WAV, MP3, FLAC, Ogg/Vorbis, AAC/M4A,
ALAC, CAF, and other codecs supported by the bundled Symphonia feature set.
WebM/Opus is not currently decoded; convert it to WAV, FLAC, MP3, or Ogg
Vorbis before upload.

## Raw request body

Multipart is the portable/OpenAI-compatible form. A single encoded audio
file can also be sent as the complete request body:

```bash
curl -sS \
  -H 'Content-Type: audio/wav' \
  -H 'X-Filename: speech.wav' \
  --data-binary @speech.wav \
  'http://127.0.0.1:17432/v1/audio/transcriptions?language=ru&response_format=text'
```

This is still an encoded audio container, not headerless PCM.

## Configuration and LAN access

The config lives at `~/.local/share/flov/flov.toml` on Linux:

```toml
[server]
enabled = true
bind = "127.0.0.1:17432"
api_key = ""
max_body_mb = 25
max_audio_seconds = 600
postprocess = false
```

For another machine on the LAN:

```toml
[server]
enabled = true
bind = "0.0.0.0:17432"
api_key = "replace-with-a-long-random-token"
max_body_mb = 25
max_audio_seconds = 600
postprocess = false
```

Flov refuses to bind a non-loopback address without `api_key`. Restart the
application after changing server settings, then call it with:

```bash
curl -sS http://HOST:17432/v1/audio/transcriptions \
  -H 'Authorization: Bearer replace-with-a-long-random-token' \
  -F file=@speech.wav
```

This server is plain HTTP. For access beyond a trusted LAN, put it behind a
TLS reverse proxy or an authenticated VPN; do not expose the port directly
to the internet.

## Other endpoints

```text
GET  /health
GET  /v1/health
GET  /v1/models
GET  /v1/recording
POST /v1/recording/start
POST /v1/recording/stop
```

`/health` and `/v1/health` are intentionally unauthenticated and report
whether the process and model are ready. All other routes require the bearer
token when `server.api_key` is configured.

The recording routes control the local microphone cycle; they exist
primarily for Hyprland press/release bindings. Uploaded files only return
text and never inject it into the focused application.

## OpenAI SDK shape

Point an OpenAI-compatible client at:

```text
base URL: http://127.0.0.1:17432/v1
API key:  any non-empty placeholder when Flov has no server.api_key,
          otherwise the configured token
model:    flov-whisper
```

Only the audio transcription and model-list subset is implemented; this is
not a general chat/completions server.
