# Tests use the actual compiled executable and a local archive; no credentials/network needed.
$ErrorActionPreference = 'Stop'
$temp = Join-Path $env:RUNNER_TEMP ("dial-installer-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $temp | Out-Null
try {
    $version = 'v' + ((& target\debug\dial.exe --version) -replace '^dial ', '').Trim()
    $package = "dial-$version-x86_64-pc-windows-msvc"
    $payload = Join-Path $temp $package
    New-Item -ItemType Directory $payload | Out-Null
    Copy-Item target\debug\dial.exe $payload
    $archive = Join-Path $temp "$package.zip"
    Compress-Archive -Path $payload -DestinationPath $archive
    $checksum = "$((Get-FileHash $archive -Algorithm SHA256).Hash.ToLower())  $package.zip"
    Set-Content "$archive.sha256" $checksum -Encoding Ascii
    $destination = Join-Path $temp 'Installed space 雪'
    & "$PSScriptRoot\install.ps1" -Version $version -InstallDirectory $destination -AssetDirectory $temp -NoPathUpdate
    $before = (Get-FileHash (Join-Path $destination 'dial.exe')).Hash
    Add-Content $archive 'tampered'
    $refused = $false
    try { & "$PSScriptRoot\install.ps1" -Version $version -InstallDirectory $destination -AssetDirectory $temp -NoPathUpdate }
    catch { if ($_ -notmatch 'Checksum verification failed') { throw }; $refused = $true }
    if (-not $refused) { throw 'Corrupt archive was accepted.' }
    if ((Get-FileHash (Join-Path $destination 'dial.exe')).Hash -ne $before) { throw 'Failed update replaced the working executable.' }
    & (Join-Path $destination 'dial.exe') --help
    if ($LASTEXITCODE -ne 0) { throw 'Installed executable failed.' }
    Write-Host 'PowerShell installer: install, Unicode destination, corrupt update preservation passed.'
} finally { Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue }
