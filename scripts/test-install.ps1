[CmdletBinding()]
param(
    [string] $ArchivePath,
    [string] $InstallerPath = (Join-Path $PSScriptRoot 'install.ps1')
)

# Tests for scripts/install.ps1. Serves signed fixture releases from a loopback HTTP server
# (python -m http.server) and runs the installer against them. Requires openssl and python 3.

$ErrorActionPreference = 'Stop'
$script:Passed = 0
$script:Failed = 0
$script:IsWindowsHost = [Environment]::OSVersion.Platform -eq 'Win32NT'
$script:HostExe = (Get-Process -Id $PID).Path
$script:InstallerPath = [IO.Path]::GetFullPath($InstallerPath)
$script:WorkRoot = Join-Path ([IO.Path]::GetTempPath()) ('dial-install-test-' + [Guid]::NewGuid().ToString('N'))
$script:Serve = Join-Path $script:WorkRoot 'serve'
$script:Port = 0
$script:Server = $null

function Assert-True {
    param([bool] $Condition, [string] $Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Equal {
    param($Expected, $Actual, [string] $Message)
    if ($Expected -ne $Actual) { throw "$Message (expected [$Expected], got [$Actual])" }
}

function Assert-Contains {
    param([string] $Text, [string] $Needle)
    if (-not $Text.Contains($Needle)) { throw "output does not contain '$Needle'. Output: $Text" }
}

function Test-Case {
    param([string] $Name, [scriptblock] $Body)
    try {
        & $Body
        $script:Passed++
        Write-Host "PASS  $Name"
    } catch {
        $script:Failed++
        Write-Host "FAIL  ${Name}: $($_.Exception.Message)"
    }
}

function Invoke-OpenSsl {
    param([string[]] $OsslArgs)
    $ErrorActionPreference = 'Continue'
    $out = & $script:OpenSsl @OsslArgs 2>&1
    if ($LASTEXITCODE -ne 0) { throw "openssl $($OsslArgs[0]) failed: $out" }
}

function ConvertFrom-Hex {
    param([string] $Hex)
    $bytes = New-Object byte[] ($Hex.Length / 2)
    for ($i = 0; $i -lt $bytes.Length; $i++) {
        $bytes[$i] = [Convert]::ToByte($Hex.Substring(2 * $i, 2), 16)
    }
    return ,$bytes
}

function ConvertTo-LittleEndian32 {
    param([System.Numerics.BigInteger] $Value)
    $raw = $Value.ToByteArray()
    $out = New-Object byte[] 32
    for ($i = 0; $i -lt [Math]::Min($raw.Length, 32); $i++) { $out[$i] = $raw[$i] }
    return ,$out
}

function Get-BaseEnvironment {
    param([string] $BaseUrl)
    return @{
        OS                           = 'Windows_NT'
        PROCESSOR_ARCHITECTURE       = 'AMD64'
        PROCESSOR_ARCHITEW6432       = $null
        DIAL_RELEASE_BASE_URL        = $BaseUrl
        DIAL_RELEASE_PUBLIC_KEY_PEM  = $null
        DIAL_VERSION                 = $null
        DIAL_INSTALL_DIR             = $null
    }
}

function Get-ScenarioUrl {
    param([string] $Name)
    return "http://127.0.0.1:$($script:Port)/$Name"
}

function ConvertTo-PsLiteral {
    param($Value)
    if ($null -eq $Value) { return '$null' }
    return "'" + ([string]$Value).Replace("'", "''") + "'"
}

function Invoke-Installer {
    param([string[]] $Arguments, [hashtable] $Environment, [string] $ScriptPath = $script:InstallerPath)
    # The child applies the scenario environment to itself: PowerShell 7 on Windows does not reliably
    # pass variables set through [Environment]::SetEnvironmentVariable to a child pwsh (PROCESSOR_ARCHITECTURE
    # arrived empty on GitHub runners), so inheritance would test the runner instead of the scenario.
    $lines = @(foreach ($name in $Environment.Keys) {
        "[Environment]::SetEnvironmentVariable($(ConvertTo-PsLiteral $name), $(ConvertTo-PsLiteral $Environment[$name]))"
    })
    $argText = ($Arguments | ForEach-Object { if ($_ -match '^-[A-Za-z]+$') { $_ } else { ConvertTo-PsLiteral $_ } }) -join ' '
    $lines += "`$ErrorActionPreference = 'Stop'"
    $lines += "& $(ConvertTo-PsLiteral $ScriptPath) $argText"
    $lines += 'exit 0'
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes(($lines -join "`n")))
    $ErrorActionPreference = 'Continue'
    $output = & $script:HostExe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded 2>&1 | Out-String
    return [pscustomobject]@{ Code = $LASTEXITCODE; Output = $output }
}

function Invoke-InProcess {
    param([hashtable] $Environment, [scriptblock] $Body)
    $saved = @{}
    foreach ($name in $Environment.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name)
        [Environment]::SetEnvironmentVariable($name, $Environment[$name])
    }
    try {
        & $Body
    } finally {
        foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
    }
}

