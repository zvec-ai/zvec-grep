param(
    [string]$Bundle = "target/release/zg-cuda"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$bundleDirectory = [IO.Path]::GetFullPath($Bundle)
$binary = Join-Path $bundleDirectory "bin/zg.exe"
$libDirectory = Join-Path $bundleDirectory "lib"
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "zg binary is missing: $binary"
}
if (-not (Get-Command dumpbin -ErrorAction SilentlyContinue)) {
    throw "dumpbin is required; run from an MSVC developer environment"
}

function Require-File {
    param([Parameter(Mandatory = $true)][string]$Pattern)
    if (@(Get-ChildItem -LiteralPath $libDirectory -File -Filter $Pattern).Count -eq 0) {
        throw "missing packaged shared library: $Pattern"
    }
}

function Get-Imports {
    param([Parameter(Mandatory = $true)][string]$Path, [switch]$StartupOnly)
    $output = (& dumpbin /nologo /dependents $Path 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "dumpbin failed for $Path"
    }
    $imports = @()
    foreach ($line in ($output -split "`r?`n")) {
        if ($StartupOnly -and $line -match "delay load dependencies") { break }
        if ($line -match "^\s+([A-Za-z0-9_.+-]+\.dll)\s*$") {
            $imports += $Matches[1].ToLowerInvariant()
        }
    }
    return $imports | Sort-Object -Unique
}

function Require-Import {
    param(
        [Parameter(Mandatory = $true)][string[]]$Imports,
        [Parameter(Mandatory = $true)][string]$Pattern,
        [Parameter(Mandatory = $true)][string]$Path
    )
    if (-not ($Imports | Where-Object { $_ -match $Pattern })) {
        throw "missing expected dynamic dependency in ${Path}: $Pattern (actual imports: $($Imports -join ', '))"
    }
}

$requiredPatterns = @(
    "zvec_c_api.dll",
    "llama.dll",
    "ggml.dll",
    "ggml-base.dll",
    "ggml-cpu*.dll",
    "ggml-cuda.dll",
    "onnxruntime.dll",
    "onnxruntime_providers_shared.dll",
    "onnxruntime_providers_cuda.dll"
)
foreach ($pattern in $requiredPatterns) {
    Require-File $pattern
}
$fallback = Join-Path $libDirectory "cuda12/ggml-cuda.dll"
if (-not (Test-Path -LiteralPath $fallback -PathType Leaf)) {
    throw "CUDA 12.4 fallback plugin is missing"
}
if (@(Get-ChildItem -LiteralPath (Join-Path $libDirectory "cuda12") -File -Recurse).Count -ne 1) {
    throw "cuda12 must contain only the fallback CUDA plugin"
}

$unexpectedBuildFiles = @(Get-ChildItem -LiteralPath $bundleDirectory -File -Recurse |
    Where-Object { $_.Extension -in @(".lib", ".exp", ".pdb") })
if ($unexpectedBuildFiles.Count -ne 0) {
    throw "Windows CUDA bundle contains build-only files: $($unexpectedBuildFiles.Name -join ', ')"
}

$hostCudaPattern = "^(?:cudart|cublas|cudnn|nvcuda|nvrtc|cufft|curand)[a-z0-9_.-]*\.dll$"
$startupFiles = @(
    $binary,
    (Join-Path $libDirectory "zvec_c_api.dll"),
    (Join-Path $libDirectory "llama.dll"),
    (Join-Path $libDirectory "ggml.dll"),
    (Join-Path $libDirectory "ggml-base.dll"),
    (Join-Path $libDirectory "onnxruntime.dll"),
    (Join-Path $libDirectory "onnxruntime_providers_shared.dll")
)
$startupFiles += @(Get-ChildItem -LiteralPath $libDirectory -File -Filter "ggml-cpu*.dll" |
    ForEach-Object { $_.FullName })
foreach ($path in $startupFiles) {
    $imports = @(Get-Imports $path)
    $cudaImports = @($imports | Where-Object { $_ -match $hostCudaPattern })
    if ($cudaImports.Count -ne 0) {
        throw "CPU/startup library must not require CUDA: $path ($($cudaImports -join ', '))"
    }
}

$binaryImports = @(Get-Imports $binary)
Require-Import $binaryImports "^zvec_c_api\.dll$" $binary
Require-Import $binaryImports "^llama\.dll$" $binary
$packagedDlls = @(Get-ChildItem -LiteralPath $libDirectory -File -Filter "*.dll" |
    ForEach-Object { $_.Name.ToLowerInvariant() })
$startupImports = @(Get-Imports $binary -StartupOnly)
if (@($startupImports | Where-Object { $_ -in $packagedDlls }).Count -ne 0) {
    throw "bin/zg.exe must delay-load packaged DLLs so it can locate sibling lib/"
}

$cudaBackend = Join-Path $libDirectory "ggml-cuda.dll"
$cudaImports = @(Get-Imports $cudaBackend)
# The driver may be loaded through the CUDA runtime rather than imported by
# this DLL. cuBLAS is shared on Windows even when cudart is statically linked.
Require-Import $cudaImports "^cublas64_13\.dll$" $cudaBackend
$fallbackImports = @(Get-Imports $fallback)
Require-Import $fallbackImports "^cublas64_12\.dll$" $fallback
Require-Import $fallbackImports "^ggml-base\.dll$" $fallback

$provider = Join-Path $libDirectory "onnxruntime_providers_cuda.dll"
$providerImports = @(Get-Imports $provider)
Require-Import $providerImports "^cublas64_12\.dll$" $provider
Require-Import $providerImports "^cudart64_12\.dll$" $provider
# ORT 1.28 loads cuDNN/cuFFT on demand rather than importing them at startup.
if (-not ($providerImports | Where-Object { $_ -match $hostCudaPattern })) {
    throw "ONNX Runtime CUDA provider has no CUDA runtime dependency"
}

$packagedHostCuda = @(Get-ChildItem -LiteralPath $bundleDirectory -File -Recurse |
    Where-Object { $_.Name.ToLowerInvariant() -match $hostCudaPattern })
if ($packagedHostCuda.Count -ne 0) {
    throw "NVIDIA runtime DLLs must remain host dependencies: $($packagedHostCuda.Name -join ', ')"
}

Write-Host "Windows CUDA bundle linkage policy passed for $bundleDirectory"
Write-Host "zg dependencies:"
$binaryImports | ForEach-Object { Write-Host $_ }
Write-Host "llama.cpp CUDA backend dependencies:"
$cudaImports | ForEach-Object { Write-Host $_ }
Write-Host "ONNX Runtime CUDA provider dependencies:"
$providerImports | ForEach-Object { Write-Host $_ }
