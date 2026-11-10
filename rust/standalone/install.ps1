[CmdletBinding()]
param(
    [string]$Version,
    [string]$ActivateVersion,
    [string]$Accelerator = $env:ZG_INSTALL_ACCELERATOR,
    [string]$InstallDir = $env:ZG_INSTALL_DIR,
    [string]$DaemonHome,
    [string]$TokenFile,
    [switch]$NoModifyPath,
    [switch]$ModifyPath,
    [switch]$SkipAgentUpgrade,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Use install.sh on Unix platforms.' }
if ($NoModifyPath -and $ModifyPath) { throw 'Choose either -NoModifyPath or -ModifyPath.' }
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'zvec-grep\standalone' }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$state = @{}
if ($ActivateVersion -and ($Version -or $Uninstall)) { throw '-ActivateVersion cannot be combined with -Version or -Uninstall.' }
$statePath = Join-Path $InstallDir 'install-state.tsv'
if (Test-Path -LiteralPath $statePath) {
    foreach ($line in [IO.File]::ReadAllLines($statePath)) {
        $fields = $line.Split("`t")
        if ($fields.Length -eq 2) { $state[$fields[0]] = $fields[1] }
    }
}
$repository = $env:ZG_INSTALL_REPOSITORY
if (-not $repository) { $repository = $state['repository'] }
if (-not $repository) { $repository = 'zvec-ai/zvec-grep' }
if ($repository -notmatch '^[A-Za-z0-9_-][A-Za-z0-9_.-]*/[A-Za-z0-9_-][A-Za-z0-9_.-]*$') { throw 'Repository must be a GitHub owner/name.' }
$BaseUrl = "https://github.com/$repository/releases"
# Tests emulate the fixed GitHub download paths without changing the installed source.
if ($env:ZG_INSTALL_TEST_RELEASE_URL) {
    if ($env:ZG_INSTALL_TEST_RELEASE_URL -notmatch '^http://127\.0\.0\.1:\d+$') { throw 'Invalid test release server.' }
    $BaseUrl = $env:ZG_INSTALL_TEST_RELEASE_URL
}
if (-not $Accelerator) { $Accelerator = $state['accelerator'] }
if (-not $Accelerator) { $Accelerator = 'auto' }
if ($Accelerator -notin @('auto', 'cpu', 'cuda')) { throw 'Accelerator must be auto, cpu, or cuda.' }
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(-[A-Za-z0-9.-]+)?$') { throw 'Invalid version.' }
$changePath = -not $NoModifyPath -and ($ModifyPath -or $state['modify_path'] -ne '0')
$skipAgents = $SkipAgentUpgrade -or $env:ZG_INSTALL_SKIP_AGENT_UPGRADE -eq '1' -or $env:CI -in @('true', '1')
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { $skipAgents = $true }
$arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($arch -notin @('x64', 'arm64')) { throw "Unsupported architecture: $arch" }
$target = "win32-$arch-msvc"
$tempBase = if ($env:ZG_INSTALL_TMPDIR) { $env:ZG_INSTALL_TMPDIR } else { [IO.Path]::GetTempPath() }
$staging = [IO.Path]::GetFullPath((Join-Path $tempBase ('zg-install-' + [Guid]::NewGuid().ToString('N'))))
[IO.Directory]::CreateDirectory($staging) | Out-Null

function Download([string]$Url, [string]$Destination) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination -TimeoutSec 600
}

