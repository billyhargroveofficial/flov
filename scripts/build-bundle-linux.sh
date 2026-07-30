#!/usr/bin/env bash
# Linux/Wayland release builder. Produces an AppImage and includes a CPU
# fallback plus the best locally-buildable GPU sidecar.
#
# Usage:
#   ./scripts/build-bundle-linux.sh
#   ./scripts/build-bundle-linux.sh --backend cuda
#   ./scripts/build-bundle-linux.sh --backend vulkan
#   ./scripts/build-bundle-linux.sh --backend cpu
#   ./scripts/build-bundle-linux.sh --backend all
#   ./scripts/build-bundle-linux.sh --skip-sidecars

set -euo pipefail

backend="auto"
skip_sidecars=0
bundles="appimage"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --backend)
            backend="$2"
            shift 2
            ;;
        --skip-sidecars)
            skip_sidecars=1
            shift
            ;;
        --bundles)
            bundles="$2"
            shift 2
            ;;
        *)
            echo "Unknown arg: $1" >&2
            exit 1
            ;;
    esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
target_dir="$root/target"
bin_dir="$root/src-tauri/binaries"
triple="$(rustc -vV | sed -n 's/^host: //p')"

if [[ "$triple" != *-linux-* ]]; then
    echo "This builder must run on a Linux Rust target (got: $triple)." >&2
    exit 1
fi

case "$backend" in
    auto)
        sidecars=(cpu)
        if [[ -e /dev/nvidiactl && -x /opt/cuda/bin/nvcc ]]; then
            sidecars+=(cuda)
        elif pkg-config --exists vulkan 2>/dev/null; then
            sidecars+=(vulkan)
        fi
        ;;
    cpu) sidecars=(cpu) ;;
    cuda) sidecars=(cpu cuda) ;;
    vulkan) sidecars=(cpu vulkan) ;;
    all) sidecars=(cpu vulkan cuda) ;;
    *)
        echo "Unknown backend: $backend (use auto, cpu, cuda, vulkan, or all)." >&2
        exit 1
        ;;
esac

if [[ $skip_sidecars -eq 0 ]]; then
    for sidecar in "${sidecars[@]}"; do
        "$root/scripts/build-sidecars.sh" --backend "$sidecar"
    done
fi

mkdir -p "$bin_dir"
find "$bin_dir" -maxdepth 1 -type f -name "*-$triple" -delete

external_json=""
for sidecar in "${sidecars[@]}"; do
    src="$target_dir/release/flov-whisper-$sidecar"
    dst="$bin_dir/flov-whisper-$sidecar-$triple"
    if [[ ! -f "$src" ]]; then
        echo "Expected sidecar not found: $src" >&2
        exit 1
    fi
    cp -f "$src" "$dst"
    chmod +x "$dst"
    if [[ -n "$external_json" ]]; then
        external_json+=","
    fi
    external_json+="\"binaries/flov-whisper-$sidecar\""
    echo "   staged $(basename "$dst")"
done

override="$(mktemp "${TMPDIR:-/tmp}/flov-tauri-linux.XXXXXX.json")"
trap 'rm -f "$override"' EXIT
printf '{"bundle":{"externalBin":[%s]}}\n' "$external_json" > "$override"

tauri="$root/ui/node_modules/.bin/tauri"
if [[ ! -x "$tauri" ]]; then
    echo "Tauri CLI missing at $tauri — run npm install in ui/ first." >&2
    exit 1
fi

echo ">> tauri build ($bundles)"
cd "$root"
# The linuxdeploy binary currently downloaded by Tauri carries an older
# binutils strip. On rolling distributions (notably Arch) it cannot process
# newer ELF sections such as .relr.dyn. linuxdeploy supports NO_STRIP
# explicitly; AppImage compression still keeps the resulting bundle compact.
export NO_STRIP=1

