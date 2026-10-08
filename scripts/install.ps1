# One-command native Windows installer. Dial code and release downloads remain private.
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDirectory = "$env:LOCALAPPDATA\Dial\bin",
    [string]$AssetDirectory,
    [switch]$NoPathUpdate
)

function Update-DialProcessPath {
    # Preserve session-specific paths and pick up installers' registry changes immediately.
    $paths = @($env:Path -split ';') + @([Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';') + @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';')
    $env:Path = (($paths | Where-Object { $_ } | Select-Object -Unique) -join ';')
}

function Get-DialGitHubCli {
    Update-DialProcessPath
    $command = Get-Command gh -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $winget = Get-Command winget -ErrorAction SilentlyContinue
    if (-not $winget) { throw 'Windows App Installer (winget) is needed for automatic setup. Install GitHub CLI from https://cli.github.com/, then run this command again.' }
    Write-Host 'Installing GitHub CLI for private release access...'
    & $winget.Source install --id GitHub.cli --exact --source winget --accept-package-agreements --accept-source-agreements | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'GitHub CLI installation failed; no Dial installation was changed.' }
    Update-DialProcessPath
    $command = Get-Command gh -ErrorAction SilentlyContinue
    if (-not $command) { throw 'GitHub CLI was installed but could not be found. Open a new terminal and run this command again.' }
    return $command.Source
}

function Get-DialGitBash {
    $roots = @($env:GIT_INSTALL_ROOT)
    foreach ($key in @('HKCU:\Software\GitForWindows', 'HKLM:\Software\GitForWindows', 'HKLM:\Software\WOW6432Node\GitForWindows')) {
        $item = Get-ItemProperty $key -ErrorAction SilentlyContinue
        if ($item) { $roots += $item.InstallPath }
    }
    if ($env:ProgramFiles) { $roots += Join-Path $env:ProgramFiles 'Git' }
    if (${env:ProgramFiles(x86)}) { $roots += Join-Path ${env:ProgramFiles(x86)} 'Git' }
    if ($env:LOCALAPPDATA) { $roots += Join-Path $env:LOCALAPPDATA 'Programs\Git' }
    if ($env:USERPROFILE) { $roots += Join-Path $env:USERPROFILE 'scoop\apps\git\current' }
    $candidates = @($env:DIAL_BASH)
    foreach ($root in $roots) { if ($root) { $candidates += Join-Path $root 'bin\bash.exe' } }
    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($git) { $candidates += Join-Path (Split-Path $git.Source) '..\bin\bash.exe' }
    foreach ($path in $candidates) {
        if ($path -and (Test-Path $path -PathType Leaf) -and $path -notmatch '\\System32\\') { return (Resolve-Path $path).Path }
    }
    return $null
}

function Initialize-DialGitBash {
    $bash = Get-DialGitBash
    if (-not $bash) {
        $winget = Get-Command winget -ErrorAction SilentlyContinue
        if (-not $winget) { throw 'Install Git for Windows from https://git-scm.com/download/win, then run this command again.' }
        Write-Host 'Installing Git for Windows for command tools...'
        & $winget.Source install --id Git.Git --exact --source winget --scope user --accept-package-agreements --accept-source-agreements | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'Git for Windows installation failed; no Dial installation was changed.' }
        Update-DialProcessPath
        $bash = Get-DialGitBash
        if (-not $bash) { throw 'Git Bash could not be found after installation. Set DIAL_BASH to bash.exe and run this command again.' }
    }
    # Custom/Scoop installs work immediately and in future PowerShell terminals.
    $env:DIAL_BASH = $bash
    [Environment]::SetEnvironmentVariable('DIAL_BASH', $bash, 'User')
}

function Get-DialRelease {
    param([string]$Gh, [string]$Version)
    & $Gh auth status --hostname github.com 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host 'Dial releases are private. Sign in with a GitHub account granted access to KiidxAtlas/dial.'
        & $Gh auth login --hostname github.com --web --git-protocol https | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'GitHub sign-in did not finish. Run this installer again when ready.' }
    }
    & $Gh api repos/KiidxAtlas/dial --silent 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Your GitHub account cannot access KiidxAtlas/dial. Ask the repository owner to grant access, then run this command again.' }
    $endpoint = if ($Version -eq 'latest') { 'repos/KiidxAtlas/dial/releases/latest' } else { "repos/KiidxAtlas/dial/releases/tags/$Version" }
    $json = & $Gh api $endpoint 2>$null
    if ($LASTEXITCODE -ne 0) { throw "Dial release '$Version' has not been published yet. No Dial installation was changed." }
    return ($json | ConvertFrom-Json)
}

