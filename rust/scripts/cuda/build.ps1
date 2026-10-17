param(
    [switch]$SkipAudit,
    [switch]$PrimaryOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location (Join-Path $scriptDirectory "../..")

if ($env:OS -ne "Windows_NT") {
    throw "Windows CUDA builds require Windows"
}

foreach ($command in @("cargo", "cmake", "nvcc", "dumpbin", "python")) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "required command is unavailable: $command"
    }
}

$nvccOutput = (& nvcc --version 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "nvcc --version failed"
}
$versionMatch = [regex]::Match($nvccOutput, "release\s+([0-9]+(?:\.[0-9]+)*)")
if (-not $versionMatch.Success) {
    throw "unable to detect the CUDA toolkit version"
}
$cudaToolkitVersion = $versionMatch.Groups[1].Value
if ($cudaToolkitVersion -ne "13.1") {
    throw "the primary GGUF backend requires CUDA 13.1 (found $cudaToolkitVersion)"
}
if ([string]::IsNullOrWhiteSpace($env:CMAKE_CUDA_ARCHITECTURES)) {
    $env:CMAKE_CUDA_ARCHITECTURES = "75-real;80-real;86-real;89-real;90-real;90-virtual"
}
if ([string]::IsNullOrWhiteSpace($env:CMAKE_BUILD_PARALLEL_LEVEL)) {
    $env:CMAKE_BUILD_PARALLEL_LEVEL = [Environment]::ProcessorCount.ToString()
}

Write-Host "GGUF toolkit: $cudaToolkitVersion; ONNX Runtime: official CUDA 12 dynamic runtime"
Write-Host "CUDA architectures: $env:CMAKE_CUDA_ARCHITECTURES"

$targetDirectoryValue = if ([string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    "target"
} else {
    $env:CARGO_TARGET_DIR
}
$targetDirectory = [IO.Path]::GetFullPath($targetDirectoryValue)
$buildConfiguration = "cuda_toolkit=$cudaToolkitVersion;ort=1.28.0-cuda12-dynamic;architectures=$env:CMAKE_CUDA_ARCHITECTURES"
$buildConfigurationFile = Join-Path $targetDirectory ".zvec-grep-cuda-build-config"
$previousBuildConfiguration = if (Test-Path -LiteralPath $buildConfigurationFile -PathType Leaf) {
    (Get-Content -LiteralPath $buildConfigurationFile -TotalCount 1)
} else {
    ""
}
if ($previousBuildConfiguration -ne $buildConfiguration -and
    (Test-Path -LiteralPath (Join-Path $targetDirectory "CACHEDIR.TAG") -PathType Leaf)) {
    Write-Host "CUDA build configuration changed; rebuilding the target directory"
    & cargo clean --target-dir $targetDirectory
    if ($LASTEXITCODE -ne 0) {
        throw "cargo clean failed with exit code $LASTEXITCODE"
    }
}

& cargo build --locked --release --package zg --features cuda
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}
[IO.Directory]::CreateDirectory($targetDirectory) | Out-Null
[IO.File]::WriteAllText($buildConfigurationFile, "$buildConfiguration`n")

$profileDirectory = Join-Path $targetDirectory "release"
$bundleDirectory = Join-Path $profileDirectory "zg-cuda"
$stagingDirectory = Join-Path $profileDirectory (".zg-cuda.{0}.{1}" -f $PID, [guid]::NewGuid().ToString("N"))
$binDirectory = Join-Path $stagingDirectory "bin"
$libDirectory = Join-Path $stagingDirectory "lib"
[IO.Directory]::CreateDirectory($binDirectory) | Out-Null
[IO.Directory]::CreateDirectory($libDirectory) | Out-Null

function Copy-RequiredFile {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "required Windows CUDA bundle file is missing: $Source"
    }
    Copy-Item -LiteralPath $Source -Destination $Destination
}

try {
    Copy-RequiredFile (Join-Path $profileDirectory "zg.exe") (Join-Path $binDirectory "zg.exe")

    $backend = Get-ChildItem -LiteralPath (Join-Path $profileDirectory "build") `
        -Recurse -File -Filter "ggml-cuda.dll" |
        Where-Object { $_.FullName -match "[\\/]llama-cpp-sys-2-[^\\/]+[\\/]out[\\/]backends[\\/]" } |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($null -eq $backend) {
        throw "unable to locate the llama.cpp CUDA backend module"
    }
    $backendLibraries = @(Get-ChildItem -LiteralPath $backend.Directory.FullName -File -Filter "ggml-*.dll")
    if ($backendLibraries.Count -eq 0) {
        throw "llama.cpp backend DLLs are missing"
    }
    foreach ($library in $backendLibraries) {
        Copy-Item -LiteralPath $library.FullName -Destination (Join-Path $libDirectory $library.Name)
    }

    foreach ($name in @(
        "llama.dll",
        "ggml.dll",
        "ggml-base.dll",
        "zvec_c_api.dll"
    )) {
        Copy-RequiredFile (Join-Path $profileDirectory $name) (Join-Path $libDirectory $name)
    }

    & python scripts/cuda/prepare-onnxruntime.py --platform win32 `
        --cache-dir (Join-Path $targetDirectory "ort-cuda12") --destination $libDirectory
    if ($LASTEXITCODE -ne 0) { throw "Preparing the ORT CUDA 12 runtime failed" }

    $dataDirectory = Join-Path $profileDirectory "data"
    if (-not (Test-Path -LiteralPath $dataDirectory -PathType Container)) {
        throw "zvec runtime data directory is missing: $dataDirectory"
    }
    Copy-Item -LiteralPath $dataDirectory -Destination (Join-Path $libDirectory "data") -Recurse

    if (Test-Path -LiteralPath $bundleDirectory) {
        Remove-Item -LiteralPath $bundleDirectory -Recurse -Force
    }
    Move-Item -LiteralPath $stagingDirectory -Destination $bundleDirectory
} finally {
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
}

if (-not $PrimaryOnly) {
    $cuda12Root = if ($env:CUDA12_ROOT) { $env:CUDA12_ROOT } else { $env:CUDA_PATH_V12_4 }
    if (-not $cuda12Root) { throw "Set CUDA12_ROOT to the CUDA 12.4 toolkit directory" }
    & python scripts/cuda/build-fallback.py --bundle $bundleDirectory --cuda-root $cuda12Root
    if ($LASTEXITCODE -ne 0) { throw "Building the CUDA 12.4 fallback failed" }
}
if (-not $SkipAudit -and -not $PrimaryOnly) {
    & powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $scriptDirectory "check.ps1") `
        -Bundle $bundleDirectory
    if ($LASTEXITCODE -ne 0) {
        throw "Windows CUDA linkage audit failed with exit code $LASTEXITCODE"
    }
}
Write-Host "CUDA bundle: $bundleDirectory"
