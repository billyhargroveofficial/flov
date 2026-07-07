# 001 - Flov Stability Hardening Research Pass

Status: current
Created: 2026-07-07
Owner: implementation Codex chat after `/goal`

## Request

Run a deep stability audit of Flov, use 6 native Codex subagents, check the
Billyharness loop-development process in `D:\repos\billyharness`, and turn the
research into a large actionable TODO plus a copy-ready implementation prompt.

## Source Research Summary

### Native Codex Subagents Launched

All workers were read-only. No Playwright, Puppeteer, Chrome MCP, headless
Chrome/Edge, screenshots, network capture, or browser debugging was used.

- Hypatia: Tauri UI lifecycle, Settings window, WebView2, pill reload state.
- Euler: audio recorder, global hotkey, input/paste stability.
- Kant: sidecar Whisper backends, model management, postprocess.
- Sartre: Svelte pill/settings state, event ordering, frontend races.
- Volta: Windows/macOS build, packaging, release reproducibility.
- Avicenna: Billyharness loop-develop workflow and Flov adaptation.

### Local Verification Evidence

Commands run from `D:\CProjs\flov`:

```powershell
cargo check
cargo test
cargo clippy --all-targets --all-features
npm run check --prefix ui
npm run build --prefix ui
git diff --check
.\scripts\build-bundle.ps1 -SkipSidecars
7z l target\release\bundle\nsis\flov_0.2.3_x64-setup.exe
```

Results:

- `cargo check`: passed.
- `cargo test`: passed, 14 Rust tests.
- `cargo clippy --all-targets --all-features`: passed.
- `npm run check --prefix ui`: passed, 0 Svelte errors/warnings.
- `npm run build --prefix ui`: passed.
- `git diff --check`: passed.
- Windows bundle smoke passed and produced
  `target\release\bundle\nsis\flov_0.2.3_x64-setup.exe`, 423,483,134 bytes
  (about 403.9 MB).
- Installer contents included fresh `flov_app.exe` from 2026-07-07, but sidecar
  binaries from 2026-05-14 and CUDA runtime DLLs from 2026-02-10. This confirms
  `-SkipSidecars` can package stale ignored sidecars.
- `git status --short --branch` was clean before this TODO was created.
- `HEAD` is `b7a9a79`, tagged `0.2.3`, and `origin/main` matches.

Runtime observation:

- A user-installed `%LOCALAPPDATA%\flov\flov_app.exe` was already running, so a
  second release instance was not started to avoid competing global hotkeys and
  tray state.
- The live log shows recent recording/transcription cycles working through CUDA.
- The live log also contains historical instability evidence:
  - one `watchdog: is_recording stuck true with mode=IDLE` on 2026-05-15;
  - repeated WebView2 `HRESULT(0x8007139F)` Settings creation failures on
    2026-05-18 through 2026-05-20.
- The live config contains an OpenRouter key; do not echo secrets in logs,
  reports, UI errors, or tests.

### Billyharness Loopdev Pattern

Relevant files checked in `D:\repos\billyharness`:

- `AGENTS.md`
- `loop-develop/README.md`
- `loop-develop/current-todo/010-todo.md`
- `loop-develop/history/001-todo.md`
- `loop-develop/history/003-todo.md`
- `loop-develop/history/009-todo.md`

Rules to mirror for Flov:

- tactical TODOs and copy-ready goal prompts live in `loop-develop`, not `docs`;
- `current-todo/` contains the active TODO, `history/` contains completed and
  verified TODOs;
- filenames are `NNN-todo.md`, scanning both current and history before picking
  the next number;
- a TODO contains research summary, checklist, target files, architecture
  boundaries, verification commands, and a copy-ready `/goal`;
- implementation goal prompts must ask the agent to verify, commit, and push;
- completed TODOs move to `history` only after verification, preserving evidence.

## Architecture Canon For This Loop

- Do not reintroduce eager hidden Settings creation.
- Settings must be created lazily through `ui::open_settings_window`.
- On Windows, Settings must not share the main pill WebView2 data directory when
  browser environment settings differ.
- Keep `window.eval("void 0")` validation before reusing an existing Settings
  window.
- Do not hide the main Windows pill HWND with `window.hide()` in idle.
- Frontend listeners must be registered before `pill_frontend_ready`.
- Periodic WebView reload must remain gated by overlay activity, recording
  cycle, hotkey mode, and quiet time.
- Frontend verification starts with code, route config, imports, API calls,
  built assets, and public files.
- No browser automation/debug tools without explicit user command.
- Do not revert unrelated user changes.

## P0 Milestone 1 - Backend Selection Must Never Pick A Broken Auto Backend

