# Standalone installer tests; no private repository, credentials, winget or third-party test modules.
$ErrorActionPreference = 'Stop'
$tokens = $null; $parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'install.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
foreach ($statement in $ast.EndBlock.Statements) {
    if ($statement -is [System.Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($statement.Extent.Text))
    }
}
function Assert-DialTest($Condition, $Message) { if (-not $Condition) { throw $Message } }
function Assert-DialRefused([scriptblock]$Action, [string]$Message) {
    $refused = $false
    try { & $Action } catch { if ($_.Exception.Message -notlike "*$Message*") { throw }; $refused = $true }
    Assert-DialTest $refused "Expected refusal: $Message"
}
$root = Join-Path ([IO.Path]::GetTempPath()) ('dial-bootstrap-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $root | Out-Null
$previousBash = $env:DIAL_BASH
$previousUserBash = [Environment]::GetEnvironmentVariable('DIAL_BASH', 'User')
$previousGitRoot = $env:GIT_INSTALL_ROOT
$previousPath = $env:Path
try {
    # Exercise the actual `irm | iex` entry point without installing anything.
    $source = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'install.ps1'))
    $guardedSource = $source.Replace('[string]$Version = ''latest''', '[string]$Version = ''invalid''')
    $previousPreference = $ErrorActionPreference
    Assert-DialRefused { Invoke-Expression $guardedSource } 'Invalid release version'
    Assert-DialTest ($ErrorActionPreference -eq $previousPreference) 'Piped install did not restore the caller error preference'
    # Missing gh installs through winget and becomes discoverable in the same session.
    $script:ghQueries = 0; $script:pathRefreshes = 0; $script:wingetArguments = ''
    function Get-Command {
        param($Name, $ErrorAction)
        if ($Name -eq 'gh') { $script:ghQueries++; if ($script:ghQueries -gt 1) { return @{ Source = 'Test-DialGh' } }; return $null }
        if ($Name -eq 'winget') { return @{ Source = 'Test-DialWinget' } }
        return $null
    }
    function Test-DialWinget { $script:wingetArguments = $args -join ' '; $global:LASTEXITCODE = 0 }
    function Update-DialProcessPath { $script:pathRefreshes++ }
    Assert-DialTest ((Get-DialGitHubCli) -eq 'Test-DialGh') 'GitHub CLI did not become available immediately'
    Assert-DialTest ($script:wingetArguments -match '--id GitHub.cli --exact') 'Wrong prerequisite installed'
    Assert-DialTest ($script:pathRefreshes -eq 2) 'The process PATH was not refreshed'
    Remove-Item Function:Get-Command
    $getBash = (Get-Item Function:Get-DialGitBash).ScriptBlock
    $script:bashQueries = 0
    function Get-DialGitBash { $script:bashQueries++; if ($script:bashQueries -gt 1) { return 'C:\Fixture Git\bin\bash.exe' }; return $null }
    function Get-Command { param($Name, $ErrorAction); if ($Name -eq 'winget') { return @{ Source = 'Test-DialWinget' } }; return $null }
    Initialize-DialGitBash
    Assert-DialTest ($script:wingetArguments -match '--id Git.Git --exact.*--scope user') 'Git Bash was not installed for the current user'
    Assert-DialTest ($env:DIAL_BASH -eq 'C:\Fixture Git\bin\bash.exe') 'Git Bash was not made available immediately'
    Remove-Item Function:Get-Command
    Set-Item Function:Get-DialGitBash $getBash
    $gitRoot = Join-Path $root 'custom Git'
    New-Item -ItemType Directory (Join-Path $gitRoot 'bin') | Out-Null
    Set-Content (Join-Path $gitRoot 'bin/bash.exe') 'fixture'
    $env:DIAL_BASH = $null; $env:GIT_INSTALL_ROOT = $gitRoot
    Assert-DialTest ((Get-DialGitBash) -eq (Join-Path $gitRoot 'bin/bash.exe')) 'Custom Git discovery failed'

    # First run signs in, then checks repository access and platform assets.
    $script:signedIn = $false; $script:denied = $false; $script:missingRelease = $false
    $script:loginCount = 0
    $script:release = @{ tag_name = 'v0.1.0'; assets = @(@{ name = 'dial-v0.1.0-x86_64-pc-windows-msvc.zip' }, @{ name = 'dial-v0.1.0-x86_64-pc-windows-msvc.zip.sha256' }) }
    function Test-DialGh {
        $global:LASTEXITCODE = 0
        switch ($args[0] + ' ' + $args[1]) {
            'auth status' { if (-not $script:signedIn) { $global:LASTEXITCODE = 1 }; return }
            'auth login' { $script:loginCount++; $script:signedIn = $true; return }
            'api repos/KiidxAtlas/dial' { if ($script:denied) { $global:LASTEXITCODE = 1 }; return }
            default { if ($script:missingRelease) { $global:LASTEXITCODE = 1; return }; return ($script:release | ConvertTo-Json -Depth 5) }
        }
    }
    $release = Get-DialRelease -Gh Test-DialGh -Version latest
    Assert-DialTest ($release.tag_name -eq 'v0.1.0' -and $script:loginCount -eq 1) 'Interactive GitHub sign-in was not handled'
    $script:denied = $true
    Assert-DialRefused { Get-DialRelease -Gh Test-DialGh -Version latest } 'cannot access'
    $script:denied = $false; $script:missingRelease = $true
    Assert-DialRefused { Get-DialRelease -Gh Test-DialGh -Version latest } 'has not been published'
    $script:missingRelease = $false
    function Get-DialGitHubCli { return 'Test-DialGh' }
    $script:gitInitialized = 0
    function Initialize-DialGitBash { $script:gitInitialized++ }
    $script:release.assets = @()
    $destination = Join-Path $root ('Installed space ' + [char]0x96EA)
    Assert-DialRefused { Install-Dial -Version latest -InstallDirectory $destination -NoPathUpdate } 'native Windows download is not published'
    Assert-DialTest ($script:gitInitialized -eq 0 -and -not (Test-Path $destination)) 'Missing release installed unnecessary prerequisites or touched Dial'

    # Real ZIP/hash/filesystem contracts; Windows PowerShell also runs a compiled fixture exe.
    $assets = Join-Path $root 'assets'; New-Item -ItemType Directory $assets | Out-Null
    $payload = Join-Path $root 'dial.exe'
    $binaryVersion = (Get-Item Function:Get-DialBinaryVersion).ScriptBlock
    if ($env:OS -eq 'Windows_NT' -and $PSVersionTable.PSEdition -eq 'Desktop') {
        Add-Type -TypeDefinition 'public class DialFixture { public static void Main(string[] args) { System.Console.WriteLine("dial 0.1.0"); } }' -OutputAssembly $payload -OutputType ConsoleApplication
    } else {
        Set-Content $payload 'portable fixture'
        function Get-DialBinaryVersion { param($Binary); return 'dial 0.1.0' }
    }
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archiveName = 'dial-v0.1.0-x86_64-pc-windows-msvc.zip'
    $archive = Join-Path $assets $archiveName
    $zip = [IO.Compression.ZipFile]::Open($archive, [IO.Compression.ZipArchiveMode]::Create)
    try { [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $payload, 'dial-v0.1.0-x86_64-pc-windows-msvc/dial.exe') | Out-Null } finally { $zip.Dispose() }
    Set-Content "$archive.sha256" "$((Get-FileHash $archive -Algorithm SHA256).Hash)  $archiveName" -Encoding Ascii
    Install-Dial -Version v0.1.0 -InstallDirectory $destination -AssetDirectory $assets -NoPathUpdate
    $before = (Get-FileHash (Join-Path $destination 'dial.exe')).Hash
    if ($env:OS -eq 'Windows_NT' -and $PSVersionTable.PSEdition -eq 'Desktop') {
        $fullDestination = Join-Path $root 'full script install'
        & ([scriptblock]::Create($source)) -Version v0.1.0 -InstallDirectory $fullDestination -AssetDirectory $assets -NoPathUpdate
        Assert-DialTest ((Get-FileHash (Join-Path $fullDestination 'dial.exe')).Hash -eq $before) 'Full script invocation installed the wrong executable'
    }
    function Get-DialBinaryVersion { param($Binary); return 'dial 9.9.9' }
    Assert-DialRefused { Install-Dial -Version v0.1.0 -InstallDirectory $destination -AssetDirectory $assets -NoPathUpdate } 'version check'
    Assert-DialTest ((Get-FileHash (Join-Path $destination 'dial.exe')).Hash -eq $before) 'Wrong-version update replaced Dial'
    Add-Content $archive 'tampered'
    Assert-DialRefused { Install-Dial -Version v0.1.0 -InstallDirectory $destination -AssetDirectory $assets -NoPathUpdate } 'Checksum verification failed'
    Assert-DialTest ((Get-FileHash (Join-Path $destination 'dial.exe')).Hash -eq $before) 'Corrupt update replaced Dial'
    Assert-DialRefused { Install-Dial -Version '../unsafe' -InstallDirectory $destination -AssetDirectory $assets -NoPathUpdate } 'Invalid release version'
    Write-Host 'PowerShell bootstrap tests passed: prerequisite setup, login, access denial, missing release, private ZIP install, version/checksum refusal and update preservation.'
} finally {
    $env:DIAL_BASH = $previousBash
    [Environment]::SetEnvironmentVariable('DIAL_BASH', $previousUserBash, 'User')
    $env:GIT_INSTALL_ROOT = $previousGitRoot
    $env:Path = $previousPath
    Remove-Item $root -Recurse -Force -ErrorAction SilentlyContinue
}
