# Flov

Voice-to-text для Windows и macOS (Apple Silicon). Зажми хоткей, говори,
отпусти — текст вставляется в активное поле через буфер обмена.

Текущая версия: `0.2.3`.

Транскрипция локальная (Whisper.cpp на GPU). Опциональная пост-обработка
через OpenRouter (clean-up пунктуации/мата/etc).

## Запуск (Windows)

1. Скачай или собери `flov.exe` (см. ниже)
2. Запусти — иконка в трее, модель качается через Settings → Models
3. Зажми **Ctrl+Win**, говори, отпусти

## Запуск (macOS, Apple Silicon)

1. Открой `.dmg` → перетащи `flov.app` на ярлык `Applications`
2. Первый запуск: Spotlight → "flov" → Gatekeeper ругнётся
   ("unknown developer", потому что unsigned).
   Открой **System Settings → Privacy & Security**, прокрути вниз →
   `Open Anyway` → `Open` в повторном диалоге.
3. Приложение запустится в **menu bar** (без Dock-иконки —
   `LSUIElement = true`). Маленькая иконка в правом верхнем углу.
4. Автоматически откроется **System Settings → Privacy & Security →
   Accessibility**. Нажми `+`, выбери **`/Applications/flov.app`**
   (именно `.app`, не лезь в `Contents/MacOS/`), поставь галку.
5. Сделай то же самое в **Microphone** (та же страница, чуть выше).
6. Quit flov (правый клик на menu bar иконку → Quit) → запусти заново.
7. Иконка в menu bar → модель качается через Settings → Models.
8. Зажми **Cmd+Alt**, говори, отпусти — текст вставляется в активное поле.

### Подводные камни macOS (читай ЕСЛИ не работает)

**Хоткей не реагирует / `CGEventTapCreate failed` в логе:**
- Тогл Accessibility выключен или указывает на старый hash. Снова
  System Settings → Privacy & Security → Accessibility → удали запись
  flov через `−` → добавь через `+` → `/Applications/flov.app` →
  toggle ON → **Quit & relaunch** flov.

**НЕ используй системный auto-prompt** "Allow flov to control your
computer" если он вылез — на unsigned билдах он добавляет в TCC
**executable** (`Contents/MacOS/flov_app`), не bundle. Toggle вроде
включён, но Accessibility всё равно false. Удали запись и добавь
руками через `+`.

