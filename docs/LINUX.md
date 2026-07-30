# Linux / Wayland / Hyprland

Flov runs natively in a Wayland session. Audio capture goes through
CPAL/ALSA (normally PipeWire's ALSA compatibility layer), text insertion uses
`wl-copy` + `wtype`, and Whisper runs in the existing CPU/Vulkan/CUDA
sidecars.

On NVIDIA, Flov enables WebKitGTK's software-buffer fallback to avoid a
Hyprland explicit-sync failure (`Missing acquire timeline`). The AppImage
builder also removes linuxdeploy's forced `GDK_BACKEND=x11`, so GTK can select
native Wayland. To test a future WebKit/driver combination without the
fallback, launch once with `WEBKIT_DISABLE_DMABUF_RENDERER=0`.

## Arch Linux dependencies

```bash
sudo pacman -S --needed \
  base-devel rustup nodejs npm cmake clang pkgconf \
  webkit2gtk-4.1 gtk3 libappindicator alsa-lib \
  wl-clipboard wtype
```

For NVIDIA/CUDA:

```bash
sudo pacman -S --needed cuda
```

For Vulkan, install the loader plus the ICD for the active GPU. For example,
NVIDIA normally uses `vulkan-icd-loader` + `nvidia-utils`.

`webkit2gtk-4.1` is a build and runtime dependency of the Tauri UI. The HTTP
transcription code itself is headless-safe once the application has started:
failure to open a local microphone does not disable the server.

## Development and bundle

```bash
npm ci --prefix ui

# Builds CPU + the detected GPU sidecar, stages it, and starts Tauri dev.
./dev.sh

# AppImage; auto selects CUDA on an active NVIDIA system, otherwise Vulkan.
./scripts/build-bundle-linux.sh

# Explicit choices always retain the CPU fallback.
./scripts/build-bundle-linux.sh --backend cuda
./scripts/build-bundle-linux.sh --backend vulkan
./scripts/build-bundle-linux.sh --backend cpu
```

## Install AppImage for the current user

The installer copies the AppImage to a stable XDG data path, installs the icon
and a detached launcher, and registers `Flov` for freedesktop application
menus such as Rofi `drun`:

```bash
# Build and install the newest local AppImage.
./scripts/build-bundle-linux.sh
./scripts/install-linux.sh

# Or install an AppImage downloaded from a release.
chmod +x ~/Downloads/flov_*_amd64.AppImage
./scripts/install-linux.sh ~/Downloads/flov_*_amd64.AppImage
```

Default installed paths:

- AppImage: `~/.local/share/flov/flov.AppImage`;
- launcher: `~/.local/bin/flov`;
- desktop entry: `~/.local/share/applications/flov.desktop`;
- icon: `~/.local/share/icons/hicolor/128x128/apps/flov.png`.

Open Rofi, type `Flov`, and press Enter. The application is a tray/background
service, so opening it does not show a normal window. Right-click its tray icon
and open **Settings → Models** to download a Whisper model.

The launcher detaches the process from Rofi/GIO so the tray and HTTP server are
not reaped when the menu closes. Re-running it is idempotent. It also forwards
control arguments synchronously:

```text
flov --record-start
flov --record-stop
flov --server-health
```

To update, rerun `scripts/install-linux.sh` with the new AppImage and launch
Flov again.

Runtime data:

- config: `~/.local/share/flov/flov.toml`;
- models: `~/.local/share/flov/models/`;
- log: `~/.local/share/flov/flov.log`;
- stats: `~/.local/share/flov/stats.json`.

`$XDG_DATA_HOME/flov/` replaces `~/.local/share/flov/` when
`XDG_DATA_HOME` is set.

## Hyprland push-to-talk

The preferred Wayland path is a compositor press/release binding. The
control invocation exits before GTK/Tauri initialization, so it is cheap and
does not create a second window:

```text
flov_app --record-start
flov_app --record-stop
```

For a Hyprland 0.56+ Lua config:

```lua
local flov_bin = "/absolute/path/to/flov_app"

-- Start the tray/API process with the compositor.
hl.on("hyprland.start", function()
    hl.exec_cmd("pgrep -x flov_app >/dev/null || " .. flov_bin)
end)

-- Right Ctrl press starts capture; release stops and transcribes.
hl.bind("Control_R", hl.dsp.exec_cmd(flov_bin .. " --record-start"))
hl.bind("Control_R", hl.dsp.exec_cmd(flov_bin .. " --record-stop"), { release = true })

-- Wayland toplevels cannot position themselves. Keep the pill floating,
-- pinned and at the bottom-center of the current monitor.
hl.window_rule({
    name = "flov-pill",
    match = {
        class = "^flov_app$",
        title = "^flov$",
    },
    float = true,
    pin = true,
    no_focus = true,
    no_initial_focus = true,
    render_unfocused = true,
    no_anim = true,
    no_blur = true,
    no_shadow = true,
    decorate = false,
    border_size = 0,
    rounding = 0,
    size = "800 200",
    move = "monitor_w/2-400 monitor_h-224",
    opacity = "1 override 1 override",
})
```

Replace `flov_bin` with the installed launcher `/home/USER/.local/bin/flov`,
the built binary `/path/to/flov/target/release/flov_app`, or the AppImage
path. Keep
`[server].enabled = true`, because the lightweight control command talks to
the local control endpoint.

If the server address is changed, provide it to both Hyprland commands:

```lua
local flov_env = "FLOV_SERVER_URL=http://127.0.0.1:19000 "
hl.bind("Control_R", hl.dsp.exec_cmd(flov_env .. flov_bin .. " --record-start"))
hl.bind("Control_R", hl.dsp.exec_cmd(flov_env .. flov_bin .. " --record-stop"), { release = true })
```

If `[server].api_key` is set, add
`FLOV_API_KEY=the-same-token` alongside `FLOV_SERVER_URL`.

Run `hyprctl reload` after editing the compositor config.

## evdev fallback

Flov also watches the configurable hotkey directly through `/dev/input`.
This works under any Wayland compositor and supports hot-plugged keyboards.
On Linux the default is `RCtrl`.

The user must be able to read the keyboard event devices:

```bash
sudo usermod -aG input "$USER"
```

Log out and back in after changing group membership. The Hyprland binding
above does not need this permission and remains the recommended path. Both
paths are idempotent, so leaving evdev enabled while using the compositor
binding is harmless.

## Text insertion

After transcription Flov writes UTF-8 text to the Wayland clipboard and
sends `Ctrl+V` through `wtype`. If recognition works but nothing is inserted,
check:

```bash
command -v wl-copy wtype
echo test | wl-copy
wtype -M ctrl -k v -m ctrl
```

Some security-sensitive clients deliberately reject virtual-keyboard input;
the transcript still remains in the clipboard in that case.

## HTTP service

The service is enabled on loopback by default:

```text
POST http://127.0.0.1:17432/v1/audio/transcriptions
```

It can be called independently from the desktop hotkey and does not paste
the returned text. Full request/response and LAN configuration examples are
in [API.md](API.md).
