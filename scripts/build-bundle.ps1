# One-shot release builder. Produces an NSIS installer .exe under
# target/release/bundle/nsis/ that ships:
#   - flov_app.exe (Tauri main app)
#   - flov-whisper-cpu.exe        (always — CPU fallback)
#   - flov-whisper-vulkan.exe     (always — works on most modern GPUs)
#   - flov-whisper-cuda.exe       (only with -IncludeCuda)
#   - cublas64_*.dll, cublasLt64_*.dll  (only with -IncludeCuda)
#
# The Whisper model is NOT bundled — it's ~1.6 GB and the user picks
# which one in Settings → Models, which downloads on demand.
#
# Usage:
#   .\scripts\build-bundle.ps1                  # CPU + Vulkan (default)
#   .\scripts\build-bundle.ps1 -IncludeCuda     # CPU + Vulkan + CUDA (full)
#   .\scripts\build-bundle.ps1 -SkipSidecars    # reuse sidecars, validate manifest
#   .\scripts\build-bundle.ps1 -ReleasePreflight # clean-tree/version/locked checks
#
# Output: full path to the produced installer is printed at the end.

param(
    [switch]$IncludeCuda,
    [switch]$NoCuda,
    [switch]$SkipSidecars,
    [switch]$ReleasePreflight
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\.."
$cratesDir = Join-Path $root "crates"
$targetDir = Join-Path $root "target"
$binDir = Join-Path $root "src-tauri\binaries"
$runtimeDir = Join-Path $binDir "runtime"
$manifestPath = Join-Path $binDir "sidecars-manifest.json"

if ($NoCuda) {
    Write-Warning "-NoCuda is deprecated; CUDA is already opt-in. Use default build without CUDA, or -IncludeCuda for CUDA."
}
if ($NoCuda -and $IncludeCuda) {
    throw "Choose either -IncludeCuda or -NoCuda, not both."
}

# Same Windows-only build env as in build-sidecars.ps1 — needed because
# this script also invokes cargo directly (Build-Sidecar function below).
$env:CMAKE_GENERATOR = "Ninja"
$env:CMAKE_MAKE_PROGRAM = "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja/ninja.exe"
$env:CMAKE_GENERATOR_INSTANCE = ""
$env:CUDAFLAGS = "-allow-unsupported-compiler"
$env:CMAKE_CUDA_FLAGS = "-allow-unsupported-compiler -Xcompiler /Zc:preprocessor"
$env:CXXFLAGS = "/Zc:preprocessor"
$env:CFLAGS = "/Zc:preprocessor"
$env:CCCL_IGNORE_MSVC_TRADITIONAL_PREPROCESSOR_WARNING = "1"

# Tauri's externalBin convention: file must be named `<name>-<triple>.exe`,
# and gets renamed to `<name>.exe` at install time.
$triple = "x86_64-pc-windows-msvc"
$sidecars = @("cpu", "vulkan")
if ($IncludeCuda) { $sidecars += "cuda" }

function ConvertTo-Sha256Hex([string]$Text) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Get-GitSha {
    $sha = (& git -C $root rev-parse --short=12 HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sha)) {
        return "unknown"
    }
    return ($sha -join "").Trim()
}

