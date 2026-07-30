#!/usr/bin/env bash
# One-shot dev runner for macOS / Linux. Equivalent of dev.cmd.
#
# Tauri build script validates that every `externalBin` entry in
# tauri.{platform}.conf.json exists with the expected triple suffix,
# *even during `cargo check`*. So we stage real sidecars before
# starting tauri dev — same pattern the Windows scripts assume.

set -euo pipefail

root="$(cd "$(dirname "$0")" && pwd)"
cd "$root"

# Build + stage sidecars on first run; subsequent runs reuse the
# cached binaries (cargo's own incremental compilation handles
# rebuilds when sources change).
need_stage=0
case "$(uname -s)" in
    Darwin)
        triple="aarch64-apple-darwin"
        for s in cpu metal; do
            if [[ ! -f "src-tauri/binaries/flov-whisper-$s-$triple" ]]; then
                need_stage=1; break
            fi
        done
        ;;
    Linux)
        triple="$(rustc -vV | sed -n 's/^host: //p')"
        sidecars=(cpu)
        if [[ -e /dev/nvidiactl && -x /opt/cuda/bin/nvcc ]]; then
            sidecars+=(cuda)
        elif pkg-config --exists vulkan 2>/dev/null; then
            sidecars+=(vulkan)
        fi
        for s in "${sidecars[@]}"; do
            if [[ ! -f "target/release/flov-whisper-$s" ]]; then
                need_stage=1
                break
            fi
        done
        if [[ ! -f "src-tauri/binaries/flov-whisper-cpu-$triple" ]]; then
            need_stage=1
        fi
        ;;
esac

if [[ "$need_stage" -eq 1 ]]; then
    echo ">> staging sidecars (one-time, ~3 min cold)"
    case "$(uname -s)" in
        Darwin)
            "$root/scripts/build-sidecars.sh"
            mkdir -p src-tauri/binaries
            for s in cpu metal; do
                cp -f "target/release/flov-whisper-$s" \
                      "src-tauri/binaries/flov-whisper-$s-aarch64-apple-darwin"
            done
            ;;
        Linux)
            for s in "${sidecars[@]}"; do
                "$root/scripts/build-sidecars.sh" --backend "$s"
            done
            mkdir -p src-tauri/binaries
            cp -f target/release/flov-whisper-cpu \
                "src-tauri/binaries/flov-whisper-cpu-$triple"
            chmod +x "src-tauri/binaries/flov-whisper-cpu-$triple"
            ;;
    esac
fi

exec ./ui/node_modules/.bin/tauri dev
