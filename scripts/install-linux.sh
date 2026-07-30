#!/usr/bin/env bash
# Install a built/downloaded Flov AppImage for the current user and register it
# in freedesktop application menus such as Rofi drun.
#
# Usage:
#   ./scripts/install-linux.sh
#   ./scripts/install-linux.sh ~/Downloads/flov_0.2.3_amd64.AppImage

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bundle_dir="$root/target/release/bundle/appimage"

if [[ $# -gt 1 ]]; then
    echo "Usage: $0 [path-to-Flov.AppImage]" >&2
    exit 2
fi

if [[ $# -eq 1 ]]; then
    source_appimage="$(realpath "$1")"
else
    source_appimage=""
    for candidate in "$bundle_dir"/flov_*_amd64.AppImage; do
        [[ -f "$candidate" ]] || continue
        if [[ -z "$source_appimage" || "$candidate" -nt "$source_appimage" ]]; then
            source_appimage="$candidate"
        fi
    done
fi

if [[ -z "$source_appimage" || ! -f "$source_appimage" ]]; then
    echo "Flov AppImage not found. Build it first or pass its path explicitly." >&2
    exit 1
fi
if [[ ! -x "$source_appimage" ]]; then
    chmod +x "$source_appimage"
fi

for command in install pgrep setsid; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Required command not found: $command" >&2
        exit 1
    fi
done

data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_home="${XDG_BIN_HOME:-$HOME/.local/bin}"
installed_appimage="$data_home/flov/flov.AppImage"
launcher="$bin_home/flov"
desktop_file="$data_home/applications/flov.desktop"
icon_file="$data_home/icons/hicolor/128x128/apps/flov.png"

install -Dm755 "$source_appimage" "$installed_appimage.new"
mv -f -- "$installed_appimage.new" "$installed_appimage"
install -Dm755 "$root/scripts/flov-launcher-linux.sh" "$launcher"
install -Dm644 "$root/src-tauri/icons/128x128.png" "$icon_file"

mkdir -p "$(dirname "$desktop_file")"
desktop_tmp="$(mktemp "$(dirname "$desktop_file")/.flov.desktop.XXXXXX")"
trap 'rm -f -- "$desktop_tmp"' EXIT
{
    printf '%s\n' \
        '[Desktop Entry]' \
        'Version=1.0' \
        'Type=Application' \
        'Name=Flov' \
        'GenericName=Голосовой ввод' \
        'Comment=Распознавание речи через локальный Whisper'
    printf 'Exec=%s\n' "$launcher"
    printf 'TryExec=%s\n' "$launcher"
    printf '%s\n' \
        'Icon=flov' \
        'Terminal=false' \
        'StartupNotify=false' \
        'StartupWMClass=flov_app' \
        'Categories=AudioVideo;Audio;' \
        'Keywords=voice;speech;transcription;whisper;голос;диктовка;'
} > "$desktop_tmp"
install -m644 "$desktop_tmp" "$desktop_file"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$desktop_file"
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$(dirname "$desktop_file")"
fi

echo "Flov installed:"
echo "  AppImage: $installed_appimage"
echo "  launcher: $launcher"
echo "  desktop:  $desktop_file"
echo
echo "Open Rofi drun and select Flov, or run: $launcher"
