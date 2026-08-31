# Flov - Voice to Text Assistant

Push-to-talk на Windows и macOS (Apple Silicon): зажимаешь хоткей,
говоришь, отпускаешь — текст вставляется в активное поле через буфер
обмена (Ctrl+V на Windows / Cmd+V на маке).

Дефолтный хоткей: `Ctrl+Win` на Windows, `Cmd+Alt` на macOS — выбран
через `cfg(target_os)` в `config.rs::default_hotkey_combo()`.

UI: Tauri 2 webview с Svelte 5. Frontend рендерит Mac-style капсулу-pill у
курсора с морф-анимацией. Backend: Rust, платформенный keyboard hook
(Win32 `WH_KEYBOARD_LL` / macOS `CGEventTap` / Linux `evdev`) + cpal для
записи + spawn sidecar для транскрипции.

## Layout

```
flov/
├── Cargo.toml          # workspace root: members = ["src-tauri"], exclude = ["crates/*"]
├── AGENTS.md           # hard rules + gotchas for future coding agents
├── .cargo/config.toml  # CMake/CUDA env для whisper.cpp build (нужно для CUDA sidecar)
├── flov.toml           # дев-конфиг (gitignored), копия в target/debug/
├── dev.cmd             # one-click рестарт: PATH+=cargo, tauri dev из root
├── docs/DESIGN.md      # дизайн-нотки по pill / settings
├── src-tauri/          # main app (flov_app.exe)
│   ├── Cargo.toml
│   ├── tauri.conf.json         # cross-platform базовый конфиг (окна, иконки, build)
│   ├── tauri.windows.conf.json # Windows-only: NSIS bundle, externalBin cpu+vulkan
│   ├── tauri.macos.conf.json   # macOS-only: .app/.dmg targets, macOSPrivateApi
│   ├── Info.plist              # macOS Info.plist patch — NSMicrophoneUsageDescription + LSUIElement
│   │                         # tauri.conf declares only the main pill window;
│   │                         # settings window is created lazily from Rust
│   ├── icons/          # tray.png (32x32, чёрный глиф; tray.rs.invert на dark theme)
│   ├── capabilities/   # Tauri 2 permissions (settings.json — для settings window)
│   └── src/
│       ├── main.rs        # entry, вызывает flov_lib::run
│       ├── lib.rs         # оркестрация: загрузка config, инициализация Recorder/Transcriber/HotkeyHook,
│       │                  # Tauri Builder, manage state, spawn runtime workers
│       ├── recording.rs   # runtime loop: hotkey → record → transcribe → postprocess → paste,
│       │                  # watchdog, periodic pill webview reload, recording-cycle guard
│       ├── audio.rs       # WASAPI/CoreAudio запись через cpal, ресемплинг 16kHz, FFT-спектр для оверлея
│       ├── hotkey.rs      # глобальный keyboard hook (Win32 WH_KEYBOARD_LL / macOS CGEventTap / Linux evdev),
│       │                  # парсер combo строк (Ctrl+Win, Cmd+Alt, RCtrl и т.д.), live re-bind
│       ├── input.rs       # вставка через clipboard + paste hotkey (Win32 SendInput Ctrl+V /
│       │                  # macOS CGEventPost Cmd+V + arboard / Linux wl-copy + wtype)
│       ├── paths.rs       # user data dir: Win=exe_dir, macOS=~/Library/Application Support/com.flov.app/,
│       │                  # Linux=$XDG_DATA_HOME/flov/. flov.toml/stats.json/models/ всё через него
│       ├── transcribe.rs  # spawn sidecar (Command::new), pipe f32 PCM в stdin, читает stdout
│       ├── postprocess.rs # OpenRouter API (опционально, тогл из settings); логирует HTTP status/body
│       ├── config.rs      # flov.toml parser, surgical write через toml_edit (сохраняет коментарии)
│       ├── tray.rs        # Tauri 2 native tray (Open Settings / Quit), theme-aware иконка
│       │                  # на Windows (registry poll), template image на macOS (auto-tint)
│       ├── ui.rs          # Tauri commands + per-platform window helpers
│       │                  # (position_at_cursor_monitor: Win32 MonitorFromPoint / macOS CGDisplay)
│       ├── models.rs      # каталог Whisper моделей (tiny..large-v3-turbo)
│       ├── models_cmd.rs  # Tauri commands для скачивания / выбора модели
│       ├── state_cmd.rs   # Tauri commands для backend / postprocess / hotkey / stats settings
│       └── stats.rs       # JSON-лог записей по дням (для heatmap)
├── crates/             # sidecar transcription backends (см. crates/README.md)
│   ├── README.md       # архитектура sidecars + Mac/Metal гайд (исторический)
│   ├── flov-tuning/          # общий VAD-префильтр + env-ручки декодера для всех sidecar'ов
│   ├── flov-whisper-cuda/    # NVIDIA, whisper-rs feature cuda
│   ├── flov-whisper-vulkan/  # AMD/Intel iGPU, whisper-rs feature vulkan
│   ├── flov-whisper-cpu/     # fallback
│   └── flov-whisper-metal/   # Apple Silicon (whisper-rs feature metal)
├── ui/                 # SvelteKit (adapter-static, port 1420)
│   └── src/
│       ├── routes/
│       │   ├── +page.svelte         # main pill window — слушает state-changed/audio-spectrum events
│       │   └── settings/+page.svelte # Settings window — drag-strip + 2-col grid (left: Models+Backend+Stats,
│       │                              # right: Postprocess+Hotkey+Mic)
│       └── lib/
│           ├── Pill.svelte         # capsule с morph transition (вжух scale + width expand),
│           │                       # cross-fade currentColor recording↔transcribing,
│           │                       # hasRevealedRef latch — линия рисуется один раз на mount
│           ├── AudioWave.svelte    # SVG-стек из MAX_LINES=3 path'ей, sin-window pinned at endpoints,
│           │                       # per-line desync (phase/freq/speed), opacity-controlled visibility,
│           │                       # stroke-dashoffset для left-to-right reveal
│           └── settings/           # Models / Backend / Postprocess / Stats компоненты
├── scripts/
│   ├── build-sidecars.ps1   # билдит sidecars на Windows (cpu/vulkan/cuda)
│   ├── build-sidecars.sh    # билдит sidecars на macOS (cpu/metal)
│   ├── build-bundle.ps1     # NSIS installer (Windows)
│   └── build-bundle.sh      # .app + .dmg (macOS Apple Silicon)
├── dev.cmd                  # Windows hot-reload entry
├── dev.sh                   # macOS/Linux hot-reload entry (стейджит sidecars)
└── docs/
    ├── DESIGN.md
    ├── MACOS.md              # детали macOS port (permissions, дефолтный хоткей, paths)
    └── REPORT.md             # Windows reliability investigation / 0.2.2 report
```

