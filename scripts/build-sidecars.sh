#!/usr/bin/env bash
# Bash equivalent of scripts/build-sidecars.ps1 — builds every
# `crates/flov-whisper-*` it finds (the PowerShell version targets
# Windows/CUDA, this one targets macOS and Linux.
#
# Usage:
#   ./scripts/build-sidecars.sh                  # build all macOS-relevant sidecars (release)
#   ./scripts/build-sidecars.sh --backend cpu    # only the CPU one
#   ./scripts/build-sidecars.sh --backend metal  # only Metal
#   ./scripts/build-sidecars.sh --profile debug  # debug profile

set -euo pipefail

backend="all"
profile="release"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --backend)
            backend="$2"
            shift 2
            ;;
        --profile)
            profile="$2"
            shift 2
            ;;
        *)
            echo "Unknown arg: $1" >&2
            exit 1
            ;;
    esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
crates_dir="$root/crates"
target_dir="$root/target"
stage_dirs=("$target_dir/debug" "$target_dir/release")

apple_build_env() {
    # Pin Apple deployment target to match `tauri.macos.conf.json`
    # (`minimumSystemVersion = 11.0`). Without this the Metal sidecar
    # references symbols that only live on the build host's SDK — e.g.
    # `MTLResidencySetDescriptor` was introduced in macOS 15, so a binary
    # built against the Sequoia SDK with default deployment target won't
    # load on Sonoma (dyld: Symbol not found).
    #
    # MACOSX_DEPLOYMENT_TARGET nudges Rust's linker; CMAKE_OSX_DEPLOYMENT_TARGET
    # is required separately because whisper-rs-sys's build.rs forwards
    # any env var prefixed with CMAKE_ into the cmake invocation, and the
    # cmake crate doesn't translate MACOSX_DEPLOYMENT_TARGET on its own.
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
    export CMAKE_OSX_DEPLOYMENT_TARGET="${CMAKE_OSX_DEPLOYMENT_TARGET:-$MACOSX_DEPLOYMENT_TARGET}"

    # With deployment target < SDK version, every `@available(macOS X, *)`
    # block compiled by clang emits a runtime check via the builtin
    # `__isPlatformVersionAtLeast`. That symbol lives in libclang_rt.osx
    # which Apple's clang auto-links, but rustc's linker invocation does
    # NOT. Locate it via `clang -print-runtime-dir` so we don't hardcode
    # the Xcode toolchain version.
    local clang_rt_dir
    clang_rt_dir="$(clang -print-runtime-dir 2>/dev/null || true)"
    if [[ -d "$clang_rt_dir" && -f "$clang_rt_dir/libclang_rt.osx.a" ]]; then
        export RUSTFLAGS="${RUSTFLAGS:-} -L${clang_rt_dir} -lclang_rt.osx"
    else
        echo "warning: libclang_rt.osx.a not found via clang — link errors likely" >&2
    fi
}

if [[ "$(uname -s)" == "Darwin" ]]; then
    apple_build_env
fi

case "$(uname -s)" in
    Darwin) candidates=("cpu" "metal") ;;
    Linux)  candidates=("cpu" "vulkan" "cuda") ;;
    *)      candidates=("cpu") ;;
esac

build_one() {
    local name="$1"
    local crate="$crates_dir/flov-whisper-$name"
    if [[ ! -d "$crate" ]]; then
        echo "skip: crates/flov-whisper-$name not found"
        return 0
    fi
    echo ">> building flov-whisper-$name ($profile)"
    local args=(build --manifest-path "$crate/Cargo.toml" --target-dir "$target_dir")
    if [[ "$profile" == "release" ]]; then
        args+=(--release)
    fi
    cargo "${args[@]}"

    local exe="$target_dir/$profile/flov-whisper-$name"
    if [[ ! -f "$exe" ]]; then
        echo "expected output not found: $exe" >&2
        exit 1
    fi

    for dst in "${stage_dirs[@]}"; do
        mkdir -p "$dst"
        local staged="$dst/flov-whisper-$name"
        if [[ "$exe" != "$staged" ]]; then
            cp -f "$exe" "$staged"
        fi
    done
}

if [[ "$backend" == "all" ]]; then
    for name in "${candidates[@]}"; do
        build_one "$name"
    done
else
    build_one "$backend"
fi

echo "done."