function Read-ReleaseManifest([string]$Path) {
    $metadata = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::Ordinal)
    $assets = [Collections.Generic.List[object]]::new()
    $identities = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $lines = [IO.File]::ReadAllText($Path).Replace("`r`n", "`n").Split("`n")
    for ($index = 0; $index -lt $lines.Length; $index++) {
        if ($index -eq $lines.Length - 1 -and $lines[$index] -eq '') { continue }
        $fields = $lines[$index].Split("`t")
        if (@('schema', 'version', 'tag', 'commit', 'partial') -ccontains $fields[0]) {
            if ($fields.Length -ne 2 -or $metadata.ContainsKey($fields[0])) { throw "Invalid release manifest: invalid or duplicate release $($fields[0])." }
            $metadata.Add($fields[0], $fields[1])
        } else {
            if ($fields.Length -ne 5) { throw 'Invalid release manifest: invalid archive columns.' }
            $size = 0L
            if ($fields[0] -cnotmatch '\A[a-z0-9]+(-[a-z0-9]+)+\z' -or @('cpu', 'cuda') -cnotcontains $fields[1] -or
                -not $identities.Add("$($fields[0])/$($fields[1])") -or $fields[3] -cnotmatch '\A[a-f0-9]{64}\z' -or
                $fields[4] -cnotmatch '\A[1-9][0-9]*\z' -or -not [long]::TryParse($fields[4], [ref]$size) -or $size -gt 9007199254740991) {
                throw 'Invalid release manifest: invalid or duplicate archive metadata.'
            }
            $assets.Add([pscustomobject]@{ target = $fields[0]; accelerator = $fields[1]; file = $fields[2]; sha256 = $fields[3]; bytes = $size })
        }
    }
    if ($metadata['schema'] -cne '1') { throw 'Invalid release manifest: unsupported schema.' }
    if (-not $metadata['version'] -or $metadata['version'] -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?\z') { throw 'Invalid release manifest: invalid release version.' }
    if (-not $metadata['tag'] -or $metadata['tag'] -cnotmatch '\A[A-Za-z0-9][A-Za-z0-9._-]*\z') { throw 'Invalid release manifest: invalid release tag.' }
    if (-not $metadata['commit'] -or $metadata['commit'] -cnotmatch '\A[a-f0-9]{40}\z') { throw 'Invalid release manifest: invalid source commit.' }
    if (@('true', 'false') -cnotcontains $metadata['partial']) { throw 'Invalid release manifest: invalid partial flag.' }
    if (-not $assets.Count) { throw 'Invalid release manifest: missing archives.' }
    foreach ($asset in $assets) {
        $extension = if ($asset.target.StartsWith('win32-')) { 'zip' } else { 'tar.gz' }
        if ($asset.file -cne "zvec-grep-$($metadata['version'])-$($asset.target)-$($asset.accelerator).$extension") { throw 'Invalid release manifest: invalid archive filename.' }
    }
    return [pscustomobject]@{ schemaVersion = 1; version = $metadata['version']; tag = $metadata['tag']; commit = $metadata['commit']; partial = $metadata['partial'] -ceq 'true'; assets = $assets.ToArray() }
}

function Get-ArchiveSha256([string]$Path) {
    $algorithm = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead($Path)
    try { return [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $stream.Dispose(); $algorithm.Dispose() }
}

function Has-Nvidia {
    $command = Get-Command nvidia-smi -ErrorAction SilentlyContinue
    if (-not $command) { return $false }
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $command.Source
    $start.Arguments = '--query-gpu=name,driver_version --format=csv,noheader'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $start
    try {
        $process.Start() | Out-Null
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(5000)) { $process.Kill(); return $false }
        return $process.ExitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($output.GetAwaiter().GetResult())
    } finally { $process.Dispose() }
}

function Get-UpdatedUserPath([string]$UserPath, [string]$PathDir, [bool]$Remove, [bool]$ChangePath, [bool]$Managed) {
    if (-not $ChangePath -or ($Remove -and -not $Managed)) { return $null }
    $parts = @($UserPath -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $PathDir })
    if (-not $Remove) { $parts = @($PathDir) + $parts }
    return ($parts -join ';')
}

function Test-StagedVersion([string]$Executable, [string]$Expected) {
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.Arguments = '--version'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $start
    try {
        $process.Start() | Out-Null
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) {
            $process.Kill()
            $process.WaitForExit()
            throw 'Staged executable version check timed out.'
        }
        if ($process.ExitCode -ne 0 -or $output.GetAwaiter().GetResult().Trim() -ne $Expected) { throw 'Staged executable version mismatch.' }
    } finally { $process.Dispose() }
}