# GdkPixbuf 2.44+ can use Glycin without installing the legacy loaders
# directory, while linuxdeploy-plugin-gtk still assumes that directory exists.
# Give the plugin a harmless, project-local compatibility directory rather
# than requiring a fake path under /usr.
if [[ "$bundles" == *appimage* ]] && command -v pkg-config >/dev/null; then
    gdk_binary_dir="$(pkg-config --variable=gdk_pixbuf_binarydir gdk-pixbuf-2.0 2>/dev/null || true)"
    if [[ -n "$gdk_binary_dir" && ! -d "$gdk_binary_dir" ]]; then
        gdk_version="$(pkg-config --variable=gdk_pixbuf_binary_version gdk-pixbuf-2.0)"
        compat_dir="$target_dir/appimage-compat/gdk-pixbuf-2.0/$gdk_version"
        compat_pc_dir="$target_dir/appimage-compat/pkgconfig"
        source_pc="$(pkg-config --path gdk-pixbuf-2.0)"

        mkdir -p "$compat_dir/loaders" "$compat_pc_dir"
        gdk-pixbuf-query-loaders > "$compat_dir/loaders.cache"
        sed "s|^gdk_pixbuf_binarydir=.*|gdk_pixbuf_binarydir=$compat_dir|" \
            "$source_pc" > "$compat_pc_dir/gdk-pixbuf-2.0.pc"
        export PKG_CONFIG_PATH="$compat_pc_dir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    fi
fi

"$tauri" build --bundles "$bundles" --config "$override"

bundle_dir="$target_dir/release/bundle"

# linuxdeploy's GTK plugin currently forces X11 and does not blacklist
# NVIDIA's driver shim. Keep GTK's automatic Wayland/X11 selection and never
# ship the build machine's libcuda.so.1. CUDA runtime and cuBLAS stay bundled;
# libcuda is resolved from the target host.
if [[ "$bundles" == *appimage* ]]; then
    appdir="$(find "$bundle_dir/appimage" -maxdepth 1 -type d -name '*.AppDir' -print -quit)"
    appimage="$(find "$bundle_dir/appimage" -maxdepth 1 -type f -name '*.AppImage' \
        -printf '%T@ %p\n' | sort -nr | head -1 | cut -d' ' -f2-)"
    if [[ -n "$appdir" && -n "$appimage" ]]; then
        repack=0

        # linuxdeploy rewrites WebKitGTK's fixed /usr/lib helper-process path
        # to ././/lib inside the AppImage, but currently does not copy those
        # executables. That rewritten prefix is relative to AppDir/usr, so run
        # from there and bundle the matching Network/Web/GPU processes plus the
        # injected bundle. Without this, the AppImage exits before Tauri starts.
        webkit_libdir="$(pkg-config --variable=libdir webkit2gtk-4.1 2>/dev/null || true)"
        webkit_helpers="$webkit_libdir/webkit2gtk-4.1"
        if [[ ! -d "$webkit_helpers" ]]; then
            echo "WebKitGTK helper directory not found: $webkit_helpers" >&2
            exit 1
        fi
        mkdir -p "$appdir/usr/lib"
        cp -a "$webkit_helpers" "$appdir/usr/lib/"
        apprun="$appdir/AppRun"
        if [[ -f "$apprun" ]] && ! rg -q '^cd "\$this_dir/usr"$' "$apprun"; then
            sed -i '/^source .*linuxdeploy-plugin-gtk\\.sh/a cd "$this_dir/usr"' "$apprun"
        fi
        repack=1

        gtk_hook="$appdir/apprun-hooks/linuxdeploy-plugin-gtk.sh"
        if [[ -f "$gtk_hook" ]] && rg -q '^export GDK_BACKEND=x11' "$gtk_hook"; then
            sed -i '/^export GDK_BACKEND=x11/d' "$gtk_hook"
            repack=1
        fi
        if [[ -f "$appdir/usr/lib/libcuda.so.1" ]]; then
            rm -f -- "$appdir/usr/lib/libcuda.so.1"
            repack=1
        fi

        if [[ $repack -eq 0 ]]; then
            echo ">> AppImage already uses host GTK backend and NVIDIA driver"
        else
            appimage_plugin="${XDG_CACHE_HOME:-$HOME/.cache}/tauri/linuxdeploy-plugin-appimage.AppImage"
            if [[ ! -x "$appimage_plugin" ]]; then
                echo "AppImage plugin not found: $appimage_plugin" >&2
                exit 1
            fi

            repacked="${appimage%.AppImage}.repacked.AppImage"
            echo ">> repacking for native Wayland with WebKitGTK helpers"
            LDAI_OUTPUT="$repacked" "$appimage_plugin" --appimage-extract-and-run \
                --appdir="$appdir"
            mv -f -- "$repacked" "$appimage"
        fi
    fi
fi

artifact="$(find "$bundle_dir" -type f \( -name '*.AppImage' -o -name '*.deb' -o -name '*.rpm' \) \
    -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -1 | cut -d' ' -f2- || true)"
echo "done."
if [[ -n "$artifact" ]]; then
    echo "Bundle: $artifact ($(du -h "$artifact" | cut -f1))"
else
    echo "Bundle output: $bundle_dir"
fi