## Транскрипция через sidecars

Главное архитектурное решение: транскрипция вынесена в **отдельные бинари
по backend'у**. `flov_app.exe` не знает про whisper-rs / CUDA / Vulkan и не
требует CUDA toolchain для своей сборки. См. `crates/README.md` для полной
картины — wire protocol, build, добавление нового sidecar (Mac/Metal гайд
там же).

Селекция backend'а в `transcribe::resolve_sidecar`:
1. `FLOV_BACKEND` env var (debug override)
2. `[backend].choice` из flov.toml (тогглится из Settings → Backend)
3. `auto` → priority `[cuda, vulkan, metal, cpu]`, первый существующий рядом с exe

При смене backend'а из Settings не нужен рестарт — Transcriber резолвит
sidecar на каждый transcribe call через shared `Arc<Mutex<String>>`.

## UI flow

Threads:
1. **Main** — Win message loop, tray events, Tauri command handlers
2. **Recording loop** (`recording.rs::recording_loop`) — ждёт хоткей, эмитит state events
   (`state-changed: idle|recording|transcribing`), пишет аудио в Recorder,
   вызывает Transcriber, опционально OpenRouter, шлёт текст через `input::type_text`
3. **WebView pill** — Svelte рендерит Pill реагирующий на state events
4. **WebView settings** — создаётся лениво по tray Open Settings, ходит в backend через Tauri commands

