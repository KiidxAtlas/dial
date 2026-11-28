# npm snapshot of dial-web/public/install.ps1; CLI installer is bundled alongside it.
# Installs the signed dial CLI and Dial Desktop on Windows.
#
# Environment variables:
#   $env:DIAL_CLI_ONLY = '1'         Install only the dial CLI (skip Dial Desktop)
#   $env:DIAL_DESKTOP_ONLY = '1'     Install only Dial Desktop (skip the dial CLI)
#   $env:DIAL_INSTALL_DESKTOP = '0'  Skip Dial Desktop installation
#   $env:DIAL_INSTALL_DESKTOP = '1'  Force Dial Desktop installation
#   $env:DIAL_VERSION = 'v0.4.1'     Pin a specific dial CLI release tag
#   $env:DIAL_DESKTOP_VERSION = 'v0.4.0'  Pin a specific Dial Desktop release tag
$ErrorActionPreference = 'Stop'
try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
  # PowerShell 7+ manages TLS automatically and may omit ServicePointManager protocols.
}

$tagPattern = '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
$desktopOnly = ($env:DIAL_DESKTOP_ONLY -eq '1')
$cliOnly = (-not $desktopOnly) -and (($env:DIAL_CLI_ONLY -eq '1') -or ($env:DIAL_INSTALL_DESKTOP -eq '0'))
# Preserve the CLI-only behavior for Dial Desktop's custom install directory.
if (-not $desktopOnly -and -not $cliOnly -and ($env:DIAL_INSTALL_DESKTOP -ne '1') -and $env:DIAL_INSTALL_DIR -and ($env:DIAL_NO_MODIFY_PATH -eq '1')) {
  $cliOnly = $true
}

$cliVersion = if ($env:DIAL_VERSION -and ($env:DIAL_VERSION -ne 'latest')) {
  if ($env:DIAL_VERSION -notmatch $tagPattern) {
    throw "Invalid DIAL_VERSION tag: $($env:DIAL_VERSION)"
  }
  $env:DIAL_VERSION
} else {
  $null
}

if (-not $desktopOnly) {
  $cliScript = Join-Path $PSScriptRoot 'install-cli.ps1'
  Write-Host '==> Installing dial CLI...'
  if ($cliVersion) {
    & $cliScript -Version $cliVersion
  } else {
    & $cliScript
  }
}

if ($cliOnly) {
  return
}

$desktopVersion = if ($env:DIAL_DESKTOP_VERSION -and ($env:DIAL_DESKTOP_VERSION -ne 'latest')) {
  if ($env:DIAL_DESKTOP_VERSION -notmatch $tagPattern) {
    throw "Invalid DIAL_DESKTOP_VERSION tag: $($env:DIAL_DESKTOP_VERSION)"
  }
  $env:DIAL_DESKTOP_VERSION
} else {
  $null
}

$desktopBase = if ($desktopVersion) {
  "https://github.com/KiidxAtlas/dial-desktop/releases/download/$desktopVersion"
} else {
  'https://github.com/KiidxAtlas/dial-desktop/releases/latest/download'
}

$isArm64 = $false
try {
  $isArm64 = ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -eq 'Arm64')
} catch {
  $isArm64 = ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') -or ($env:PROCESSOR_ARCHITEW6432 -eq 'ARM64')
}

$archLabel = if ($isArm64) { 'arm64' } else { 'x64' }
Write-Host "==> Installing Dial Desktop for Windows ($archLabel)..."

$manifest = Invoke-RestMethod -UseBasicParsing -Uri "$desktopBase/latest.json" -Headers @{ 'User-Agent' = 'dial-installer' }
$platforms = $manifest.platforms
$keys = if ($isArm64) {
  # Prefer native ARM64 builds; fall back to x64 (runs under Windows ARM64 emulation) if not yet published.
  @('windows-aarch64-nsis', 'windows-arm64-nsis', 'windows-aarch64', 'windows-arm64', 'windows-x86_64-nsis', 'windows-x86_64')
} else {
  @('windows-x86_64-nsis', 'windows-x86_64')
}

$entry = $null
$matchedKey = $null
foreach ($key in $keys) {
  if ($platforms -and ($platforms.PSObject.Properties.Name -contains $key)) {
    $candidate = $platforms.$key
    if ($candidate -and $candidate.url) {
      $entry = $candidate
      $matchedKey = $key
      break
    }
  }
}

if (-not $entry) {
  Write-Host "    No Windows desktop installer published for $archLabel in this release; skipped desktop app."
  return
}

if ($isArm64 -and ($matchedKey -like 'windows-x86_64*')) {
  Write-Host '    Native Windows ARM64 build not published in this release; using x64 installer (runs via Windows ARM64 emulation).'
}

$assetUrl = $null
if (-not [Uri]::TryCreate([string]$entry.url, [UriKind]::Absolute, [ref]$assetUrl) -or
    $assetUrl.Scheme -ne 'https' -or $assetUrl.Host -ne 'github.com' -or
    -not $assetUrl.IsDefaultPort -or $assetUrl.UserInfo -or
    $assetUrl.AbsolutePath -notmatch '^/KiidxAtlas/dial-desktop/releases/download/[^/]+/[^/]+\.(exe|msi)$') {
  throw 'Dial Desktop installer URL must point to a GitHub release asset in KiidxAtlas/dial-desktop.'
}
if (-not $entry.signature) { throw 'Dial Desktop installer is missing its release signature.' }

$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("dial-desktop-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
try {
  $isMsi = $assetUrl.AbsolutePath -match '\.msi$'
  $setupPath = Join-Path $tmpDir $(if ($isMsi) { 'Dial-setup.msi' } else { 'Dial-setup.exe' })
  $signaturePath = Join-Path $tmpDir 'asset.sig'
  [IO.File]::WriteAllText($signaturePath, [string]$entry.signature)
  Invoke-WebRequest -UseBasicParsing -Uri $assetUrl -Headers @{ 'User-Agent' = 'dial-installer' } -OutFile $setupPath

  & $env:DIAL_NPM_RUNTIME (Join-Path $PSScriptRoot 'verify-desktop.mjs') $setupPath $signaturePath
  if ($LASTEXITCODE -ne 0) { throw 'Dial Desktop installer failed release signature verification.' }
  $sig = Get-AuthenticodeSignature -FilePath $setupPath
  if ($sig.Status -ne 'Valid' -or -not $sig.SignerCertificate) {
    throw "Dial Desktop installer failed Authenticode signature check ($($sig.Status))."
  }

  if ($isMsi) {
    $proc = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/i', $setupPath, '/qn', '/norestart') -Wait -PassThru
  } else {
    $proc = Start-Process -FilePath $setupPath -ArgumentList '/S' -Wait -PassThru
  }
  if ($proc.ExitCode -ne 0) {
    throw "Dial Desktop installer exited with code $($proc.ExitCode)."
  }
  Write-Host '    Installed Dial Desktop (open Dial from the Start menu).'
} finally {
  Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