Finding: `available_backends()` filters CUDA through `cuda_runtime_present()`,
but `resolve_sidecar("auto")` only checks binary existence. A full Windows
bundle can include `flov-whisper-cuda.exe` on a non-NVIDIA machine, so UI may
show CUDA unavailable while real transcription still picks CUDA.

Target files:

- `src-tauri/src/transcribe.rs`
- `src-tauri/src/recording.rs`
- `src-tauri/src/state_cmd.rs`
- `scripts/build-bundle.ps1`

Checklist:

- [ ] Problem: `auto` can choose CUDA even when CUDA runtime is absent. Make
      backend discovery and sidecar resolution share one source of truth.
- [ ] Problem: explicit stale config can fail unclearly. Return actionable
      errors for explicit unavailable backend choices.
- [ ] Problem: a GPU backend can exist but fail at runtime. Add structured
      sidecar runner errors and fallback to the next auto backend for spawn,
      missing runtime, and GPU init failures.
- [ ] Problem: corrupt/missing model should not silently fallback across
      backends. Classify model errors separately and stop with a clear message.
- [ ] Problem: CUDA packaging can be runtime-broken. If CUDA is included in a
      bundle, hard-fail missing cuBLAS staging instead of warning.

Suggested tests:

- auto skips CUDA when `nvcuda.dll` is unavailable;
- explicit CUDA without runtime returns a clear error;
- fake sidecar nonzero runtime error falls back in auto;
- fake corrupt model error does not fallback;
- packaging preflight fails if CUDA requested but cuBLAS DLLs are missing.

## P0 Milestone 2 - Pill Reload And WebView2 Lifecycle Must Be Race-Resistant

Finding: the main window is configured `visible: false`; setup sets alpha 0 and
click-through but does not show the HWND until first hotkey. This weakens the
"never keep main WebView2 hidden/backgrounded" strategy. The periodic reloader
also has a check-to-eval race and can leave `FRONTEND_RELOAD_IN_PROGRESS` stuck.

Target files:

- `src-tauri/src/ui.rs`
- `src-tauri/src/recording.rs`
- `src-tauri/src/lib.rs`
- `ui/src/routes/+page.svelte`

Checklist:

- [ ] Problem: cold-start pill begins OS-hidden. On Windows, show the main
      window once during setup after alpha 0 and click-through are applied, then
      repaint; keep visual alpha 0 to avoid flash.
- [ ] Problem: reloader can pass gates, then hotkey starts before
      `location.reload()`. Extract reload gating into a testable state machine
      with a lease/CAS and a second gate-check immediately before eval.
- [ ] Problem: reload flag can stay true forever after successful eval if
      frontend never reports ready. Add timestamped stale recovery and clear or
      retry without imposing a permanent 750 ms hotkey penalty.
- [ ] Problem: stale `pill_frontend_ready` snapshot can overwrite a newer
      frontend event. Add state version/event sequence to snapshot application.
- [ ] Problem: high-frequency `audio-spectrum` broadcasts to all windows. Emit
      pill events to the main window, and settings/model/stats events only where
      needed.

Suggested tests:

- pure unit tests for reload gate state;
- "activity starts after first gate" cancels reload before eval;
- reload started -> ready clears;
- reload started -> timeout recovers;
- frontend reducer/fake timer test for snapshot/event ordering;
- static guard against `window.hide()` for the main pill.

## P0 Milestone 3 - Recording Must Recover From Device Loss And Lost Keyup

Finding: `AudioRecorder` is created once at startup and holds a specific
`cpal::Device` plus stream config. Settings changes apply only after restart.
Stream errors only `eprintln!`. The watchdog handles `is_recording=true` with
`mode=IDLE`, but not `active_mode=MODE_TRANSCRIBE` stuck forever after a lost
keyup or hook/tap transition.

Target files:

- `src-tauri/src/audio.rs`
- `src-tauri/src/recording.rs`
- `src-tauri/src/hotkey.rs`
- `src-tauri/src/state_cmd.rs`
- `ui/src/lib/settings/MicPicker.svelte`

Checklist:

- [ ] Problem: unplugged/default-changed mic can wedge recording until restart.
      Introduce an `AudioCaptureManager` that resolves the selected/default
      device per recording or after stream errors.
- [ ] Problem: stream errors are invisible. Send `cpal::StreamError` through a
      channel/atomic outcome and surface a concise pill/settings error.
- [ ] Problem: lost keyup can keep `active_mode=MODE_TRANSCRIBE`. Add max
      recording duration and a state watchdog that can reset a physically
      impossible/too-long active mode.