function Get-SidecarSourceHash($name) {
    $crate = Join-Path $cratesDir "flov-whisper-$name"
    if (-not (Test-Path $crate)) {
        throw "missing crate: $crate"
    }
    $parts = Get-ChildItem $crate -Recurse -File |
        Where-Object { $_.FullName -notmatch "\\target\\" } |
        Sort-Object FullName |
        ForEach-Object {
            $rel = [System.IO.Path]::GetRelativePath($root, $_.FullName).Replace("\", "/")
            $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            "${rel}:${hash}"
        }
    return ConvertTo-Sha256Hex ($parts -join "`n")
}

function New-SidecarManifestEntry($name) {
    $fileName = "flov-whisper-$name-$triple.exe"
    $path = Join-Path $binDir $fileName
    if (-not (Test-Path $path)) {
        throw "cannot manifest missing staged sidecar: $path"
    }
    $item = Get-Item $path
    [ordered]@{
        backend = $name
        target = $triple
        fileName = $fileName
        sourceGitSha = Get-GitSha
        sourceHash = Get-SidecarSourceHash $name
        builtAtUtc = $item.LastWriteTimeUtc.ToString("o")
        size = $item.Length
        sha256 = (Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

function Write-SidecarManifest($requiredSidecars) {
    $manifest = [ordered]@{
        schema = 1
        generatedAtUtc = (Get-Date).ToUniversalTime().ToString("o")
        target = $triple
        includeCuda = [bool]$IncludeCuda
        sidecars = @($requiredSidecars | ForEach-Object { New-SidecarManifestEntry $_ })
    }
    ($manifest | ConvertTo-Json -Depth 10) | Out-File $manifestPath -Encoding utf8 -Force
    Write-Host "   wrote sidecars-manifest.json" -ForegroundColor DarkGray
}

function Assert-SidecarManifestFresh($requiredSidecars) {
    if (-not (Test-Path $manifestPath)) {
        throw "-SkipSidecars requires $manifestPath. Rebuild sidecars once without -SkipSidecars to create a fresh manifest."
    }
    $manifest = Get-Content -Raw $manifestPath | ConvertFrom-Json
    if ($manifest.schema -ne 1) {
        throw "unsupported sidecar manifest schema: $($manifest.schema)"
    }
    foreach ($name in $requiredSidecars) {
        $entry = @($manifest.sidecars) | Where-Object { $_.backend -eq $name -and $_.target -eq $triple } | Select-Object -First 1
        if (-not $entry) {
            throw "sidecar manifest missing backend '$name' for $triple"
        }
        $currentSourceHash = Get-SidecarSourceHash $name
        if ($entry.sourceHash -ne $currentSourceHash) {
            throw "sidecar '$name' source hash changed since manifest was written; rebuild without -SkipSidecars"
        }
        $path = Join-Path $binDir $entry.fileName
        if (-not (Test-Path $path)) {
            throw "sidecar manifest points to missing file: $path"
        }
        $item = Get-Item $path
        $hash = (Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ([int64]$entry.size -ne [int64]$item.Length -or $entry.sha256 -ne $hash) {
            throw "sidecar '$name' binary differs from manifest; rebuild without -SkipSidecars"
        }
    }
    Write-Host "   sidecar manifest validated" -ForegroundColor DarkGray
}

function Get-TomlVersion($path) {
    $match = Select-String -Path $path -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $match) { throw "version not found in $path" }
    return $match.Matches[0].Groups[1].Value
}

function Invoke-ReleasePreflight {
    Write-Host ">> release preflight" -ForegroundColor Cyan
    $dirty = (& git -C $root status --porcelain)
    if ($dirty) {
        throw "release preflight requires a clean git tree"
    }
    $cargoVersion = Get-TomlVersion (Join-Path $root "src-tauri\Cargo.toml")
    $tauriVersion = (Get-Content -Raw (Join-Path $root "src-tauri\tauri.conf.json") | ConvertFrom-Json).version
    if ($cargoVersion -ne $tauriVersion) {
        throw "version mismatch: Cargo.toml=$cargoVersion tauri.conf.json=$tauriVersion"
    }
    $lockText = Get-Content -Raw (Join-Path $root "Cargo.lock")
    if ($lockText -notmatch "name = `"flov_app`"[\s\S]*?version = `"$([regex]::Escape($cargoVersion))`"") {
        throw "Cargo.lock does not contain flov_app $cargoVersion"
    }
    $tags = (& git -C $root tag --points-at HEAD)
    if ($tags -notcontains $cargoVersion) {
        throw "HEAD is not tagged with $cargoVersion"
    }
    Push-Location $root
    try {
        npm ci --prefix ui
        if ($LASTEXITCODE -ne 0) { throw "npm ci failed" }
        cargo check --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo check --locked failed" }
    } finally {
        Pop-Location
    }
}

function Build-Sidecar($name) {
    $crate = Join-Path $cratesDir "flov-whisper-$name"
    if (-not (Test-Path $crate)) {
        throw "missing crate: $crate"
    }
    Write-Host ">> building flov-whisper-$name (release)" -ForegroundColor Cyan
    cargo build --locked --release --manifest-path "$crate\Cargo.toml" --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for flov-whisper-$name" }
}

function Stage-Sidecar($name) {
    $src = Join-Path $targetDir "release\flov-whisper-$name.exe"
    if (-not (Test-Path $src)) { throw "expected sidecar not found: $src" }
    $dst = Join-Path $binDir "flov-whisper-$name-$triple.exe"
    Copy-Item $src $dst -Force
    Write-Host "   staged $dst" -ForegroundColor DarkGray
}

function Stage-CudaDlls {
    $cudaBin = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.2\bin\x64"
    if (-not (Test-Path $cudaBin)) {
        throw "CUDA requested but CUDA bin dir not found at $cudaBin"
    }
    $missing = @()
    foreach ($dll in @("cublas64_13.dll", "cublasLt64_13.dll")) {
        $src = Join-Path $cudaBin $dll
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $runtimeDir $dll) -Force
            Write-Host "   staged runtime\$dll" -ForegroundColor DarkGray
        } else {
            $missing += $src
        }
    }
    if ($missing.Count -gt 0) {
        throw "CUDA requested but required cuBLAS DLLs are missing: $($missing -join ', ')"
    }
}

# Visual C++ Redistributable (2015-2022, x64). Required by ALL sidecars —
# they import MSVCP140.dll / VCRUNTIME140.dll. Modern Win11 usually has it
# (any C++ app installs it), but freshly imaged boxes do not, so we ship
# it and run silently in the NSIS post-install hook.
#
# Permanent Microsoft URL — always serves the latest patched build.
function Stage-VCRedist {
    $dst = Join-Path $runtimeDir "vc_redist.x64.exe"
    $url = "https://aka.ms/vs/17/release/vc_redist.x64.exe"
    Write-Host ">> downloading vc_redist.x64.exe (latest)" -ForegroundColor Cyan
    try {
        Invoke-WebRequest -Uri $url -OutFile $dst -UseBasicParsing -ErrorAction Stop
        $sz = [math]::Round((Get-Item $dst).Length / 1MB, 1)
        $hash = (Get-FileHash $dst -Algorithm SHA256).Hash.ToLowerInvariant()
        $meta = [ordered]@{
            url = $url
            downloadedAtUtc = (Get-Date).ToUniversalTime().ToString("o")
            size = (Get-Item $dst).Length
            sha256 = $hash
        }
        ($meta | ConvertTo-Json -Depth 4) | Out-File (Join-Path $runtimeDir "vc_redist.x64.json") -Encoding utf8 -Force
        Write-Host "   staged runtime\vc_redist.x64.exe ($sz MB)" -ForegroundColor DarkGray
        Write-Host "   vc_redist sha256: $hash" -ForegroundColor DarkGray
    } catch {
        throw "vc_redist download failed: $_"
    }
}

# ── 0. Optional release preflight ────────────────────────────────────
if ($ReleasePreflight) {
    Invoke-ReleasePreflight
}

# ── 1. Build sidecars ────────────────────────────────────────────────
if (-not $SkipSidecars) {
    foreach ($name in $sidecars) {
        Build-Sidecar $name
    }
}

# ── 2. Stage with Tauri's expected naming ────────────────────────────
if (-not (Test-Path $binDir)) { New-Item -ItemType Directory -Path $binDir | Out-Null }
if (-not (Test-Path $runtimeDir)) { New-Item -ItemType Directory -Path $runtimeDir | Out-Null }

# Clean previous staging so an aborted CUDA build doesn't smuggle stale
# sidecars into the bundle.
Get-ChildItem $binDir -Filter "*-$triple.exe" -ErrorAction SilentlyContinue | Remove-Item -Force
Get-ChildItem $runtimeDir -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne ".gitkeep" } |
    Remove-Item -Force

foreach ($name in $sidecars) {
    Stage-Sidecar $name
}
if ($IncludeCuda) {
    Stage-CudaDlls
}
if ($SkipSidecars) {
    Assert-SidecarManifestFresh $sidecars
} else {
    Write-SidecarManifest $sidecars
}
Stage-VCRedist

# Patch tauri.conf.json's externalBin only if CUDA is requested — Tauri
# fails the bundle if it lists a binary that isn't on disk. We use a
# scratch override file (`tauri.bundle.conf.json`) merged via -c to keep
# the source config clean.
$cfgOverride = Join-Path $root "src-tauri\tauri.bundle.conf.json"
if ($IncludeCuda) {
    $patch = @{
        bundle = @{
            externalBin = @(
                "binaries/flov-whisper-cpu",
                "binaries/flov-whisper-vulkan",
                "binaries/flov-whisper-cuda"
            )
        }
    }
    ($patch | ConvertTo-Json -Depth 10) | Out-File $cfgOverride -Encoding utf8 -Force
} elseif (Test-Path $cfgOverride) {
    Remove-Item $cfgOverride -Force
}

# ── 3. Build the bundle ──────────────────────────────────────────────
Write-Host ">> tauri build (NSIS installer)" -ForegroundColor Cyan
$tauri = Join-Path $root "ui\node_modules\.bin\tauri.cmd"
$args = @("build", "--bundles", "nsis")
if ($IncludeCuda) {
    $args += @("-c", $cfgOverride)
}

# Tauri CLI must run with the repo root as cwd so it picks up
# src-tauri/tauri.conf.json regardless of where this script was invoked.
Push-Location $root
try {
    & $tauri @args
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
} finally {
    Pop-Location
}

# ── 4. Report installer location ─────────────────────────────────────
$nsisDir = Join-Path $targetDir "release\bundle\nsis"
$installer = Get-ChildItem $nsisDir -Filter "*.exe" -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($installer) {
    Write-Host "`ndone." -ForegroundColor Green
    Write-Host "Installer: $($installer.FullName)" -ForegroundColor Green
    Write-Host "Size: $([math]::Round($installer.Length / 1MB, 1)) MB" -ForegroundColor Green
} else {
    Write-Warning "Tauri reported success but no installer found in $nsisDir"
}