function New-SigningKey {
    param([string] $Name)
    $dir = Join-Path $script:WorkRoot $Name
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $private = Join-Path $dir 'release-private.pem'
    $public = Join-Path $dir 'release-public.pem'
    Invoke-OpenSsl @('genpkey', '-algorithm', 'ED25519', '-out', $private)
    Invoke-OpenSsl @('pkey', '-in', $private, '-pubout', '-out', $public)
    return [pscustomobject]@{
        Private   = $private
        Public    = $public
        PublicPem = ([IO.File]::ReadAllText($public)).Trim() + "`n"
    }
}

function Set-ManifestSignature {
    param([string] $KeyPath, [string] $ManifestPath, [string] $SigPath)
    $raw = Join-Path $script:WorkRoot ([Guid]::NewGuid().ToString('N') + '.bin')
    Invoke-OpenSsl @('pkeyutl', '-sign', '-inkey', $KeyPath, '-rawin', '-in', $ManifestPath, '-out', $raw)
    [IO.File]::WriteAllText($SigPath, [Convert]::ToBase64String([IO.File]::ReadAllBytes($raw)) + "`n")
}

function New-FixtureBinary {
    param([string] $Path, [string] $Version)
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    if ($script:IsWindowsHost) {
        $source = [IO.Path]::ChangeExtension($Path, '.cs')
        [IO.File]::WriteAllText($source, 'public static class Program { public static void Main() { System.Console.WriteLine("dial ' + $Version + '"); } }')
        $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
        & $csc /nologo "/out:$Path" $source | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'csc failed to build the fixture binary' }
    } else {
        [IO.File]::WriteAllText($Path, "#!/bin/sh`necho 'dial $Version'`n")
        & chmod +x $Path
    }
}

function New-ZipFromFile {
    param([string] $ZipPath, [string] $EntryName, [string] $SourceFile)
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    $zip = [IO.Compression.ZipFile]::Open($ZipPath, [IO.Compression.ZipArchiveMode]::Create)
    try {
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $SourceFile, $EntryName)
    } finally {
        $zip.Dispose()
    }
}

# Builds <serve>/<Scenario>/ with the archive, SHA256SUMS, and SHA256SUMS.sig for $Tag.
function New-Release {
    param([string] $Scenario, [string] $Tag, [string] $ReportedVersion, [string] $KeyPath, [string] $ArchiveName, [string] $Target = 'x86_64-pc-windows-msvc')
    $dir = Join-Path $script:Serve $Scenario
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $pkg = "dial-$Tag-$Target"
    if (-not $ArchiveName) { $ArchiveName = "$pkg.zip" }
    $stage = Join-Path $script:WorkRoot ('stage-' + [Guid]::NewGuid().ToString('N'))
    $binary = Join-Path $stage (Join-Path $pkg 'dial.exe')
    New-FixtureBinary -Path $binary -Version $ReportedVersion
    $archive = Join-Path $dir $ArchiveName
    New-ZipFromFile -ZipPath $archive -EntryName "$pkg/dial.exe" -SourceFile $binary
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    $sums = Join-Path $dir 'SHA256SUMS'
    [IO.File]::WriteAllText($sums, "$hash  $ArchiveName`n")
    Set-ManifestSignature -KeyPath $KeyPath -ManifestPath $sums -SigPath (Join-Path $dir 'SHA256SUMS.sig')
    return $dir
}