Tauri events (backend → pill webview):
- `state-changed: "idle"|"recording"|"transcribing"` → Pill переключает контент
- `audio-spectrum: number[]` (20 bands) → AudioWave амплитуда

Tauri commands (frontend → backend):
- `pill_frontend_ready` — вызывается после регистрации frontend listeners,
  отмечает reload завершённым и возвращает snapshot текущего pill-state
- `hide_window` — после morph-out transition помечает pill логически скрытым
  (OS window не прячем; оно остаётся transparent + click-through)
- `list_models` / `download_model` / `delete_model` / `set_active_model` — модели
- `get_backend_state` / `set_backend_choice` — backend
- `get_postprocess_config` / `set_postprocess_config` / `set_postprocess_enabled`
- `get_hotkey` / `set_hotkey` — хоткей с live re-bind
- `get_stats`

Pill (`Pill.svelte` + `AudioWave.svelte`):
- Recording: 3 переплетающихся wavy линий, амплитуда от FFT mic spectrum
- Transcribing: схлопывается до одной линии в accent цвете (lime на dark, чёрный на light),
  амплитуда sequenced (приглушение → recolor → ramp up)
- Появление: morph (backOut scale pop "вжух" → width expand с overlap),
  линия рисуется через stroke-dashoffset слева направо. Ровно один раз на mount —
  переход recording→transcribing НЕ запускает re-stroke
- Уход: транзишн зеркалится через css(t) reverse

Tray меню (минимальное):
- **Open Settings**
- **Quit**

(модели, backend, post-process, hotkey — всё в Settings window)