- [ ] Problem: audio callback does locking and unbounded Vec growth. Move toward
      bounded buffering and no-allocation callback work.
- [ ] Problem: input sample conversion is too narrow. Support common CPAL sample
      formats beyond `F32` and `I16`, and add safer 48 kHz -> 16 kHz resampling
      strategy or closest-native-config selection.

Suggested tests:

- recording loop clears state on stream error;
- lost keyup/max duration returns to idle;
- no-model/error paths do not wait forever for release;
- sample conversion for `U16`, `I32`, `F64`;
- resample duration and anti-alias smoke tests;
- hotkey repeat/release/combo-swap state-machine tests.

## P0 Milestone 4 - Packaging Must Be Explicit, Reproducible, And Honest

Finding: script/docs disagree about CUDA default. `build-bundle.ps1` enables
CUDA by default and supports `-NoCuda`; README/CLAUDE mention `-IncludeCuda`
and describe default CPU+Vulkan. `-SkipSidecars` packaged old sidecars from
May 2026 into a fresh July 2026 installer.

Target files:

- `scripts/build-bundle.ps1`
- `scripts/build-sidecars.ps1`
- `scripts/build-bundle.sh`
- `scripts/build-sidecars.sh`
- `README.md`
- `CLAUDE.md`
- `AGENTS.md`
- `.gitignore`

Checklist:

- [ ] Problem: CUDA default is ambiguous. Choose one policy, then align script
      flags and docs (`-NoCuda` vs `-IncludeCuda`) everywhere.
- [ ] Problem: stale sidecars can ship silently. Add sidecar artifact manifest
      with backend, source git SHA or build timestamp, size, hash, and target
      triple; fail bundle if manifest/source expectations are stale.
- [ ] Problem: release reproducibility is manual. Add release preflight for clean
      tree, version/tag match, `Cargo.lock`, `npm ci`, and `--locked` cargo
      builds.
- [ ] Problem: external runtime download is unpinned. Pin/hash or at least record
      the downloaded `vc_redist` version/checksum in the build output.
- [ ] Problem: macOS production release is ad-hoc only. Keep ad-hoc documented,
      but add a separate Developer ID/notarization lane and post-build checks.

Suggested verification:

- inspect NSIS contents for no-CUDA and CUDA builds;
- fail CUDA build if cublas DLLs are missing;
- check sidecar manifest against installer contents;
- macOS arm64 ad-hoc codesign verification;
- production notarization lane when certs exist.

## P1 Milestone - Model/Postprocess/Input User-Facing Failure Handling

Target files:

- `src-tauri/src/models.rs`
- `src-tauri/src/models_cmd.rs`
- `src-tauri/src/postprocess.rs`
- `src-tauri/src/input.rs`
- `src-tauri/src/recording.rs`
- `ui/src/lib/settings/Models.svelte`
- `ui/src/lib/settings/Postprocess.svelte`

Checklist:

- [ ] Problem: downloaded model validity is only `exists()`. Validate model files
      by `is_file`, expected size, and preferably checksum before marking
      downloaded or active.
- [ ] Problem: partial or HTML/error-body download can become final model. Use a
      unique temp file, verify bytes, delete `.part` on error, then atomic rename.
- [ ] Problem: long transcription/OpenRouter calls cannot be canceled. Add a
      cancel path that kills child sidecars and skips postprocess on user cancel
      or next hotkey intent.
- [ ] Problem: postprocess failures are log-only and raw paste is silent. Show
      "cleanup failed, pasted raw" and store last safe error status for Settings.
- [ ] Problem: paste failures are invisible. Make `input::type_text` return
      `Result<()>`, check `SendInput` counts, retry clipboard opens, and surface
      macOS Accessibility/UIPI-style failures.

## P1 Milestone - Settings Frontend Concurrency And Accessibility

Target files:

- `ui/src/routes/+page.svelte`
- `ui/src/lib/settings/Postprocess.svelte`
- `ui/src/lib/settings/Models.svelte`
- `ui/src/lib/settings/MicPicker.svelte`
- `ui/src/lib/settings/HotkeyControl.svelte`
- `ui/src/lib/settings/Stats.svelte`

Checklist:

- [ ] Problem: `Postprocess.save()` can clobber edits typed during in-flight
      save. Capture payloads, use pending flags, and refresh only if the form is
      still at the saved version.
- [ ] Problem: `Postprocess.toggle()` can race on double-click. Add pending
      lock and backend-confirmed state assignment.
- [ ] Problem: hidden delete button is still tab-reachable. Reveal action groups
      on `:focus-within` or use keyboard-visible controls.