function Start-ReleaseServer {
    $listener = New-Object System.Net.Sockets.TcpListener -ArgumentList ([Net.IPAddress]::Loopback), 0
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $script:Port = $port

    $candidates = @(@('python3'), @('python'), @('py', '-3'))
    foreach ($candidate in $candidates) {
        $command = $candidate[0]
        $prefix = @($candidate | Select-Object -Skip 1)
        try {
            $ErrorActionPreference = 'Continue'
            $probe = & $command @prefix '--version' 2>&1 | Out-String
            $ok = ($LASTEXITCODE -eq 0) -and ($probe -match 'Python 3')
            $ErrorActionPreference = 'Stop'
        } catch {
            $ErrorActionPreference = 'Stop'
            $ok = $false
        }
        if (-not $ok) { continue }
        $argString = (($prefix) -join ' ') + " -m http.server $port --bind 127.0.0.1 --directory `"$($script:Serve)`""
        $script:Server = Start-Process -FilePath $command -ArgumentList $argString -PassThru -NoNewWindow `
            -RedirectStandardOutput (Join-Path $script:WorkRoot 'server.out') `
            -RedirectStandardError (Join-Path $script:WorkRoot 'server.err')
        for ($i = 0; $i -lt 50; $i++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$port/" -TimeoutSec 2 | Out-Null
                return
            } catch {
                Start-Sleep -Milliseconds 200
            }
        }
        throw 'the local release server did not start'
    }
    throw 'python 3 is required to serve the test releases (tried python3, python, py -3)'
}

function Import-Ed25519 {
    $text = [IO.File]::ReadAllText($script:InstallerPath)
    $match = [regex]::Match($text, "(?ms)^    \`$ed25519Source = @'\r?\n(.*?)\r?\n'@")
    Assert-True $match.Success 'install.ps1 has no ed25519Source block'
    if (-not ('DialInstallerEd25519V1' -as [type])) {
        Add-Type -TypeDefinition $match.Groups[1].Value -ReferencedAssemblies ([System.Numerics.BigInteger].Assembly.Location), ([System.Security.Cryptography.SHA512].Assembly.Location)
    }
}

function Test-Ed25519 {
    param([byte[]] $PublicKey, [byte[]] $Message, [byte[]] $Signature)
    return [DialInstallerEd25519V1]::Verify($PublicKey, $Message, $Signature)
}

$script:OpenSsl = $null
$opensslCommand = @(Get-Command openssl -CommandType Application -ErrorAction SilentlyContinue) | Select-Object -First 1
if ($opensslCommand) {
    $script:OpenSsl = $opensslCommand.Source
} elseif ($env:ProgramFiles) {
    $gitOpenSsl = Join-Path $env:ProgramFiles 'Git\usr\bin\openssl.exe'
    if (Test-Path -LiteralPath $gitOpenSsl) { $script:OpenSsl = $gitOpenSsl }
}

function Get-ArchiveIdentity {
    param([string] $Path)
    $archiveLeaf = Split-Path -Leaf $Path
    $nameMatch = [regex]::Match($archiveLeaf, '^dial-(.+)-(x86_64|aarch64)-pc-windows-msvc\.zip$')
    Assert-True $nameMatch.Success "ArchivePath must be named dial-<tag>-<x86_64|aarch64>-pc-windows-msvc.zip (got $archiveLeaf)"
    return [pscustomobject]@{
        Tag = $nameMatch.Groups[1].Value
        Architecture = if ($nameMatch.Groups[2].Value -eq 'aarch64') { 'ARM64' } else { 'AMD64' }
    }
}

$archiveIdentity = if ($ArchivePath) { Get-ArchiveIdentity $ArchivePath } else { $null }
$tagFromArchive = if ($archiveIdentity) { $archiveIdentity.Tag } else { $null }

try {
    New-Item -ItemType Directory -Force -Path $script:Serve | Out-Null
    Test-Case 'recognizes both published Windows archive targets and rejects unknown targets' {
        foreach ($case in @(@('x86_64', 'AMD64'), @('aarch64', 'ARM64'))) {
            $identity = Get-ArchiveIdentity "dial-v0.2.2-$($case[0])-pc-windows-msvc.zip"
            Assert-Equal 'v0.2.2' $identity.Tag 'archive tag'
            Assert-Equal $case[1] $identity.Architecture 'archive architecture'
        }
        $rejected = $false
        try { Get-ArchiveIdentity 'dial-v0.2.2-i686-pc-windows-msvc.zip' } catch { $rejected = $true }
        Assert-True $rejected 'unsupported archive target was accepted'
    }

    Assert-True ([bool] $script:OpenSsl) 'openssl is required to run the installer tests'

    $keyA = New-SigningKey -Name 'key-a'
    $keyB = New-SigningKey -Name 'key-b'
    Start-ReleaseServer

    New-Release -Scenario 'ok' -Tag 'v0.1.0' -ReportedVersion '0.1.0' -KeyPath $keyA.Private | Out-Null
    New-Release -Scenario 'wrong-key' -Tag 'v0.1.0' -ReportedVersion '0.1.0' -KeyPath $keyB.Private | Out-Null
    New-Release -Scenario 'tampered' -Tag 'v0.1.0' -ReportedVersion '0.1.0' -KeyPath $keyA.Private | Out-Null
    New-Release -Scenario 'mismatch' -Tag 'v9.9.9' -ReportedVersion '0.1.0' -KeyPath $keyA.Private | Out-Null
    $manifestDir = New-Release -Scenario 'manifest-edit' -Tag 'v0.1.0' -ReportedVersion '0.1.0' -KeyPath $keyA.Private
    New-Release -Scenario 'arm' -Tag 'v0.1.0' -ReportedVersion '0.1.0' -KeyPath $keyA.Private -Target 'aarch64-pc-windows-msvc' | Out-Null

    # Tamper the archive after the manifest was signed.
    $tamperedZip = Join-Path (Join-Path $script:Serve 'tampered') 'dial-v0.1.0-x86_64-pc-windows-msvc.zip'
    $tamperedBytes = [IO.File]::ReadAllBytes($tamperedZip)
    [IO.File]::WriteAllBytes($tamperedZip, ([byte[]] ($tamperedBytes + [byte] 0x41)))

    # Edit the manifest after signing.
    $sumsPath = Join-Path $manifestDir 'SHA256SUMS'
    $sumsText = [IO.File]::ReadAllText($sumsPath)
    $newFirst = 'a'
    if ($sumsText.Substring(0, 1) -eq 'a') { $newFirst = 'b' }
    [IO.File]::WriteAllText($sumsPath, $newFirst + $sumsText.Substring(1))

    if ($tagFromArchive) {
        $realDir = Join-Path $script:Serve 'real'
        New-Item -ItemType Directory -Force -Path $realDir | Out-Null
        Copy-Item -LiteralPath $ArchivePath -Destination (Join-Path $realDir (Split-Path -Leaf $ArchivePath)) -Force
        $realHash = (Get-FileHash -LiteralPath (Join-Path $realDir (Split-Path -Leaf $ArchivePath)) -Algorithm SHA256).Hash.ToLowerInvariant()
        $realSums = Join-Path $realDir 'SHA256SUMS'
        [IO.File]::WriteAllText($realSums, "$realHash  $(Split-Path -Leaf $ArchivePath)`n")
        Set-ManifestSignature -KeyPath $keyA.Private -ManifestPath $realSums -SigPath (Join-Path $realDir 'SHA256SUMS.sig')
    }

    Test-Case 'Ed25519 RFC 8032 test 1 (empty message) verifies' {
        Import-Ed25519
        $ok = Test-Ed25519 (ConvertFrom-Hex 'd75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a') (ConvertFrom-Hex '') (ConvertFrom-Hex 'e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b')
        Assert-True $ok 'RFC 8032 test 1 rejected'
    }

    Test-Case 'Ed25519 RFC 8032 test 2 verifies and rejects a changed message' {
        $pk = ConvertFrom-Hex '3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c'
        $sig = ConvertFrom-Hex '92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00'
        Assert-True (Test-Ed25519 $pk (ConvertFrom-Hex '72') $sig) 'RFC 8032 test 2 rejected'
        Assert-True (-not (Test-Ed25519 $pk (ConvertFrom-Hex '73') $sig)) 'changed message accepted'
    }

    Test-Case 'Ed25519 RFC 8032 test 3 verifies and rejects a changed signature' {
        $pk = ConvertFrom-Hex 'fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025'
        $sig = ConvertFrom-Hex '6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a'
        Assert-True (Test-Ed25519 $pk (ConvertFrom-Hex 'af82') $sig) 'RFC 8032 test 3 rejected'
        $bad = [byte[]] $sig.Clone()
        $bad[63] = [byte] ($bad[63] -bxor 1)
        Assert-True (-not (Test-Ed25519 $pk (ConvertFrom-Hex 'af82') $bad)) 'changed signature accepted'
    }

    Test-Case 'Ed25519 rejects non-canonical S (S + L)' {
        $pk = ConvertFrom-Hex '3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c'
        $sig = ConvertFrom-Hex '92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00'
        $order = [System.Numerics.BigInteger]::Pow(2, 252) + [System.Numerics.BigInteger]::Parse('27742317777372353535851937790883648493')
        $s = New-Object System.Numerics.BigInteger (,([byte[]] ($sig[32..63] + [byte] 0)))
        $sPlusL = ConvertTo-LittleEndian32 ($s + $order)
        $forged = [byte[]] ($sig[0..31] + $sPlusL)
        Assert-True (-not (Test-Ed25519 $pk (ConvertFrom-Hex '72') $forged)) 'non-canonical S accepted'
    }

    Test-Case 'Ed25519 rejects invalid public keys (y = p, and x = 0 with sign bit set)' {
        $sig = ConvertFrom-Hex '92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00'
        $p = ConvertTo-LittleEndian32 ([System.Numerics.BigInteger]::Pow(2, 255) - 19)
        Assert-True (-not (Test-Ed25519 $p (ConvertFrom-Hex '72') $sig)) 'y = p accepted'
        $signBitOne = New-Object byte[] 32
        $signBitOne[0] = 1
        $signBitOne[31] = 0x80
        Assert-True (-not (Test-Ed25519 $signBitOne (ConvertFrom-Hex '72') $sig)) 'x = 0 with sign bit accepted'
    }

    Test-Case 'Ed25519 verifies an OpenSSL signature over random data' {
        $msgPath = Join-Path $script:WorkRoot 'random.bin'
        $sigPath = Join-Path $script:WorkRoot 'random.sig'
        $derPath = Join-Path $script:WorkRoot 'random.der'
        $random = New-Object byte[] 96
        [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($random)
        [IO.File]::WriteAllBytes($msgPath, $random)
        Invoke-OpenSsl @('pkeyutl', '-sign', '-inkey', $keyA.Private, '-rawin', '-in', $msgPath, '-out', $sigPath)
        Invoke-OpenSsl @('pkey', '-pubin', '-in', $keyA.Public, '-outform', 'DER', '-out', $derPath)
        $der = [IO.File]::ReadAllBytes($derPath)
        $raw = [byte[]] ($der[12..43])
        $sig = [IO.File]::ReadAllBytes($sigPath)
        Assert-True (Test-Ed25519 $raw $random $sig) 'OpenSSL signature rejected'
        $changed = [byte[]] $random.Clone()
        $changed[0] = [byte] ($changed[0] -bxor 0xff)
        Assert-True (-not (Test-Ed25519 $raw $changed $sig)) 'OpenSSL signature accepted for changed data'
    }

    $dirOk = Get-ScenarioUrl 'ok'
    $pathBin = Join-Path $script:WorkRoot 'bin-ok'

    Test-Case 'installs a signed release and prints next steps without a manual PATH step' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', $pathBin) $envMap
        Assert-Equal 0 $run.Code "installer exit code. Output: $($run.Output)"
        Assert-True (Test-Path -LiteralPath (Join-Path $pathBin 'dial.exe')) 'dial.exe was not installed'
        Assert-Contains $run.Output 'Installed Dial 0.1.0'
        Assert-Contains $run.Output 'dial config set sandbox.mode off'
        Assert-True (-not ($run.Output -match 'Add .* to PATH')) 'output still tells the user to add to PATH'
    }

    Test-Case 'reinstall over an existing version keeps dial.exe.bak' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', $pathBin) $envMap
        Assert-Equal 0 $run.Code "reinstall exit code. Output: $($run.Output)"
        Assert-True (Test-Path -LiteralPath (Join-Path $pathBin 'dial.exe.bak')) 'backup was not kept'
    }

    Test-Case 'installs the native aarch64 Windows build on ARM64 Windows' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'arm')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $envMap['PROCESSOR_ARCHITECTURE'] = 'ARM64'
        $armBin = Join-Path $script:WorkRoot 'bin-arm'
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', $armBin) $envMap
        Assert-Equal 0 $run.Code "ARM64 exit code. Output: $($run.Output)"
        Assert-Contains $run.Output 'Installed Dial 0.1.0'
    }

    Test-Case 'refuses unsupported architectures' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $envMap['PROCESSOR_ARCHITECTURE'] = 'x86'
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', (Join-Path $script:WorkRoot 'bin-x86')) $envMap
        Assert-True ($run.Code -ne 0) 'x86 was accepted'
        Assert-Contains $run.Output 'Unsupported Windows architecture'
    }

    Test-Case 'refuses a release signed by a different key' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'wrong-key')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $dir = Join-Path $script:WorkRoot 'bin-wrong'
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', $dir) $envMap
        Assert-True ($run.Code -ne 0) 'wrong-key release was installed'
        Assert-Contains $run.Output 'signature verification failed'
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $dir 'dial.exe'))) 'dial.exe written despite signature failure'
    }

    Test-Case 'refuses a manifest edited after signing' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'manifest-edit')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', (Join-Path $script:WorkRoot 'bin-edit')) $envMap
        Assert-True ($run.Code -ne 0) 'edited manifest was accepted'
        Assert-Contains $run.Output 'signature verification failed'
    }

    Test-Case 'refuses an archive changed after the manifest was signed' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'tampered')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $dir = Join-Path $script:WorkRoot 'bin-tampered'
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', $dir) $envMap
        Assert-True ($run.Code -ne 0) 'tampered archive was installed'
        Assert-Contains $run.Output 'digest mismatch'
        Assert-True (-not (Test-Path -LiteralPath (Join-Path $dir 'dial.exe'))) 'dial.exe written despite digest mismatch'
    }

    Test-Case 'refuses a binary whose version does not match the tag' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'mismatch')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $run = Invoke-Installer @('-Version', 'v9.9.9', '-InstallDir', (Join-Path $script:WorkRoot 'bin-mismatch')) $envMap
        Assert-True ($run.Code -ne 0) 'mismatched binary was installed'
        Assert-Contains $run.Output 'version check failed'
    }

    Test-Case 'reports a missing release as not found' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $run = Invoke-Installer @('-Version', 'v8.8.8', '-InstallDir', (Join-Path $script:WorkRoot 'bin-missing')) $envMap
        Assert-True ($run.Code -ne 0) 'missing release reported success'
        Assert-Contains $run.Output 'Release asset not found'
    }

    Test-Case 'refuses to run without an embedded or provided key, naming the variable' {
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', (Join-Path $script:WorkRoot 'bin-nokey')) $envMap
        Assert-True ($run.Code -ne 0) 'installed without any key'
        Assert-Contains $run.Output 'DIAL_RELEASE_PUBLIC_KEY_PEM'
    }

    Test-Case 'runs the embedded-key copy with no environment variable set' {
        $src = [IO.File]::ReadAllText($script:InstallerPath)
        $pattern = '(?m)^    \$DialReleasePublicKeyPem = [\x27]{2}\r?$'
        Assert-Equal 1 ([regex]::Matches($src, $pattern)).Count 'placeholder line count'
        $match = [regex]::Match($src, $pattern)
        $replacement = "    `$DialReleasePublicKeyPem = @'`n" + $keyA.PublicPem + "'@"
        $embedded = $src.Substring(0, $match.Index) + $replacement + $src.Substring($match.Index + $match.Length)
        $embeddedPath = Join-Path $script:WorkRoot 'install-embedded.ps1'
        [IO.File]::WriteAllText($embeddedPath, $embedded)
        $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'ok')
        $run = Invoke-Installer @('-Version', 'v0.1.0', '-InstallDir', (Join-Path $script:WorkRoot 'bin-embedded')) $envMap $embeddedPath
        Assert-Equal 0 $run.Code "embedded-key run exit code. Output: $($run.Output)"
        Assert-Contains $run.Output 'Installed Dial 0.1.0'
    }

    Test-Case 'scriptblock form installs a pinned version and leaves no function or preference behind' {
        $envMap = Get-BaseEnvironment $dirOk
        $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
        $dir = Join-Path $script:WorkRoot 'bin-sb'
        $before = $ErrorActionPreference
        $sb = [scriptblock]::Create([IO.File]::ReadAllText($script:InstallerPath))
        Invoke-InProcess -Environment $envMap -Body { & $sb -Version 'v0.1.0' -InstallDir $dir }
        Assert-Equal $before $ErrorActionPreference 'ErrorActionPreference leaked'
        Assert-True (-not (Test-Path -LiteralPath Function:\Install-DialRelease)) 'installer function was not removed'
        Assert-True (Test-Path -LiteralPath (Join-Path $dir 'dial.exe')) 'dial.exe was not installed'
    }

    Test-Case 'Invoke-Expression form uses DIAL_VERSION and updates the session PATH' {
        $origSessionPath = $env:Path
        $origUserPath = $null
        if ($script:IsWindowsHost) { $origUserPath = [Environment]::GetEnvironmentVariable('Path', 'User') }
        $dir = Join-Path $script:WorkRoot 'bin-iex'
        try {
            $envMap = Get-BaseEnvironment $dirOk
            $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
            $envMap['DIAL_VERSION'] = 'v0.1.0'
            $envMap['DIAL_INSTALL_DIR'] = $dir
            $before = $ErrorActionPreference
            $text = [IO.File]::ReadAllText($script:InstallerPath)
            Invoke-InProcess -Environment $envMap -Body { Invoke-Expression $text }
            Assert-Equal $before $ErrorActionPreference 'ErrorActionPreference leaked'
            Assert-True (-not (Test-Path -LiteralPath Function:\Install-DialRelease)) 'installer function was not removed'
            Assert-True (Test-Path -LiteralPath (Join-Path $dir 'dial.exe')) 'dial.exe was not installed'
            $sessionEntries = @($env:Path -split ';' | Where-Object { $_ })
            Assert-True (@($sessionEntries | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }).Count -gt 0) 'session PATH does not contain the install directory'
            if ($script:IsWindowsHost) {
                $userEntries = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ })
                Assert-True (@($userEntries | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }).Count -gt 0) 'user PATH does not contain the install directory'
            }
        } finally {
            $env:Path = $origSessionPath
            if ($script:IsWindowsHost) { [Environment]::SetEnvironmentVariable('Path', $origUserPath, 'User') }
        }
    }

    if ($tagFromArchive) {
        Test-Case "installs the real archive $(Split-Path -Leaf $ArchivePath) end to end" {
            $envMap = Get-BaseEnvironment (Get-ScenarioUrl 'real')
            $envMap['PROCESSOR_ARCHITECTURE'] = $archiveIdentity.Architecture
            $envMap['DIAL_RELEASE_PUBLIC_KEY_PEM'] = $keyA.PublicPem
            $dir = Join-Path $script:WorkRoot 'bin-real'
            $run = Invoke-Installer @('-Version', $tagFromArchive, '-InstallDir', $dir) $envMap
            Assert-Equal 0 $run.Code "real archive exit code. Output: $($run.Output)"
            $installed = & (Join-Path $dir 'dial.exe') --version 2>&1 | Out-String
            Assert-True ($installed -match "^dial $([regex]::Escape($tagFromArchive.TrimStart('v')))( |\r|\n|$)") "installed binary reports: $installed"
        }
    }
} catch {
    $script:Failed++
    Write-Host "FAIL  harness: $($_.Exception.Message)"
} finally {
    if ($script:Server) { Stop-Process -Id $script:Server.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $script:WorkRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "Passed: $script:Passed  Failed: $script:Failed"
if ($script:Failed -gt 0) { exit 1 }
exit 0