try {
    $cached = $false
    if ($Uninstall -or $ActivateVersion) {
        $bundle = $state['bundle']
        if ($bundle -notmatch '^\d+\.\d+\.\d+(-[A-Za-z0-9.-]+)?/[A-Za-z0-9-]+-(cpu|cuda)$') { throw 'No valid standalone installation found.' }
        $source = Join-Path $InstallDir $bundle
        if (Test-Path -LiteralPath (Join-Path $source 'bin\zg.exe')) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $staging 'zg') -Recurse
            try { Test-StagedVersion (Join-Path $staging 'zg\bin\zg.exe') $state['version']; $cached = $true } catch { }
        }
        if (-not $cached) {
            if ($ActivateVersion) { throw 'Cached executable is unavailable; repair the installation first.' }
            # Resume a committed uninstall even after part of its cached runtime was deleted.
            Remove-Item -LiteralPath (Join-Path $staging 'zg') -Recurse -Force -ErrorAction SilentlyContinue
            $Version = $state['version']
        }
    }
    if (-not $cached) {
        $manifestUrl = if ($Version) { "$BaseUrl/download/v$Version/manifest.tsv" } else { "$BaseUrl/latest/download/manifest.tsv" }
        $manifestPath = Join-Path $staging 'manifest.tsv'
        Download $manifestUrl $manifestPath
        $manifest = Read-ReleaseManifest $manifestPath
        if ($Version -and $Version -ne $manifest.version) { throw 'Requested version does not match release manifest.' }
        $tag = $manifest.tag
        if ($Version -and $tag -cne "v$Version") { throw 'Release tag does not match requested version.' }
        $Version = $manifest.version
        $variant = 'cpu'
        if ($Accelerator -eq 'cuda' -or ($Accelerator -eq 'auto' -and $arch -eq 'x64' -and (Has-Nvidia))) { $variant = 'cuda' }
        $archive = Join-Path $staging 'archive.zip'
        function Fetch-Bundle([string]$Selected) {
            $assets = @($manifest.assets | Where-Object { $_.target -eq $target -and $_.accelerator -eq $Selected })
            if ($assets.Count -ne 1) { throw "No unique $Selected package for $target." }
            $asset = $assets[0]
            Download "$BaseUrl/download/$tag/$($asset.file)" $archive
            if ((Get-Item -LiteralPath $archive).Length -ne $asset.bytes -or (Get-ArchiveSha256 $archive) -ne $asset.sha256) { throw 'Package checksum mismatch.' }
        }
        try { Fetch-Bundle $variant } catch {
            if ($Accelerator -ne 'auto' -or $variant -ne 'cuda') { throw }
            Write-Warning 'CUDA download failed verification; installing the CPU base.'
            $variant = 'cpu'
            Fetch-Bundle $variant
        }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            foreach ($entry in $zip.Entries) {
                $name = $entry.FullName.Replace('\', '/')
                $mode = ($entry.ExternalAttributes -shr 16) -band 0xF000
                if ($name -notmatch '^zg/' -or $name -match '(^|/)\.\.?(/|$)|:' -or $mode -notin @(0, 0x4000, 0x8000)) { throw "Unsafe archive entry: $name" }
                $destination = [IO.Path]::GetFullPath((Join-Path $staging $name))
                if (-not $destination.StartsWith($staging + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Archive path escapes staging.' }
            }
            foreach ($entry in $zip.Entries) {
                $destination = Join-Path $staging $entry.FullName
                if ($entry.FullName.EndsWith('/')) { [IO.Directory]::CreateDirectory($destination) | Out-Null }
                else {
                    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination)) | Out-Null
                    [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $destination, $false)
                }
            }
        } finally { $zip.Dispose() }
        Test-StagedVersion (Join-Path $staging 'zg\bin\zg.exe') $Version
    }
    $pathDir = Join-Path $InstallDir 'bin'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $alreadyOnPath = @($userPath -split ';' | Where-Object { $_.TrimEnd('\') -eq $pathDir }).Count -gt 0
    $managedPath = $state['path_managed'] -eq '1' -or ($changePath -and -not $Uninstall -and -not $alreadyOnPath)
    $arguments = @('--standalone', '--install-dir', $InstallDir, '--accelerator', $Accelerator, '--repository', $repository)
    if ($managedPath) { $arguments += '--path-managed' }
    if ($Uninstall) { $arguments += '--remove' }
    if ($ActivateVersion) { $arguments += @('--activate', $ActivateVersion) }
    if (-not $changePath) { $arguments += '--no-modify-path' }
    if ($skipAgents) { $arguments += '--skip-agent-upgrade' }
    if ($DaemonHome) { $arguments += @('--home', $DaemonHome) }
    if ($TokenFile) { $arguments += @('--token-file', $TokenFile) }
    & (Join-Path $staging 'zg\bin\zg.exe') @arguments
    if ($LASTEXITCODE -ne 0) { throw 'Standalone activation failed; the previous installation was retained or recovery instructions were printed above.' }
    $updatedPath = Get-UpdatedUserPath ([Environment]::GetEnvironmentVariable('Path', 'User')) $pathDir $Uninstall $changePath $managedPath
    if ($null -ne $updatedPath) {
        [Environment]::SetEnvironmentVariable('Path', $updatedPath, 'User')
        Write-Host 'Open a new terminal and reconnect existing MCP clients.'
    } elseif (-not $Uninstall) { Write-Host "Add $pathDir to PATH to use zg." }
    $existing = Get-Command zg -ErrorAction SilentlyContinue
    if (-not $Uninstall -and $existing -and $existing.Source -ne (Join-Path $pathDir 'zg.exe')) { Write-Warning "The current shell resolves zg to $($existing.Source); check PATH before using the new installation." }
} finally {
    Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
}
