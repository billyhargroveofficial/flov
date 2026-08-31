# Transcription sidecars

Each backend (CUDA / Vulkan / Metal / CPU) lives in its own Cargo crate that
compiles to a separate binary `flov-whisper-<backend>(.exe)`. The main app
(`flov.exe`) does not link `whisper-rs` itself — it spawns whichever sidecar
the user picked and pipes audio through stdin.

## Why separate workspaces

Each `crates/flov-whisper-*/Cargo.toml` declares an empty `[workspace]` so it
is **not** a member of the parent workspace. This is deliberate: Cargo would
otherwise unify `whisper-rs` features across crates and pull `cuda` into the
CPU sidecar (turning the "CPU build" into a CUDA build with extra DLL
dependencies). The parent workspace also has `exclude = ["crates/*"]` to make
the boundary explicit.

## Wire protocol

The same protocol is used by every sidecar. Source of truth:
`crates/flov-whisper-cpu/src/main.rs`.

```
args:   --model <path> --language <code>
stdin:  raw f32 LE PCM, 16 kHz mono. Parent closes stdin to signal end.
stdout: transcribed text (trimmed, no trailing newline).
stderr: human-readable progress / errors. Never on stdout.
exit:   0 on success, 1 on failure.
```

If you add a new backend, copy the existing CPU sidecar verbatim and flip the
`whisper-rs` feature flag in `Cargo.toml`. There is no other code change.

## Backend selection at runtime

`flov_app` resolves which sidecar to spawn for each transcription:

1. `FLOV_BACKEND=<name>` env var, if set, wins (debug knob).
2. `[backend].choice` in `flov.toml` (set by tray menu "Backend" radio).
3. Default `auto` walks `BACKEND_PRIORITY = [cuda, vulkan, metal, cpu]` and
   uses the first sidecar present next to `flov.exe`.

Missing sidecars are greyed-out in the tray menu, so the user can't pick one
that won't run.

## Building

`scripts/build-sidecars.ps1` builds every `crates/flov-whisper-*` it finds
and stages the binary (plus required runtime DLLs) into both
`target/debug/` and `target/release/`. It accepts `-Backend cuda` to limit
to one.

The CUDA build needs a working CUDA toolchain (env vars are pre-set in
`.cargo/config.toml`). The Vulkan build needs `VULKAN_SDK` to point at a
LunarG SDK install.

## Adding the Metal sidecar (macOS, Apple Silicon)

`crates/flov-whisper-metal/` doesn't exist yet — these are the steps to
create and ship it on a Mac with Xcode installed.

### 1. Create the crate

```bash
mkdir -p crates/flov-whisper-metal/src
```

`crates/flov-whisper-metal/Cargo.toml`:

```toml
[package]
name = "flov-whisper-metal"
version = "0.1.0"
edition = "2021"
description = "Metal Whisper transcription sidecar for flov (Apple Silicon)"

[workspace]

[[bin]]
name = "flov-whisper-metal"
path = "src/main.rs"

[dependencies]
# `metal` enables the Metal backend in whisper.cpp.
# Add "coreml" too if you want ANE (Apple Neural Engine) acceleration
# — but that requires extra .mlmodelc files alongside the .bin model.
whisper-rs = { version = "0.16", features = ["metal"] }
anyhow = "1"
num_cpus = "1"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

`crates/flov-whisper-metal/src/main.rs`: copy `crates/flov-whisper-cpu/src/main.rs`
verbatim and rename strings/error prefixes from `cpu` to `metal`.

### 2. Build

No env vars needed — Metal headers ship with macOS. From repo root:

```bash
cargo build --release \
  --manifest-path crates/flov-whisper-metal/Cargo.toml \
  --target-dir target
```

### 3. Stage

```bash
cp target/release/flov-whisper-metal target/release/flov-whisper-metal
# also into target/debug/ if you run `tauri dev` on Mac
```

For packaged builds the binary needs to land next to `flov` inside the
`.app` bundle. Tauri's `externalBin` in `tauri.conf.json` is the supported
path — add an entry once we wire bundler config.

### 4. Verify

Start `flov`, check `flov.log`:

```
INFO flov_lib: available backends: ["metal", "cpu"]; configured choice: auto
INFO flov_lib::transcribe: transcribe via metal (...)
```

Metal will be picked automatically because it is ahead of `cpu` in
`BACKEND_PRIORITY`. Tray menu's "Metal (Apple Silicon)" item should be
enabled.

### Optional: CoreML for ANE acceleration

Add `coreml` to features. CoreML expects `<model>.mlmodelc` next to the
`.bin` (e.g. `ggml-large-v3-turbo.bin` + `ggml-large-v3-turbo-encoder.mlmodelc/`).
The whisper.cpp repo has scripts to generate these from a Whisper model.
First inference with CoreML compiles the model (slow, one-off), then ANE
takes over.

## Decoding tuning (`flov-tuning`)

Every sidecar links `crates/flov-tuning`, a small shared library so the four
backends cannot drift apart. All of it is opt-in through environment
variables: unset, a sidecar decodes exactly as it did before.

| Variable | Default | Effect |
| --- | --- | --- |
| `FLOV_WHISPER_VAD_MODEL` | unset | Path to a Silero ggml model (`ggml-silero-v5.1.2.bin`). Enables the VAD prefilter. |
| `FLOV_WHISPER_VAD_THRESHOLD` | `0.5` | Speech probability threshold. Lower keeps laughter and interjections. |
| `FLOV_WHISPER_VAD_SPEECH_PAD_MS` | `30` | Padding around each speech segment. |
| `FLOV_WHISPER_VAD_MIN_SPEECH_MS` | `250` | Shorter bursts are discarded. |
| `FLOV_WHISPER_VAD_MIN_SILENCE_MS` | `100` | Silence needed to split segments. |
| `FLOV_WHISPER_VAD_SAMPLES_OVERLAP` | `0.1` | Seconds of overlap carried into the next segment. |
| `FLOV_WHISPER_VAD_GPU` | `0` | Run VAD on the GPU. |
| `FLOV_WHISPER_PROMPT` | unset | Whisper's initial prompt. Biases domain spelling. |
| `FLOV_WHISPER_QUALITY` | `0` | Force the beam-search long-form decode on sub-window clips too. |
| `FLOV_WHISPER_SUPPRESS_NST` | `0` | Suppress non-speech tokens. |
| `FLOV_WHISPER_NO_SPEECH_THOLD` / `_ENTROPY_THOLD` / `_LOGPROB_THOLD` | whisper.cpp defaults | Decoder fallback thresholds. |

### Why VAD runs here and not in whisper.cpp

whisper.cpp applies `whisper_full_params.vad` inside `whisper_full()`. The
sidecars decode through `whisper_full_with_state()`, which skips that stage
entirely — setting `params.vad` there is silently ignored. `flov-tuning`
therefore runs Silero itself and feeds the decoder only the speech, mirroring
upstream's segment stitching (overlap on every segment but the last, 100 ms of
silence between them).

This matters because a clip with no speech does not produce an empty
transcript: whisper fills the silence with subtitle credits from its training
data ("Субтитры сделал DimaTorzok", "Продолжение следует..."). With the VAD
prefilter the sidecar prints nothing and the caller decides what to do.

### A note on `FLOV_WHISPER_QUALITY`

Beam search is not free accuracy. On a 3.7 s noisy clip it produced a
hallucinated subtitle credit where the greedy fast path transcribed correctly,
so the flag stays off by default. Measure on your own corpus before enabling.