Settings window intentionally is **not** declared in `tauri.conf.json`.
On Windows, eagerly creating a hidden transparent settings WebView2 has
repeatedly failed with `HRESULT(0x8007139F)` ("group or resource is not in
the correct state"), leaving tray Open Settings as a silent no-op because
`get_webview_window("settings")` returned `None`. `ui::open_settings_window`
creates it on demand, logs any failure, and the frontend close button uses
`window.close()` so the settings WebView is destroyed instead of sitting in
hidden/suspended state for hours.

Windows WebView2 also requires webviews with different environment settings
to use different data directories. The main pill has custom
`additionalBrowserArgs` for occlusion/background throttling, so lazy Settings
uses a dedicated `webview-settings` data dir. Without that, WebView2 can fail
the second webview with the same `0x8007139F` while Tauri still leaves behind
a blank window wrapper. Existing Settings wrappers are validated with
`window.eval("void 0")`; if the underlying WebView is dead, the wrapper is
closed and recreated.

## Hotkey

Парсер в `hotkey.rs` поддерживает:
- Generic модификаторы: `Ctrl`, `Alt`, `Shift`, `Win` (любая сторона)
- L/R-specific: `LCtrl`, `RCtrl`, `LAlt`, `RAlt`, `LShift`, `RShift`, `LWin`, `RWin`
- Триггер-keys: те же модификаторы (как single-key) + `A-Z`, `0-9`, `Space`, `Enter`,
  `Tab`, `Esc`, `Delete`, `Backspace`

Combo формат: `Ctrl+Win`, `RCtrl`, `Ctrl+Shift+K` и т.д. Последний токен — trigger
(KEYDOWN запускает запись, KEYUP останавливает). Все предыдущие — modifier'ы,
которые должны быть нажаты во время trigger'а.

UI capture (`settings/HotkeyControl.svelte`): keydown буферит pending combo, keyup коммитит.
Это позволяет одинаково записывать и одиночные клавиши (RCtrl), и комбо.

## Сборка

Main app (из root):
```powershell
.\dev.cmd                       # дев с hot reload (PATH+=cargo, tauri dev)
.\ui\node_modules\.bin\tauri.cmd build  # release
```

Sidecars (отдельная команда — workspace excluded):
```powershell
.\scripts\build-sidecars.ps1                  # все backend'ы (release)
.\scripts\build-sidecars.ps1 -Backend cuda    # один
.\scripts\build-sidecars.ps1 -Profile debug   # debug build
```

Скрипт сам стейджит exe + cublas DLLs (для CUDA) в `target/debug` и
`target/release`, чтобы `tauri dev` и packaged build их подхватили.

CUDA build env (уже в `.cargo/config.toml`, не надо ручками):
- `CMAKE_GENERATOR=Ninja`, `CUDAFLAGS=-allow-unsupported-compiler`,
  `CXXFLAGS=/Zc:preprocessor`, `CMAKE_CUDA_FLAGS="-Xcompiler /Zc:preprocessor"`

Vulkan build требует LunarG SDK: `winget install KhronosGroup.VulkanSDK`,
после установки `VULKAN_SDK` подхватывается из system env (новый shell).

## Дистрибуция

### NSIS installer (`scripts/build-bundle.ps1`)

Скрипт строит single-file `flov_<version>_x64-setup.exe`:
- `.\scripts\build-bundle.ps1` — CPU + Vulkan (zero deps на машине user'a)
- `.\scripts\build-bundle.ps1 -IncludeCuda` — добавляет CUDA sidecar
  и cublas DLLs (требует CUDA toolkit при сборке, NVIDIA driver у юзера)

Tauri требует sidecar бинари с triple-суффиксом
(`flov-whisper-cpu-x86_64-pc-windows-msvc.exe`) в
`src-tauri/binaries/`. Скрипт это стейджит автоматически из
`target/release/`. Cublas DLLs идут в `binaries/runtime/` и
конфигурируются через `bundle.resources` в `tauri.conf.json`.

Whisper модель (~1.6 GB) НЕ в installer — пользователь скачает её
через Settings → Models после установки.

### Manual layout (без installer)

Минимум для CUDA-варианта:
```
flov_app.exe
flov-whisper-cuda.exe
cublas64_13.dll
cublasLt64_13.dll
ggml-large-v3-turbo.bin     # или скачивается через Settings
icons/tray.png
flov.toml                    # опционален — Settings создаст
```

Можно класть несколько sidecar бинарей рядом — pick'ается на runtime.
Vulkan/CPU sidecars не требуют дополнительных DLL.

## flov.toml

```toml
[whisper]
model_path = "ggml-large-v3-turbo.bin"  # относительно exe или абсолютный
language = "ru"

[audio]
sample_rate = 16000

[backend]
choice = "auto"  # "auto" | "cuda" | "vulkan" | "metal" | "cpu"

[hotkey]
combo = "Ctrl+Win"  # см. hotkey.rs парсер для синтаксиса

[openrouter]
api_key = "sk-or-..."        # без ключа пост-обработка недоступна
model = "openai/gpt-4o-mini"
system_prompt = "..."
```

Все поля редактируются surgical через toml_edit — комментарии сохраняются.
`audio.sample_rate` фактически должен оставаться 16000: sidecar protocol
принимает raw f32 LE PCM именно 16 kHz mono. `AudioRecorder` всё равно
ресемплит native WASAPI rate к `audio::TRANSCRIBE_SAMPLE_RATE`, а если в
конфиге окажется другое значение — логирует warning и игнорирует его.

## Gotchas / hard-won lessons

Реальные проблемы которые мы уже починили — не наступай повторно при
портировании или рефакторе.

### Chromium occlusion вырубает рендер pill окна (windows)

Симптом: после 2-6 часов uptime юзер жмёт хоткей, backend честно делает
`window.show()` + `emit("state-changed", "recording")`, Svelte листенер
получает событие — **но pill не виден**. Backend продолжает работать
идеально (recordings/transcripts/paste).

Причина: Chromium фича `CalculateNativeWinOcclusion` через COM API
виртуальных рабочих столов раз в N секунд проверяет видимость окна. Если
оно "occluded" — **полностью останавливает рендеринг** (не throttle, STOP)
для экономии GPU. На long uptime COM call может зафейлиться → окно
застряло в OCCLUDED навсегда, пока процесс не перезапустят. Маленькое
frameless transparent always-on-top окно с постоянным hide/show — идеальный
кандидат.

Fix — `additionalBrowserArgs` на pill window в `tauri.conf.json`:
```json
"additionalBrowserArgs": "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling"
```

`msWebOOUI,msPdfOOUI,msSmartScreenProtection` — дефолты wry, которые
переопределяются если задать `additionalBrowserArgs`, поэтому их надо
вернуть. Cross-reference: [Seelen-UI webview.rs](https://github.com/eythaann/Seelen-UI/blob/main/src/background/widgets/webview.rs) делает то же.

**Mac/Linux**: на Mac (WKWebView) и Linux (WebKitGTK) Chromium не
используется — эта конкретная фича отсутствует, но похожие occlusion-
оптимизации возможны (WKWebView имеет `_isOnscreen`). Если на macOS pill
ведёт себя так же — копать в [`WKPreferences`](https://developer.apple.com/documentation/webkit/wkpreferences)
/ window visibility state. На Linux обычно проще: WebKitGTK редко
оптимизирует фоновые окна агрессивно.

### LL keyboard hook затыкается через время (windows)

Симптом: хоткей перестаёт реагировать после неопределённого времени
работы / sleep+wake / "много нажатий подряд".

Корни — несколько слоёв (все в `hotkey.rs::windows_impl`):
- **Hook на dedicated thread** с тайтным `GetMessage` loop. Иначе любой
  hiccup на main thread (Tauri command, lock контеншн) тригерит
  `LowLevelHooksTimeout` (~300ms) и Windows **silently** деинсталлит hook.
- **Periodic reinstall** через `SetTimer(None, 0, 30_000, None)` →
  WM_TIMER → unhook + re-hook каждые 30s. На случай если hook всё-таки
  убили (бывает при sleep/wake, при инжекте анти-чита и т.д.).
- **`RwLock::try_read`** вместо Mutex — hook callback не должен ждать
  ни наносекунды. Запись хоткея из UI берёт write на микросекунды, что
  всё равно может триггерить timeout если бы мы блокировались.
- **`LLKHF_INJECTED` filter** — наш собственный SendInput Ctrl+V (paste)
  fires Ctrl-down → если бы мы его не фильтровали, на каждой вставке
  hook callback бы тратил время и мог re-arm recorder если хоткей —
  Ctrl как trigger.
- **`TRIGGER_HELD` AtomicBool** отдельно от `is_recording`. Раньше hook
  смотрел `is_recording.load()` чтобы dedupe KEYDOWN auto-repeat — но
  если юзер жал во время `transcribe`, флаг застревал true и все
  следующие нажатия dropиluсь. Теперь hook рулит только своим
  `TRIGGER_HELD`, recording_loop рулит своим `is_recording`.

### Recording loop wedge → watchdog в recording.rs

Если recording_loop крашнется mid-iteration (panicки), `is_recording`
останется true навсегда. Воркер `flov-state-watchdog` проверяет каждые 2s:
если `is_recording && mode==IDLE` 3 раза подряд — сбрасывает флаг.

### WebView2 long-session rot — periodic reload

Belt-and-suspenders сверх occlusion fix. WebView2 renderer leak'ит memory
+ DOM state накапливается за multi-hour сессию. Воркер
`flov-webview-reloader` делает `window.eval("location.reload()")` каждые
30 минут, пропуская моменты когда `is_recording==true`, hotkey mode active,
или `ui::overlay_active()==true` (чтобы не дёрнуть pill из-под живой
записи/transcribe). `is_visible()` больше нельзя использовать как busy
signal: main overlay window теперь намеренно остаётся OS-visible даже в idle,
а визуально исчезает только через Svelte `{#if}`. Дополнительно reload
требует `overlay_quiet_for(5s)`, чтобы stale logical hide не разрешил reload
в узком окне morph-out.

Важно: reload скрытого WebView не должен silently съесть первый следующий
`state-changed`. Main page вызывает `pill_frontend_ready` только после того,
как все Tauri event listeners зарегистрированы. Backend держит
`frontend_reload_in_progress`, ждёт ready после `location.reload()`, а при
следующем `window.show()` коротко ждёт ready перед emit `"recording"`, если
reload всё ещё pending. Если событие всё-таки попало в узкое окно между
reload и listener registration, `pill_frontend_ready` возвращает snapshot
последнего backend state (`idle|recording|transcribing|error` + error text),
и frontend восстанавливает pill без ожидания нового event.

Чтобы periodic reload и первый `show()` не мигали белой/пустой WebView2
surface на Windows, защита двухслойная:
- до создания WebView2 выставляем `WEBVIEW2_DEFAULT_BACKGROUND_COLOR=00000000`.
  Microsoft прямо документирует это как самый ранний способ убрать white
  flicker до применения `DefaultBackgroundColor` API/CSS;
- idle HWND держится с layered alpha=0 через `SetLayeredWindowAttributes`.
  Backend делает `window.show()` при alpha=0, эмитит state, а frontend
  после `tick()` + `requestAnimationFrame()` вызывает `repaint_window`;
  только этот command возвращает alpha=255 и tickle'ит DWM.

Ранее этот reload был привязан к recording cycle — если юзер сидел idle
6 часов, ни одна запись = reload так и не сработал, webview успевал
сгнить. Independent thread решил.

### Stale hide timers can hide the next recording

Main window больше не скрывается через `window.hide()`. После morph-out
frontend вызывает `hide_window` через ~520ms, но backend только помечает
overlay логически inactive и делает repaint/click-through refresh. Если
пользователь снова нажимает хоткей до конца transition, старый таймер уже не
может OS-hide'нуть новую запись; дополнительно `ui/src/routes/+page.svelte`
сбрасывает все таймеры (`hide/error`) на каждый новый visible state
и защищает delayed callbacks sequence counter'ом. Backend держит
`RECORDING_CYCLE_ACTIVE`, поэтому даже уже улетевший stale `hide_window`
не разрешит periodic reload во время новой recording/transcribe cycle.

Причина именно в Windows/WebView2: hidden/minimized WebView попадает в
background/hidden path с timer throttling/suspend; Wry прямо документирует,
что view может быть unloaded/suspended после ~5 минут hidden/minimized, а
Windows-ветка не поддерживает `background_throttling=disabled`. Поэтому
самый надёжный путь — не переводить pill WebView в hidden state вообще:
держим прозрачное click-through окно живым, idle DOM пустой.

Windows repaint тоже делается в два шага: Rust всё ещё tickle'ит DWM сразу
после `window.show()`, но frontend дополнительно вызывает `repaint_window`
после `tick()` + `requestAnimationFrame()`, когда SVG pill уже реально
появился в DOM. Иначе repaint мог произойти по пустому idle DOM.

### VC++ Redist обязателен для ВСЕХ sidecars (windows)

Не только CUDA — все sidecars (cpu/vulkan/cuda) импортят `MSVCP140.dll`,
`VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`. На свежем Windows этих DLL
нет. NSIS hook (`installer-hooks.nsh::NSIS_HOOK_POSTINSTALL`) сначала
запускает `vc_redist.x64.exe /install /passive /norestart`, потом
переименовывает cuBLAS DLLs (если CUDA вариант).

`build-bundle.ps1` качает vc_redist.x64.exe с
`aka.ms/vs/17/release/vc_redist.x64.exe` всегда (не только для CUDA).

### cuBLAS DLLs path quirk (Tauri 2 NSIS)

`bundle.resources` в `tauri.conf.json` ставит файлы по пути
**`$INSTDIR\<source-rel-path>`**, НЕ `$INSTDIR\resources\` как можно
подумать. Поэтому NSIS hook ищет cuBLAS в `$INSTDIR\binaries\runtime\`
(не `resources\runtime\`) и переименовывает в `$INSTDIR\` (рядом с exe —
системный DLL search path).

### Logging: OpenOptions::append, не File::create

`File::create()` **truncates** при каждом старте → флов.log на машинах
юзеров был 0 KB всё время. Сейчас `OpenOptions::new().create(true).append(true)` + `Mutex<File>` writer для `Sync` (см. `lib.rs::init_logging`). Дефолтный
уровень INFO; путь — рядом с exe (не CWD, который меняется когда
запускают из tray/start menu).

### audio-spectrum IPC saturation

Раньше FFT loop в `audio.rs` слал `audio-spectrum` event раз в 30 мс
(33 Hz). На multi-hour сессии Tauri's event channel не shed'ит load —
если JS listener fall'ит behind, очередь IPC растёт и в итоге выглядит
как зависание. Снизили до 60 мс (16 Hz) — wave выглядит так же гладко,
а IPC traffic вдвое меньше.

### Recorder hot path

Audio callback не должен делать дорогие операции. Старый FFT буфер делал
`Vec::remove(0)` после заполнения окна и держал отдельные locks для samples
и spectrum. Сейчас `CaptureState` держит samples + fixed-size ring buffer
под одним коротким mutex lock; FFT scratch/Hann buffers выделяются один раз
и переиспользуются в polling loop.

### Sidecar / OpenRouter hangs

`transcribe.rs` читает stdout/stderr sidecar'а в отдельных threads, пишет
PCM в stdin чанками (без полного `samples.len()*4` byte clone) и ждёт child
через `try_wait()` с timeout: max(30s, audio_seconds*12), capped at 10 min.
Если sidecar завис — процесс убивается, ошибка показывается пользователю,
recording cycle освобождается.

`postprocess.rs` использует reusable `ureq::Agent` с global timeout 120s.
Без timeout OpenRouter/network hang мог держать pill в `transcribing`
неограниченно долго.

### macOS unsigned-app + TCC: первый запуск = боль (mac)

**Главное правило**: на macOS любая capture-keyboard / global-paste
функциональность требует **Accessibility** permission, любой mic
capture — **Microphone** permission. Юзер выдаёт оба руками. Это
*невозможно* обойти из кода — это намеренный Apple security boundary.

Конкретные подводные грабли, на которые мы уже наступили:

**`AXIsProcessTrustedWithOptions(prompt=true)` ломает TCC для unsigned
app**. Apple API должен показать "Allow flov to control your computer"
диалог. Для unsigned билдов он показывает диалог, но запись в TCC
указывает на executable (`flov.app/Contents/MacOS/flov_app`), не на
bundle. Toggle вроде включён — но `AXIsProcessTrusted` возвращает
false навсегда. **Fix**: вызываем с `prompt=false`, открываем System
Settings → Accessibility через `open
"x-apple.systempreferences:..."` и логируем "add manually via +".
См. `hotkey.rs::macos_impl::install_hook`.

**Ad-hoc signing (`signingIdentity: "-"`) НЕ стабилизирует TCC между
rebuilds**. Был соблазн думать "ad-hoc даёт стабильный identity →
TCC entries persist". **Неправда**: ad-hoc signing у каждого rebuild
свой `cdhash`, и TCC keys на cdhash для anything без TeamIdentifier.
Только Developer ID Application certificate ($99/год Apple Dev
Program) даёт TeamIdentifier, по которому TCC матчит rebuild'ы как
"то же приложение". Ad-hoc реально даёт только: hardened-runtime
ready, notarization-ready, чуть лучший Gatekeeper UX. Не больше.

**`cpal::default_input_config()` блокирует** до того как юзер
ответит на mic permission диалог. На первом запуске между "INFO
Using input device" и "INFO Audio config" может пройти 1-3 минуты
(юзер не сразу видит диалог). Это норма, не зависание.

**TCC keys на полный path .app bundle**. Запуск из `/Volumes/flov/`
(смонтированный DMG) и `/Applications/flov.app` — две разные TCC
entries. После drag .app из DMG в Applications → permission grants
делать снова. Тоже — если двигаешь .app между папками.

**LSUIElement = true делает приложение menu-bar-only** (нет Dock
иконки, нет app menu). На первом запуске новичок может не понять
что приложение вообще запустилось. Иконка маленькая, в верхнем
правом углу экрана. Если хочется лучше UX onboarding'а — нужно
временно убрать LSUIElement или показывать Settings window на первом
запуске (флаг "first-run" в `flov.toml` → forced show).

**Gatekeeper "unknown developer" warning**: на unsigned первый
запуск открывает System Settings → Privacy & Security, внизу
"Open Anyway". Single click. Не повторяется. Notarization ($99/год)
убирает полностью.

**После каждого `tauri build` юзер должен заново выдать permissions**
(для unsigned билдов). Это значит для dev iteration:
- Установка через `cp -R target/release/bundle/macos/flov.app /Applications/`
- `tccutil reset Accessibility com.flov.app && tccutil reset Microphone com.flov.app`
- Запуск, выдача grants, restart

Делать пользователю — невыносимо. **Реально для production:**
- Apple Developer Program → Developer ID cert → `bundle.macOS.signingIdentity` указать его
- `xcrun notarytool submit ... --apple-id ... --team-id ... --password ...`
- `xcrun stapler staple flov.app` для embed notarization
- Тогда: drag в Applications → запуск → 2 permission диалога → готово,
  TCC persists через rebuilds.

### macOS SDK / deployment target dyld pitfall (mac)

Apple SDK 15+ добавляет `MTLResidencySetDescriptor` в Metal. whisper.cpp
безопасно гард'ит вызов через `@available(macOS 15.0, *)`, но без
явного deployment target клас линкается как **required** не **weak**
symbol. Билд против SDK 15 на Sonoma 14.x crash'ится при загрузке:
```
dyld: Symbol not found: _OBJC_CLASS_$_MTLResidencySetDescriptor
```

Sidecar умирает молча, родительский Rust код видит broken pipe при
write в его stdin: `failed to write samples to sidecar`. Симптом
выглядит как баг IPC, на самом деле — динамический линкер.

Три env vars в `scripts/build-sidecars.sh` / `build-bundle.sh`
решают это для macOS:
1. `MACOSX_DEPLOYMENT_TARGET=11.0` — clang добавляет `-mmacosx-version-min=11.0`
2. `CMAKE_OSX_DEPLOYMENT_TARGET=11.0` — whisper-rs-sys's build.rs форвардит
   env vars с префиксом `CMAKE_` в cmake; `cmake-rs` сам не транслирует
   MACOSX_DEPLOYMENT_TARGET → CMAKE_OSX_DEPLOYMENT_TARGET
3. `RUSTFLAGS="-L<clang_rt_dir> -lclang_rt.osx"` — clang генерит
   runtime call `__isPlatformVersionAtLeast` для `@available()`, который
   живёт в `libclang_rt.osx.a`. Apple's clang auto-линкует, rustc — нет.
   Путь через `clang -print-runtime-dir`.

### CGEventTap на macOS не выживает long sessions без re-enable (mac)

Симметрично Windows-side LL hook timeout. macOS присылает в tap
callback pseudo-events `TapDisabledByTimeout` (когда callback тупит)
или `TapDisabledByUserInput` (на Cmd+Tab, password prompts и т.п.).
Tap **остаётся disabled** пока мы явно не вызовем `CGEventTapEnable`.

Fix в `hotkey.rs::macos_impl::tap_callback`: на эти event types
вызываем `CGEventTapEnable(port, true)` через raw FFI + `AtomicPtr<c_void>`
со stashed `CFMachPortRef`. Без этого хоткей умирает после первого
Cmd+Tab.

Аналогично Windows-side: `TRIGGER_HELD` AtomicBool отдельно от
`state.is_recording` (recording_loop сам управляет своим флагом),
`RwLock<MacCombo>` + `try_read` вместо Mutex.

## Зависимости (src-tauri/Cargo.toml)

- **tauri** (2, features = ["tray-icon", "image-png"])
- **cpal** — WASAPI запись
- **rustfft** — FFT для спектра
- **windows** (0.61) — Win32 API (хук, clipboard, MonitorFromPoint,
  DwmSetWindowAttribute для отключения native rounding на Settings, Registry)
- **ureq** — HTTP клиент для OpenRouter
- **toml** + **toml_edit** — конфиг (read + surgical write)
- **image** — recolor tray PNG для dark theme
- **anyhow**, **tracing**, **tracing-subscriber**, **serde/serde_json**

Sidecar crates (`crates/flov-whisper-*/Cargo.toml`):
- **whisper-rs** (0.16, разные features per backend)
- **anyhow**, **num_cpus**
