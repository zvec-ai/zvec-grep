param([switch]$DriverOnly)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

# The hosted GPU image is a Data Science VM, not the standard Windows CI image.
# Provision only the ephemeral Actions host; never reboot it.
if ($env:GITHUB_ACTIONS -ne "true" -or $env:OS -ne "Windows_NT") {
    throw "This helper is only for ephemeral Windows GitHub Actions runners"
}

# Fixed Data Center driver for T4 / Server 2022 / CUDA 13.1. Toolkit and
# runtime-only installers do not update the GPU image's old NVIDIA driver.
# https://www.microsoft.com/en-us/download/details.aspx?id=108602
$driverVersion = [version]"591.59"
$driverUrl = "https://download.microsoft.com/download/8920fc6c-3546-458b-99d9-647c4e8621a4/591.59-data-center-tesla-desktop-win10-win11-64bit-dch-international.exe"

function Get-GpuDriverVersions {
    # Resolve again after installation: do not keep using an old NVSMI path.
    $smi = @(
        (Join-Path $env:SystemRoot "System32/nvidia-smi.exe"),
        (Join-Path $env:ProgramFiles "NVIDIA Corporation/NVSMI/nvidia-smi.exe")
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if (-not $smi) { $smi = (Get-Command nvidia-smi.exe -ErrorAction Stop).Source }
    $output = & $smi --query-gpu=driver_version --format=csv,noheader
    if ($LASTEXITCODE -ne 0) { throw "nvidia-smi driver query failed (exit code $LASTEXITCODE)" }
    $versions = @($output | ForEach-Object { [version]($_.Trim()) })
    if ($versions.Count -eq 0) { throw "nvidia-smi did not report any GPU drivers" }
    Write-Host "Active NVIDIA driver versions: $($versions -join ', ')"
    return $versions
}

function Assert-DriverInstallExitCode([int]$Code) {
    # NVIDIA uses 1 for reboot-required; also reject Windows Installer's
    # reboot-required / reboot-initiated results. Never restart this runner.
    if ($Code -in @(1, 3010, 1641)) {
        throw "NVIDIA driver installation requires a reboot (exit code $Code). This job will not reboot the runner; CUDA verification cannot continue."
    }
    if ($Code -ne 0) { throw "NVIDIA driver installation failed (exit code $Code)" }
}

$versions = @(Get-GpuDriverVersions)
if (@($versions | Where-Object { $_ -lt $driverVersion }).Count -ne 0) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Updating the NVIDIA driver requires an elevated ephemeral GPU runner"
    }
    $driverInstaller = Join-Path $env:RUNNER_TEMP "zg-nvidia-$driverVersion.exe"
    & curl.exe --fail --location --proto '=https' --retry 3 --max-time 600 `
        $driverUrl --output $driverInstaller
    if ($LASTEXITCODE -ne 0) { throw "Downloading NVIDIA driver $driverVersion failed" }
    # No official checksum is published for this fixed download. Authenticate
    # the signed installer before executing it and record its hash for audit.
    $signature = Get-AuthenticodeSignature -FilePath $driverInstaller
    if ($signature.Status -ne "Valid" -or -not $signature.SignerCertificate -or
        $signature.SignerCertificate.GetNameInfo([Security.Cryptography.X509Certificates.X509NameType]::SimpleName, $false) -ne "NVIDIA Corporation") {
        throw "NVIDIA driver installer signature is not trusted"
    }
    Write-Host "NVIDIA installer SHA256: $((Get-FileHash -Algorithm SHA256 -LiteralPath $driverInstaller).Hash)"
    # NVIDIA's own Windows CI invokes the self-extracting installer with these
    # switches. /noreboot is mandatory, even on this disposable runner.
    # https://github.com/NVIDIA/cuda-python/blob/main/ci/tools/install_gpu_driver.ps1
    $result = Start-Process -FilePath $driverInstaller `
        -ArgumentList @('/s', '/noeula', '/noreboot') -Wait -PassThru
    Assert-DriverInstallExitCode $result.ExitCode
    $versions = @(Get-GpuDriverVersions)
    if (@($versions | Where-Object { $_ -lt $driverVersion }).Count -ne 0) {
        throw "NVIDIA driver $driverVersion is not active after installation; a reboot may be required. This job will not reboot the runner."
    }
}

# A successful nvidia-smi query is not proof that CUDA can initialize. Load
# the system driver (not a toolkit DLL), and verify its API and real devices.
@'
import ctypes
import os

cuda = ctypes.WinDLL(os.path.join(os.environ["SystemRoot"], "System32", "nvcuda.dll"))
cuda.cuGetErrorString.argtypes = [ctypes.c_int, ctypes.POINTER(ctypes.c_char_p)]
cuda.cuGetErrorString.restype = ctypes.c_int
cuda.cuInit.argtypes = [ctypes.c_uint]
cuda.cuInit.restype = ctypes.c_int
cuda.cuDriverGetVersion.argtypes = [ctypes.POINTER(ctypes.c_int)]
cuda.cuDriverGetVersion.restype = ctypes.c_int
cuda.cuDeviceGetCount.argtypes = [ctypes.POINTER(ctypes.c_int)]
cuda.cuDeviceGetCount.restype = ctypes.c_int

