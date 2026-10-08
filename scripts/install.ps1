# Native Windows installer. Private releases use the operator's GitHub CLI login.
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDirectory = "$env:LOCALAPPDATA\Dial\bin",
    [string]$AssetDirectory,
    [switch]$NoPathUpdate
)
$ErrorActionPreference = 'Stop'
$repo = 'KiidxAtlas/dial'
if (-not [Environment]::Is64BitOperatingSystem) { throw 'Dial requires 64-bit Windows.' }
if (-not $InstallDirectory) { throw 'LOCALAPPDATA is unavailable; supply -InstallDirectory.' }
if ($Version -eq 'latest') {
    if ($AssetDirectory) { throw 'Offline installs require an explicit release version.' }
    $Version = (& gh api "repos/$repo/releases/latest" --jq '.tag_name')
    if ($LASTEXITCODE -ne 0) { throw 'Sign in with gh auth login using an account with repository access.' }
}
if ($Version -notmatch '^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') { throw "Invalid release version: $Version" }
$package = "dial-$Version-x86_64-pc-windows-msvc"
$archive = "$package.zip"
$temp = Join-Path ([IO.Path]::GetTempPath()) ("dial-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    if ($AssetDirectory) {
        Copy-Item (Join-Path $AssetDirectory $archive) $temp
        Copy-Item (Join-Path $AssetDirectory "$archive.sha256") $temp
    } else {
        & gh release download $Version --repo $repo --pattern $archive --pattern "$archive.sha256" --dir $temp
        if ($LASTEXITCODE -ne 0) { throw 'Release download failed. Check your GitHub login and repository access.' }
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
    $reported = & $binary --version
    if ($LASTEXITCODE -ne 0 -or $reported.Trim() -ne "dial $($Version.Substring(1))") { throw 'The downloaded executable failed its version check.' }
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
    Write-Host "Installed Dial $Version to $InstallDirectory"
    Write-Host 'Open a new terminal, then run: dial --sandbox off'
    Write-Host 'Native command tools need Git for Windows. Install it with: winget install --id Git.Git -e'
} finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
