#!/usr/bin/env bash
# Stable per-user launcher installed by scripts/install-linux.sh.

set -euo pipefail

data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
appimage="$data_home/flov/flov.AppImage"

if [[ ! -x "$appimage" ]]; then
    notify-send "Flov" "AppImage не найден: $appimage" 2>/dev/null || true
    exit 1
fi

# Control invocations must stay synchronous so Hyprland press/release bindings
# receive an error when the local service is not running.
if [[ $# -gt 0 ]]; then
    exec "$appimage" "$@"
fi

if pgrep -x flov_app >/dev/null 2>&1; then
    exit 0
fi

# Rofi/GIO may reap the desktop-launch process group after activation.
# Detach Flov so its tray and HTTP service keep running independently.
setsid --fork "$appimage" >/dev/null 2>&1