def check(code, operation):
    if code:
        message = ctypes.c_char_p()
        cuda.cuGetErrorString(code, ctypes.byref(message))
        detail = message.value.decode("utf-8", errors="replace") if message.value else "unknown error"
        raise RuntimeError(f"{operation} failed: CUDA error {code}: {detail}. No automatic reboot will be attempted.")

version = ctypes.c_int()
check(cuda.cuDriverGetVersion(ctypes.byref(version)), "cuDriverGetVersion")
if version.value < 13010:
    raise RuntimeError(f"Active CUDA driver API {version.value} does not support CUDA 13.1; no automatic reboot will be attempted")
check(cuda.cuInit(0), "cuInit")
count = ctypes.c_int()
check(cuda.cuDeviceGetCount(ctypes.byref(count)), "cuDeviceGetCount")
if count.value < 1:
    raise RuntimeError("CUDA driver initialized but no GPU devices are available")
print(f"Verified NVIDIA driver: CUDA API {version.value}, {count.value} GPU device(s)", flush=True)
'@ | python -
if ($LASTEXITCODE -ne 0) { throw "Windows CUDA driver verification failed; this job will not reboot the runner" }
if ($DriverOnly) { return }

# The nextest install action explicitly uses pwsh, not Windows PowerShell 5.
# Provision a pinned portable runtime for this job without changing the VM image.
# SHA256: https://github.com/PowerShell/PowerShell/releases/download/v7.6.7/hashes.sha256
$powerShellVersion = "7.6.7"
$powerShellSha256 = "d2f952ebdbf24386cb8923553aa58e41a846db35a6155ea04c58142a4b16dbb7"
$powerShellDirectory = Join-Path $env:RUNNER_TEMP "zg-pwsh-$powerShellVersion"
$powerShellArchive = "$powerShellDirectory.zip"
& curl.exe --fail --location --proto '=https' --retry 3 --max-time 300 `
    "https://github.com/PowerShell/PowerShell/releases/download/v$powerShellVersion/PowerShell-$powerShellVersion-win-x64.zip" `
    --output $powerShellArchive
if ($LASTEXITCODE -ne 0) { throw "Downloading PowerShell $powerShellVersion failed" }
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $powerShellArchive).Hash -ne $powerShellSha256) {
    throw "PowerShell archive SHA256 mismatch"
}
Expand-Archive -LiteralPath $powerShellArchive -DestinationPath $powerShellDirectory -Force
$powerShellExecutable = Join-Path $powerShellDirectory "pwsh.exe"
$activePowerShellVersion = & $powerShellExecutable -NoLogo -NoProfile -NonInteractive `
    -Command '$PSVersionTable.PSVersion.ToString()'
if ($LASTEXITCODE -ne 0 -or "$activePowerShellVersion".Trim() -ne $powerShellVersion) {
    throw "The pinned PowerShell $powerShellVersion runtime could not start"
}
$powerShellDirectory | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append
Write-Host "Verified PowerShell $activePowerShellVersion for subsequent Actions steps"

# System32/bash.exe is the WSL launcher on this image, with no distro installed.
# Prefer Git Bash for the Rust setup action and other cross-platform steps.
$gitBash = Join-Path $env:ProgramFiles "Git/bin"
if (-not (Test-Path -LiteralPath (Join-Path $gitBash "bash.exe"))) {
    throw "Git Bash is required on the Windows GPU runner"
}
$gitBash | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append
$vswhere = "${env:ProgramFiles(x86)}/Microsoft Visual Studio/Installer/vswhere.exe"
$components = @(
    # Rust linking and other C/C++ crates still need MSVC + the Windows SDK.
    # llama's pinned SDK needs neither CUDA 12's older compiler nor bindgen.
    "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
    "Microsoft.VisualStudio.Component.Windows11SDK.26100"
)

function Find-Compiler {
    if (Test-Path -LiteralPath $vswhere) {
        & $vswhere -latest -products '*' -version '[17.0,18.0)' `
            -requires $components -property installationPath
    }
}

$installation = Find-Compiler
if (-not $installation) {
    $installer = Join-Path $env:RUNNER_TEMP "zg-vs2022-build-tools.exe"
    & curl.exe --fail --location --retry 3 --max-time 300 `
        https://aka.ms/vs/17/release/vs_BuildTools.exe --output $installer
    if ($LASTEXITCODE -ne 0) { throw "Downloading Visual Studio Build Tools failed" }
    $signature = Get-AuthenticodeSignature -FilePath $installer
    if ($signature.Status -ne "Valid" -or $signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
        throw "Visual Studio installer signature is not trusted"
    }
    $existing = if (Test-Path -LiteralPath $vswhere) {
        & $vswhere -latest -products '*' -version '[17.0,18.0)' -property installationPath
    } else { $null }
    $arguments = @('--quiet', '--wait', '--norestart', '--nocache')
    if ($existing) {
        $arguments = @('modify', '--installPath', "`"$existing`"") + $arguments
    }
    foreach ($component in $components) { $arguments += @('--add', $component) }
    $result = Start-Process $installer -ArgumentList $arguments -Wait -PassThru
    if ($result.ExitCode -notin @(0, 3010)) {
        throw "Visual Studio Build Tools installation failed: $($result.ExitCode)"
    }
    $installation = Find-Compiler
    if (-not $installation) { throw "Visual Studio 2022 C++ build components are still missing" }
}
Write-Host "Visual Studio Rust/C++ linker tools: $installation"