**После каждой пересборки** (`./scripts/build-bundle.sh` →
переустановка `.app`) — TCC видит **другой бинарь** (поменялся hash)
и **сбрасывает grants**. Заново даёшь Accessibility + Microphone.
Это ограничение macOS для unsigned-апп; решается только Apple
Developer Account ($99/год → Developer ID Application certificate
→ стабильный TeamIdentifier → TCC помнит permissions через rebuild'ы).

**Запуск из примонтированного `.dmg` не равен запуску из
`/Applications/flov.app`** — это два разных пути, две разные TCC
entries. Перетаскивай в Applications **до** первого запуска.

**Лог**: `~/Library/Application Support/com.flov.app/flov.log`
(`tail -f` чтобы смотреть в realtime).

**Если хочешь сбросить permissions для теста**:
```bash
tccutil reset Accessibility com.flov.app
tccutil reset Microphone com.flov.app
```

## Settings (правый клик по трею → Open Settings)

- **Models** — каталог Whisper моделей (tiny / base / small / medium / large-v3-turbo)
- **Backend** — выбор GPU sidecar (CUDA / Vulkan / Metal / CPU), Auto = первый usable
  backend; CUDA на Windows скрывается без NVIDIA runtime (`nvcuda.dll`)
- **Post-process** — OpenRouter API key, модель, системный промпт
- **Hotkey** — любая комбинация (включая одиночный RCtrl)
- **Stats** — heatmap записей по дням

На Windows Settings создаётся лениво при клике в трее. Это важно:
скрытый transparent WebView2 при старте и второй WebView с другим
`additionalBrowserArgs` в той же profile папке могут падать с
`HRESULT(0x8007139F)`. Начиная с `0.2.2` Settings использует отдельный
WebView2 data dir (`webview-settings`) и пересоздаёт битую window-обёртку,
если Tauri оставил её после неудачного WebView init.

## Windows reliability notes

Pill window на Windows намеренно не скрывается через OS `window.hide()`.
В idle оно остаётся живым transparent + click-through HWND с alpha `0`, а
видимость контролируется Svelte DOM. Это обходит WebView2 hidden/background
path, где renderer/timers могут быть suspended, а следующий `show()` иногда
возвращает пустую stale surface.

Периодический reload main WebView нужен против long-session WebView2 rot,
но он alpha-gated: backend показывает HWND при alpha `0`, frontend ждёт
`tick()` + `requestAnimationFrame()`, затем вызывает `repaint_window`, и
только после этого Windows alpha возвращается к `255`. Дополнительно
`WEBVIEW2_DEFAULT_BACKGROUND_COLOR=00000000` выставляется до создания
WebView2, чтобы убрать white flash до применения CSS/API.

## Сборка из исходников (Windows)

Требования: Rust toolchain, Node.js, для CUDA — CUDA toolkit и Ninja, для
Vulkan — LunarG SDK.

```powershell
# Дев с hot-reload
.\dev.cmd

# Sidecar бинари (отдельно — workspace excluded)
.\scripts\build-sidecars.ps1                  # все backend'ы
.\scripts\build-sidecars.ps1 -Backend cuda    # один

# Релиз (просто main app)
.\ui\node_modules\.bin\tauri.cmd build
```

## Сборка из исходников (macOS, Apple Silicon)

Требования: Xcode Command Line Tools, Rust toolchain, Node.js, `cmake`.
Никаких отдельных SDK'шек — Metal headers идут с macOS.

```bash
brew install rust cmake node      # если их ещё нет

# Дев с hot-reload (первый запуск собирает + стейджит sidecars ~3 мин)
./dev.sh

# Sidecar бинари
./scripts/build-sidecars.sh                   # cpu + metal
./scripts/build-sidecars.sh --backend metal   # один
```

Полный гайд + permissions (Microphone, Accessibility) + архитектурные
заметки — [docs/MACOS.md](docs/MACOS.md).

## Bundle для релиза

### Windows: NSIS installer

```powershell
# CPU + Vulkan (нет внешних зависимостей у пользователя)
.\scripts\build-bundle.ps1

# + CUDA (требует CUDA toolkit на машине сборки, тащит cublas DLLs)
.\scripts\build-bundle.ps1 -IncludeCuda
```

На выходе: `target/release/bundle/nsis/flov_<version>_x64-setup.exe` —
single-file installer. Whisper модель (~1.6 GB) НЕ внутри installer —
пользователь скачивает её через Settings → Models после установки.

`-SkipSidecars` переиспользует уже собранные sidecar binaries, но теперь
требует свежий `sidecars-manifest.json` из предыдущей non-skip сборки и
проверяет source hash + sha256 файлов. CUDA — только opt-in через
`-IncludeCuda`; если включить CUDA и cuBLAS DLLs не найдены, сборка падает.

### macOS: .app + .dmg

```bash
./scripts/build-bundle.sh
```

На выходе:
- `target/release/bundle/macos/flov.app`
- `target/release/bundle/dmg/flov_<version>_aarch64.dmg` (~5 MB)

Bundle подписан **ad-hoc** (`signingIdentity: "-"`) с hardened
runtime + entitlements (`com.apple.security.device.audio-input`).
Это **не** избавляет от Gatekeeper "unknown developer" warning при
первом запуске и **не** делает TCC entries persistent через
rebuilds — для этого нужен **Apple Developer ID Application
certificate** ($99/год Apple Developer Program). Без него — каждая
пересборка требует от юзера повторно добавить .app в Accessibility +
Microphone (см. "Подводные камни macOS" выше).

Модель Whisper тоже не внутри — качается из Settings → Models после
первого запуска.

Подробности по архитектуре — [CLAUDE.md](CLAUDE.md). По sidecar'ам —
[crates/README.md](crates/README.md). По маковому порту в деталях —
[docs/MACOS.md](docs/MACOS.md).

## Стек

- Tauri 2 + Svelte 5 (frontend, frameless transparent windows)
- Rust + cpal (WASAPI/CoreAudio запись) + windows-rs / core-graphics
  (хук клавиатуры)
- whisper-rs sidecar бинари по backend'у (CUDA / Vulkan / Metal / CPU)
- OpenRouter HTTP API через ureq
