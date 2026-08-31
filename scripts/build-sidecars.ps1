# Builds every transcription sidecar in crates/ and stages the binaries
# (plus required runtime DLLs) next to the main flov binary so locate_sidecar
# can find them during `tauri dev` and packaged builds.
#
# Usage:
#   .\scripts\build-sidecars.ps1                # build all backends (release)
#   .\scripts\build-sidecars.ps1 -Backend cpu   # only CPU
#   .\scripts\build-sidecars.ps1 -Profile debug # also build debug
#
# Skips backends whose Cargo project doesn't exist yet, so adding a new
# sidecar (vulkan, metal, …) is just a matter of creating crates/flov-whisper-X.

param(
    [string]$Backend = "all",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release"
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\.."
$cratesDir = Join-Path $root "crates"
$targetDir = Join-Path $root "target"
$stageDirs = @(
    (Join-Path $targetDir "debug"),
    (Join-Path $targetDir "release")
)

# Build env for whisper.cpp on Windows + MSVC (+ CUDA 13.x). These used
# to live in .cargo/config.toml but that file applied them globally on
# every platform, breaking macOS/Linux builds. The vars only matter for
# the CUDA sidecar's whisper-rs-sys cmake invocation, but it's harmless
# to set them for cpu/vulkan too — keeps the script simple.
$env:CMAKE_GENERATOR = "Ninja"
$env:CMAKE_MAKE_PROGRAM = "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja/ninja.exe"
$env:CMAKE_GENERATOR_INSTANCE = ""
$env:CUDAFLAGS = "-allow-unsupported-compiler"
$env:CMAKE_CUDA_FLAGS = "-allow-unsupported-compiler -Xcompiler /Zc:preprocessor"
$env:CXXFLAGS = "/Zc:preprocessor"
$env:CFLAGS = "/Zc:preprocessor"
$env:CCCL_IGNORE_MSVC_TRADITIONAL_PREPROCESSOR_WARNING = "1"

# Source for runtime DLLs that CUDA / Vulkan / etc. dynamically load.
# Adjust if CUDA installs elsewhere.
# CUDA's install path carries its version, so a hard-coded one silently
# breaks on every toolkit upgrade: the build still succeeds and the
# installer just ships without cuBLAS, leaving the CUDA sidecar unable to
# start on the user's machine. Prefer CUDA_PATH, then the newest toolkit.
function Resolve-CudaBinDir {
    if ($env:CUDA_PATH) {
        $fromEnv = Join-Path $env:CUDA_PATH 'bin\x64'
        if (Test-Path $fromEnv) { return $fromEnv }
    }
    $root = 'C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA'
    if (Test-Path $root) {
        $newest = Get-ChildItem $root -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending | Select-Object -First 1
        if ($newest) { return (Join-Path $newest.FullName 'bin\x64') }
    }
    return $null
}

$cudaBin = Resolve-CudaBinDir
$cudaDlls = @("cublas64_13.dll", "cublasLt64_13.dll")

function Build-One($name) {
    $crate = Join-Path $cratesDir "flov-whisper-$name"
    if (-not (Test-Path $crate)) {
        Write-Host "skip: crates/flov-whisper-$name not found" -ForegroundColor Yellow
        return
    }
    Write-Host ">> building flov-whisper-$name ($Profile)" -ForegroundColor Cyan
    $args = @("build", "--manifest-path", "$crate/Cargo.toml", "--target-dir", $targetDir)
    if ($Profile -eq "release") { $args += "--release" }
    cargo @args
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for flov-whisper-$name" }

    $exe = Join-Path $targetDir "$Profile\flov-whisper-$name.exe"
    if (-not (Test-Path $exe)) { throw "expected output not found: $exe" }

    # Stage in both debug and release so tauri dev (debug flov_app) and
    # packaged builds (release flov) can both find the sidecar.
    foreach ($dst in $stageDirs) {
        if (-not (Test-Path $dst)) { New-Item -ItemType Directory -Path $dst | Out-Null }
        Copy-Item $exe (Join-Path $dst "flov-whisper-$name.exe") -Force
    }

    if ($name -eq "cuda") {
        Stage-CudaDlls
    }
}

function Stage-CudaDlls {
    if (-not $cudaBin -or -not (Test-Path $cudaBin)) {
        Write-Warning "CUDA bin dir not found at $cudaBin — cublas DLLs not staged"
        return
    }
    foreach ($dst in $stageDirs) {
        foreach ($dll in $cudaDlls) {
            $src = Join-Path $cudaBin $dll
            if (Test-Path $src) {
                Copy-Item $src (Join-Path $dst $dll) -Force
            } else {
                Write-Warning "missing: $src"
            }
        }
    }
}

if ($Backend -eq "all") {
    foreach ($d in Get-ChildItem $cratesDir -Directory) {
        if ($d.Name -like "flov-whisper-*") {
            $name = $d.Name -replace "^flov-whisper-", ""
            Build-One $name
        }
    }
} else {
    Build-One $Backend
}

Write-Host "done." -ForegroundColor Green