function Get-DialBinaryVersion {
    param([string]$Binary)
    $reported = & $Binary --version
    if ($LASTEXITCODE -ne 0) { throw 'The downloaded executable could not start; the existing installation was preserved.' }
    return ($reported -join "`n").Trim()
}

function Install-Dial {
    param([string]$Version, [string]$InstallDirectory, [string]$AssetDirectory, [switch]$NoPathUpdate)
    if (-not [Environment]::Is64BitOperatingSystem) { throw 'Dial requires 64-bit Windows.' }
    if (-not $InstallDirectory) { throw 'LOCALAPPDATA is unavailable; supply -InstallDirectory.' }
    if ($Version -ne 'latest' -and $Version -notmatch '^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') { throw "Invalid release version: $Version" }
    $gh = $null
    if ($AssetDirectory) {
        if ($Version -eq 'latest') { throw 'Offline installs require an explicit release version.' }
    } else {
        $gh = Get-DialGitHubCli
        $release = Get-DialRelease -Gh $gh -Version $Version
        $Version = $release.tag_name
        if ($Version -notmatch '^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') { throw 'The release returned an invalid version.' }
        $required = @("dial-$Version-x86_64-pc-windows-msvc.zip", "dial-$Version-x86_64-pc-windows-msvc.zip.sha256")
        foreach ($asset in $required) {
            if ($release.assets.name -notcontains $asset) { throw "The native Windows download is not published for $Version yet. No Dial installation was changed." }
        }
        Initialize-DialGitBash
    }
    $package = "dial-$Version-x86_64-pc-windows-msvc"
    $archive = "$package.zip"
    $temp = Join-Path ([IO.Path]::GetTempPath()) ("dial-install-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        if ($AssetDirectory) {
            Copy-Item (Join-Path $AssetDirectory $archive) $temp
            Copy-Item (Join-Path $AssetDirectory "$archive.sha256") $temp
        } else {
            & $gh release download $Version --repo KiidxAtlas/dial --pattern $archive --pattern "$archive.sha256" --dir $temp
            if ($LASTEXITCODE -ne 0) { throw 'Release download failed; the existing installation was preserved.' }
        }
        $checksum = (Get-Content (Join-Path $temp "$archive.sha256") -Raw).Trim()
        if ($checksum -notmatch '^([a-fA-F0-9]{64})\s+\*?([^\r\n]+)$' -or $Matches[2] -ne $archive) { throw 'Invalid release checksum file.' }
        $expected = $Matches[1]
        $actual = (Get-FileHash (Join-Path $temp $archive) -Algorithm SHA256).Hash
        if ($actual -ne $expected) { throw 'Checksum verification failed; the existing installation was preserved.' }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead((Join-Path $temp $archive))
        try {
            $entry = $zip.GetEntry("$package/dial.exe")
            if (-not $entry) { throw 'The release archive does not contain dial.exe.' }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $temp 'dial.exe'), $false)
        } finally { $zip.Dispose() }
        $binary = Join-Path $temp 'dial.exe'
        if ((Get-DialBinaryVersion $binary) -ne "dial $($Version.Substring(1))") { throw 'The downloaded executable failed its version check; the existing installation was preserved.' }
        New-Item -ItemType Directory -Path $InstallDirectory -Force | Out-Null
        Copy-Item $binary (Join-Path $InstallDirectory 'dial.exe') -Force
        if (-not $NoPathUpdate) {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $paths = @($userPath -split ';' | Where-Object { $_ })
            if ($paths -notcontains $InstallDirectory) {
                [Environment]::SetEnvironmentVariable('Path', (($paths + $InstallDirectory) -join ';'), 'User')
            }
            if (($env:Path -split ';') -notcontains $InstallDirectory) { $env:Path = "$InstallDirectory;$env:Path" }
        }
        Write-Host "Installed Dial $Version. Run this in your project: dial --sandbox off"
    } finally { Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue }
}

$dialInstallerPreviousErrorPreference = $ErrorActionPreference
try {
    $ErrorActionPreference = 'Stop'
    # PowerShell 5.1 must use TLS 1.2 when an older profile selected TLS 1.0.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Install-Dial -Version $Version -InstallDirectory $InstallDirectory -AssetDirectory $AssetDirectory -NoPathUpdate:$NoPathUpdate
} finally { $ErrorActionPreference = $dialInstallerPreviousErrorPreference }