- [ ] Problem: custom mic listbox lacks full semantics. Add `aria-controls`,
      `role=option`, `aria-selected`, arrow-key navigation, and escape handling.
- [ ] Problem: Settings async refresh/listen cleanup can update after close. Add
      mounted guards and `.catch` paths.
- [ ] Problem: Stats `today` freezes at mount. Refresh date boundaries or derive
      from a timer when Settings stays open over midnight.

## Architecture Boundaries

- Do not use browser automation/debug tooling unless Billy explicitly asks.
- Keep changes small and aligned with existing Rust/Svelte/Tauri patterns.
- Do not rewrite the app into a new framework.
- Do not introduce a database or service daemon.
- Do not move stable architecture docs into `loop-develop`; update `docs/` only
  for durable platform/build/runtime behavior changes.
- Do not log raw OpenRouter keys, full user transcripts in new error paths, or
  model download URLs with secrets.
- Do not make Settings eager, shared-profile, or hidden-at-start.
- Do not call `window.hide()` on the Windows main pill idle path.

## Verification Commands

Focused commands while developing:

```powershell
cargo test -p flov_app --lib
cargo test
npm run check --prefix ui
npm run build --prefix ui
git diff --check
```

Before calling Windows work done:

```powershell
cargo check
cargo test
cargo clippy --all-targets --all-features
npm run check --prefix ui
npm run build --prefix ui
git diff --check
.\scripts\build-bundle.ps1 -SkipSidecars
```

If release/build scripts are touched:

```powershell
git status --porcelain
git tag --points-at HEAD
7z l target\release\bundle\nsis\flov_0.2.3_x64-setup.exe
```

If versioning is touched, verify all match:

- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`
- `Cargo.lock`
- release tag

Manual matrix to run when practical:

- Windows: mic unplug during recording, sleep/wake, long idle, Settings open
  after long session, RCtrl hotkey, no-NVIDIA machine with CUDA sidecar present,
  elevated target paste.
- macOS: Accessibility denied/granted, Microphone denied/granted, AirPods or
  external mic loss, Cmd+Tab during hotkey, unsigned/ad-hoc TCC reset after
  rebuild.

## Copy-Ready Goal Prompt

```text
/goal Implement loop-develop/current-todo/001-todo.md end to end in D:\CProjs\flov. This is a stability hardening loop: work P0 milestones first and keep changes conservative. Preserve Flov AGENTS.md hard rules: do not use Playwright, Puppeteer, Chrome MCP, headless Chrome/Edge, screenshots, browser network capture, or browser debug without explicit user command; validate frontend changes through code, route config, imports, API calls, built assets, and public files. Fix backend auto-selection so it cannot choose CUDA when CUDA runtime is unavailable, add structured sidecar fallback for auto-mode runtime failures, harden pill/WebView2 reload state against cold-start and stuck-reload races, add recording recovery for device loss/lost keyup, and make Windows packaging CUDA policy/stale-sidecar checks explicit. Keep Settings lazy, keep Settings on a dedicated WebView2 data dir, keep main pill idle without window.hide(), and preserve listener-before-pill_frontend_ready ordering. Update durable docs only if platform behavior, build flags, release flow, config, or architecture invariants change. Work with the existing dirty worktree without reverting unrelated user changes. Verify with the TODO commands, then create a git commit and push the branch after verification passes. Do not move the TODO to history; leave archival for the main verification chat.
```

## Follow-Up Candidates

- Split P1 model/postprocess/input into `002-todo.md` if P0 gets too large.
- Split Settings accessibility/concurrency into `003-todo.md`.
- Add a real CI matrix for Windows no-CUDA, Windows CUDA, and macOS arm64.
- Add signed/notarized macOS release lane after Developer ID credentials exist.

## Final Status

Status: implemented; pending archival by the main verification chat.

Implementation branch: `codex/stability-hardening-loop-001`
Implementation commit: branch tip commit from this implementation chat.
Push state: pushed to `origin/codex/stability-hardening-loop-001`.

Commands run:

```text
cargo test -p flov_app --lib
cargo check
cargo test
cargo clippy --all-targets --all-features
npm run check --prefix ui
npm run build --prefix ui
git diff --check
.\scripts\build-bundle.ps1
.\scripts\build-bundle.ps1 -SkipSidecars
git status --porcelain
git tag --points-at HEAD
7z l target\release\bundle\nsis\flov_0.2.3_x64-setup.exe
```

Remaining blockers:

- Runtime manual matrix still needs physical/platform passes.
- CUDA `-IncludeCuda` packaging lane was made strict but not executed in this
  implementation chat because the default verified bundle is now CPU+Vulkan.
