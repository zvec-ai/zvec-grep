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

$commands = @("cargo", "dumpbin", "python")
if ($env:LLAMA_CPP_BUILD_MODE -ne "prebuilt") { $commands += @("cmake", "nvcc") }
foreach ($command in $commands) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "required command is unavailable: $command"
    }
}

$cudaToolkitVersion = "13.1"
if ($env:LLAMA_CPP_BUILD_MODE -ne "prebuilt") {
    $nvccOutput = (& nvcc --version 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "nvcc --version failed" }
    $versionMatch = [regex]::Match($nvccOutput, "release\s+([0-9]+(?:\.[0-9]+)*)")
    if (-not $versionMatch.Success) { throw "unable to detect the CUDA toolkit version" }
    $cudaToolkitVersion = $versionMatch.Groups[1].Value
}
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
# The sys crate now invalidates only its native configuration. Keep unrelated
# Rust output and the verified SDK cache when switching native build options.
[IO.Directory]::CreateDirectory($targetDirectory) | Out-Null
$messages = Join-Path $targetDirectory "cuda-build.json"
# Preserve Cargo's UTF-8 bytes and exit code (including non-ASCII local paths)
# instead of passing JSON through PowerShell 5's text/code-page conversion.
# Send ASCII Python source on stdin: Legacy native argument passing strips
# embedded double quotes in python -c; paths stay separate native arguments.
@'
import subprocess, sys
with open(sys.argv[1], "wb") as output:
    result = subprocess.run(sys.argv[2:], stdout=output)
sys.exit(result.returncode)
'@ | python - $messages cargo build --locked --release --package zg --features cuda --message-format=json-render-diagnostics
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

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

    & python scripts/cuda/runtime.py --cargo-messages $messages --destination $libDirectory
    if ($LASTEXITCODE -ne 0) { throw "Staging the selected llama runtime failed" }
    Copy-RequiredFile (Join-Path $profileDirectory "zvec_c_api.dll") (Join-Path $libDirectory "zvec_c_api.dll")

    & python scripts/cuda/prepare-onnxruntime.py --platform win32 `
        --cache-dir (Join-Path $targetDirectory "ort-cuda12") --destination $libDirectory
    if ($LASTEXITCODE -ne 0) { throw "Preparing the ORT CUDA 12 runtime failed" }

    if ($PrimaryOnly) {
        # Incomplete CI output must not replace a complete local bundle.
        $bundleDirectory = Join-Path $profileDirectory "zg-cuda-staging"
    } else {
        if (-not (Test-Path -LiteralPath (Join-Path $libDirectory "cuda12/ggml-cuda.dll"))) {
            $cuda12Root = if ($env:CUDA12_ROOT) { $env:CUDA12_ROOT } else { $env:CUDA_PATH_V12_4 }
            if (-not $cuda12Root) { throw "Set CUDA12_ROOT to the CUDA 12.4 toolkit directory" }
            & python scripts/cuda/build-fallback.py --bundle $stagingDirectory `
                --cargo-messages $messages --cuda-root $cuda12Root
            if ($LASTEXITCODE -ne 0) { throw "Building the CUDA 12.4 fallback failed" }
        }
        if (-not $SkipAudit) {
            & powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass `
                -File (Join-Path $scriptDirectory "check.ps1") -Bundle $stagingDirectory
            if ($LASTEXITCODE -ne 0) {
                throw "Windows CUDA linkage audit failed with exit code $LASTEXITCODE"
            }
        }
    }

    if (Test-Path -LiteralPath $bundleDirectory) {
        Remove-Item -LiteralPath $bundleDirectory -Recurse -Force
    }
    Move-Item -LiteralPath $stagingDirectory -Destination $bundleDirectory
} finally {
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
}

Write-Host "CUDA bundle: $bundleDirectory"
