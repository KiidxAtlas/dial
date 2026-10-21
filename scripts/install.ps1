param(
    [string] $Version = 'latest',
    [string] $InstallDir
)

function Install-DialRelease {
    param(
        [string] $Version,
        [string] $InstallDir
    )

    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }

    $repository = 'KiidxAtlas/dial'
    $missingBuildMessage = 'No Windows build was found for this Dial release. Releases before v0.1.4 have no Windows archive; check https://github.com/KiidxAtlas/dial/releases or pass -Version with a release that lists dial-<tag>-x86_64-pc-windows-msvc.zip.'

    # Release builds replace the next line with `$DialReleasePublicKeyPem = @' ... '@` (the trusted Ed25519 public key, PEM).
    $DialReleasePublicKeyPem = ''

    # Ed25519 (RFC 8032) verifier, compiled once per session. Bump the type name when this source changes.
    $ed25519Source = @'
using System;
using System.Numerics;
using System.Security.Cryptography;

public static class DialInstallerEd25519V1
{
    private static readonly BigInteger P = BigInteger.Pow(2, 255) - 19;
    private static readonly BigInteger L = BigInteger.Pow(2, 252) + BigInteger.Parse("27742317777372353535851937790883648493");
    private static readonly BigInteger D = FieldMod(-121665 * Inverse(121666));
    private static readonly BigInteger SqrtM1 = BigInteger.ModPow(2, (P - 1) / 4, P);
    private static readonly Point Base = DecodePoint(BaseEncoding());

    private sealed class Point
    {
        public readonly BigInteger X;
        public readonly BigInteger Y;
        public readonly BigInteger Z;
        public readonly BigInteger T;

        public Point(BigInteger x, BigInteger y, BigInteger z, BigInteger t)
        {
            X = x;
            Y = y;
            Z = z;
            T = t;
        }
    }

    public static bool Verify(byte[] publicKey, byte[] message, byte[] signature)
    {
        if (publicKey == null || message == null || signature == null) return false;
        if (publicKey.Length != 32 || signature.Length != 64) return false;

        Point a = DecodePoint(publicKey);
        byte[] rBytes = new byte[32];
        byte[] sBytes = new byte[32];
        Array.Copy(signature, 0, rBytes, 0, 32);
        Array.Copy(signature, 32, sBytes, 0, 32);
        Point r = DecodePoint(rBytes);
        if (a == null || r == null) return false;

        BigInteger s = FromLittleEndian(sBytes);
        if (s >= L) return false;

        byte[] input = new byte[64 + message.Length];
        Array.Copy(rBytes, 0, input, 0, 32);
        Array.Copy(publicKey, 0, input, 32, 32);
        Array.Copy(message, 0, input, 64, message.Length);
        byte[] digest;
        using (SHA512 sha = SHA512.Create())
        {
            digest = sha.ComputeHash(input);
        }
        BigInteger h = BigInteger.Remainder(FromLittleEndian(digest), L);

        Point left = ScalarMultiply(s, Base);
        Point right = Add(r, ScalarMultiply(h, a));
        return SameProjective(MultiplyByEight(left), MultiplyByEight(right));
    }

    private static byte[] BaseEncoding()
    {
        byte[] encoded = new byte[32];
        for (int i = 0; i < 32; i++) encoded[i] = 0x66;
        encoded[0] = 0x58;
        return encoded;
    }

    private static BigInteger FieldMod(BigInteger value)
    {
        BigInteger result = BigInteger.Remainder(value, P);
        if (result.Sign < 0) result += P;
        return result;
    }

    private static BigInteger Inverse(BigInteger value)
    {
        return BigInteger.ModPow(FieldMod(value), P - 2, P);
    }

    private static BigInteger FromLittleEndian(byte[] bytes)
    {
        byte[] padded = new byte[bytes.Length + 1];
        Array.Copy(bytes, padded, bytes.Length);
        return new BigInteger(padded);
    }

    private static Point DecodePoint(byte[] encoded)
    {
        if (encoded.Length != 32) return null;
        int sign = (encoded[31] >> 7) & 1;
        byte[] yBytes = (byte[])encoded.Clone();
        yBytes[31] = (byte)(yBytes[31] & 0x7f);
        BigInteger y = FromLittleEndian(yBytes);
        if (y >= P) return null;

        BigInteger y2 = FieldMod(y * y);
        BigInteger u = FieldMod(y2 - 1);
        BigInteger v = FieldMod(D * y2 + 1);
        BigInteger v3 = FieldMod(v * v * v);
        BigInteger v7 = FieldMod(v3 * v3 * v);
        BigInteger x = FieldMod(u * v3 * BigInteger.ModPow(FieldMod(u * v7), (P - 5) / 8, P));

        BigInteger check = FieldMod(v * x * x);
        if (check != FieldMod(u))
        {
            if (check != FieldMod(-u)) return null;
            x = FieldMod(x * SqrtM1);
        }
        if (x.IsZero && sign == 1) return null;
        int xSign = x.IsEven ? 0 : 1;
        if (xSign != sign) x = FieldMod(-x);
        return new Point(x, y, BigInteger.One, FieldMod(x * y));
    }

    // Extended twisted Edwards coordinates, unified addition (a = -1).
    private static Point Add(Point p, Point q)
    {
        BigInteger a = FieldMod((p.Y - p.X) * (q.Y - q.X));
        BigInteger b = FieldMod((p.Y + p.X) * (q.Y + q.X));
        BigInteger c = FieldMod(p.T * q.T * 2 * D);
        BigInteger dd = FieldMod(p.Z * q.Z * 2);
        BigInteger e = b - a;
        BigInteger f = dd - c;
        BigInteger g = dd + c;
        BigInteger h = b + a;
        return new Point(FieldMod(e * f), FieldMod(g * h), FieldMod(f * g), FieldMod(e * h));
    }

    private static Point ScalarMultiply(BigInteger scalar, Point point)
    {
        Point result = new Point(BigInteger.Zero, BigInteger.One, BigInteger.One, BigInteger.Zero);
        for (int bit = 255; bit >= 0; bit--)
        {
            result = Add(result, result);
            if (!((scalar >> bit) & BigInteger.One).IsZero) result = Add(result, point);
        }
        return result;
    }

    private static Point MultiplyByEight(Point p)
    {
        Point q = Add(p, p);
        q = Add(q, q);
        return Add(q, q);
    }

    private static bool SameProjective(Point p, Point q)
    {
        return FieldMod(p.X * q.Z - q.X * p.Z).IsZero && FieldMod(p.Y * q.Z - q.Y * p.Z).IsZero;
    }
}
'@

    # Local helpers. Native tools write to stderr; capture it instead of letting it become a terminating error.
    function Invoke-DialNative {
        param([string] $File, [string[]] $Arguments)
        $ErrorActionPreference = 'Continue'
        $lines = @(& $File @Arguments 2>&1 | ForEach-Object { "$_" })
        return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($lines -join "`n") }
    }

    function Get-DialStatusCode {
        param($ErrorRecord)
        $response = $ErrorRecord.Exception.Response
        if ($null -eq $response) { return 0 }
        return [int]$response.StatusCode
    }

    function Save-DialAsset {
        param([string] $Url, [string] $Path)
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Path -Headers @{ 'User-Agent' = 'dial-installer' }
        } catch {
            $status = Get-DialStatusCode $_
            if ($status -eq 404 -and -not $baseOverride) { throw $missingBuildMessage }
            if ($status -eq 404) { throw "Release asset not found: $Url" }
            throw "Download failed for ${Url}: $($_.Exception.Message)"
        }
    }

    function ConvertFrom-DialPublicKeyPem {
        param([string] $Pem)
        if ($Pem -notmatch '-----BEGIN PUBLIC KEY-----' -or $Pem -notmatch '-----END PUBLIC KEY-----') {
            throw 'The release public key is not a PEM public key.'
        }
        $body = ($Pem -replace '-----[^-]+-----', '') -replace '\s', ''
        try { $der = [Convert]::FromBase64String($body) } catch { throw 'The release public key PEM body is not valid base64.' }
        # SubjectPublicKeyInfo for Ed25519: 30 2a 30 05 06 03 2b 65 70 03 21 00, then 32 key bytes.
        if ($der.Length -ne 44 -or [BitConverter]::ToString($der, 0, 12) -ne '30-2A-30-05-06-03-2B-65-70-03-21-00') {
            throw 'The release public key must be an Ed25519 SubjectPublicKeyInfo key.'
        }
        $raw = New-Object byte[] 32
        [Array]::Copy($der, 12, $raw, 0, 32)
        return ,$raw
    }

    if ($env:OS -ne 'Windows_NT') { throw 'This installer runs on Windows only.' }

    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    $emulated = $false
    switch ($arch) {
        'AMD64' { }
        'ARM64' { $emulated = $true }
        default { throw "Unsupported Windows architecture '$arch'. Dial publishes x86_64-pc-windows-msvc only." }
    }
    $target = 'x86_64-pc-windows-msvc'

    if (-not $InstallDir) { $InstallDir = $env:DIAL_INSTALL_DIR }
    if (-not $InstallDir) { $InstallDir = Join-Path $HOME '.local\bin' }
    if ($Version -eq 'latest' -and $env:DIAL_VERSION) { $Version = $env:DIAL_VERSION }

    $publicKeyPem = $env:DIAL_RELEASE_PUBLIC_KEY_PEM
    if ([string]::IsNullOrWhiteSpace($publicKeyPem)) { $publicKeyPem = $DialReleasePublicKeyPem }
    if ([string]::IsNullOrWhiteSpace($publicKeyPem)) {
        throw 'This install.ps1 has no embedded release public key. Use the install.ps1 attached to a GitHub release, or set DIAL_RELEASE_PUBLIC_KEY_PEM to the release Ed25519 public key (PEM). Nothing was installed.'
    }
    $publicKey = ConvertFrom-DialPublicKeyPem $publicKeyPem

    # Test-only override: a loopback HTTP base URL serving the release assets directly.
    $baseOverride = $env:DIAL_RELEASE_BASE_URL
    if ($baseOverride -and $baseOverride -notmatch '^http://(127\.0\.0\.1|localhost)(:\d+)?(/|$)') {
        throw 'DIAL_RELEASE_BASE_URL is for tests only and must be an http://127.0.0.1 or localhost URL.'
    }

    $gh = Get-Command gh -CommandType Application -ErrorAction SilentlyContinue
    $useGh = $false
    if (-not $baseOverride -and $gh) {
        $auth = Invoke-DialNative $gh.Source @('auth', 'status', '--hostname', 'github.com')
        $useGh = ($auth.ExitCode -eq 0)
    }

    if ($Version -eq 'latest') {
        if ($baseOverride) { throw 'DIAL_RELEASE_BASE_URL needs an explicit version, not latest.' }
        if ($useGh) {
            $resolved = Invoke-DialNative $gh.Source @('release', 'view', '--repo', $repository, '--json', 'tagName', '--jq', '.tagName')
            if ($resolved.ExitCode -ne 0) { throw "gh could not resolve the latest release: $($resolved.Output)" }
            $tag = $resolved.Output.Trim()
        } else {
            try {
                $release = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$repository/releases/latest" -Headers @{ 'User-Agent' = 'dial-installer' }
            } catch {
                if ((Get-DialStatusCode $_) -eq 404) { throw $missingBuildMessage }
                throw "Could not resolve the latest release: $($_.Exception.Message)"
            }
            $tag = [string]$release.tag_name
        }
    } else {
        $tag = $Version
        if (-not $tag.StartsWith('v')) { $tag = 'v' + $tag }
    }
    if ($tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
        throw "Invalid release tag: $tag"
    }
    $versionNumber = $tag.Substring(1)

    $package = "dial-$tag-$target"
    $archiveName = "$package.zip"
    if ($baseOverride) {
        $base = $baseOverride.TrimEnd('/')
    } else {
        $base = "https://github.com/$repository/releases/download/$tag"
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $work
    $destination = [IO.Path]::GetFullPath($InstallDir)
    $staged = $null
    try {
        $archive = Join-Path $work $archiveName
        $manifest = Join-Path $work 'SHA256SUMS'
        $signature = Join-Path $work 'SHA256SUMS.sig'

        if ($useGh) {
            $fetch = Invoke-DialNative $gh.Source @('release', 'download', $tag, '--repo', $repository, '--dir', $work, '--clobber', '--pattern', $archiveName, '--pattern', 'SHA256SUMS', '--pattern', 'SHA256SUMS.sig')
            if ($fetch.ExitCode -ne 0) { throw "gh release download failed: $($fetch.Output)" }
        } else {
            Save-DialAsset "$base/$archiveName" $archive
            Save-DialAsset "$base/SHA256SUMS" $manifest
            Save-DialAsset "$base/SHA256SUMS.sig" $signature
        }

        $signatureText = [IO.File]::ReadAllText($signature) -replace '\s', ''
        $signatureBytes = [Convert]::FromBase64String($signatureText)
        if ($signatureBytes.Length -ne 64) { throw 'SHA256SUMS.sig does not hold a 64-byte Ed25519 signature; nothing was installed.' }
        $manifestBytes = [IO.File]::ReadAllBytes($manifest)

        $ed25519Type = 'DialInstallerEd25519V1'
        if (-not ($ed25519Type -as [type])) {
            Add-Type -TypeDefinition $ed25519Source -ReferencedAssemblies ([System.Numerics.BigInteger].Assembly.Location), ([System.Security.Cryptography.SHA512].Assembly.Location)
        }
        if (-not [DialInstallerEd25519V1]::Verify($publicKey, $manifestBytes, $signatureBytes)) {
            throw 'Release manifest signature verification failed; nothing was installed.'
        }

        $expected = $null
        $matchCount = 0
        foreach ($line in ([Text.Encoding]::UTF8.GetString($manifestBytes) -split "`n")) {
            $line = $line.TrimEnd("`r")
            if ($line -match '^([0-9a-fA-F]{64}) [ *](.+)$' -and $Matches[2] -eq $archiveName) {
                $expected = $Matches[1].ToLowerInvariant()
                $matchCount++
            }
        }
        if ($matchCount -ne 1) { throw "Signed manifest does not list $archiveName exactly once." }
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { throw 'Release archive digest mismatch; nothing was installed.' }

        $unpack = Join-Path $work 'unpack'
        Expand-Archive -LiteralPath $archive -DestinationPath $unpack
        $binary = Join-Path (Join-Path $unpack $package) 'dial.exe'
        $item = Get-Item -LiteralPath $binary -ErrorAction SilentlyContinue
        if (-not $item -or $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Release archive did not contain the expected regular dial.exe.'
        }
        $run = Invoke-DialNative $binary @('--version')
        $firstLine = ($run.Output -split "`n")[0].Trim()
        if ($run.ExitCode -ne 0 -or $firstLine -notmatch ('^dial ' + [regex]::Escape($versionNumber) + '( |$)')) {
            throw "Downloaded binary version check failed (expected dial $versionNumber): $firstLine"
        }

        $null = New-Item -ItemType Directory -Force -Path $destination
        $staged = Join-Path $destination ('.dial.new.' + [Guid]::NewGuid().ToString('N') + '.exe')
        Copy-Item -LiteralPath $binary -Destination $staged
        $installed = Join-Path $destination 'dial.exe'
        $backup = Join-Path $destination 'dial.exe.bak'
        $hadPrevious = Test-Path -LiteralPath $installed -PathType Leaf
        try {
            if ($hadPrevious) { Move-Item -LiteralPath $installed -Destination $backup -Force }
            Move-Item -LiteralPath $staged -Destination $installed
        } catch {
            if (Test-Path -LiteralPath $installed) { Remove-Item -LiteralPath $installed -Force -ErrorAction SilentlyContinue }
            if ($hadPrevious -and (Test-Path -LiteralPath $backup)) { Move-Item -LiteralPath $backup -Destination $installed -Force }
            throw
        }

        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $userEntries = @($userPath -split ';' | Where-Object { $_ })
        $inUserPath = @($userEntries | Where-Object { $_.TrimEnd('\') -ieq $destination.TrimEnd('\') }).Count -gt 0
        if (-not $inUserPath) {
            [Environment]::SetEnvironmentVariable('Path', (@($userEntries + $destination) -join ';'), 'User')
        }
        $sessionEntries = @($env:Path -split ';' | Where-Object { $_ })
        $inSessionPath = @($sessionEntries | Where-Object { $_.TrimEnd('\') -ieq $destination.TrimEnd('\') }).Count -gt 0
        if (-not $inSessionPath) {
            $env:Path = (@($sessionEntries + $destination) -join ';')
        }

        Write-Host "Installed Dial $versionNumber to $installed"
        Write-Host "$destination is on your user PATH and in this session's PATH."
        if ($emulated) { Write-Host 'ARM64 Windows: this is the x86_64 build, running under x64 emulation.' }
        Write-Host ''
        Write-Host 'Next steps:'
        Write-Host '  1. Run dial from inside a project directory.'
        Write-Host '  2. Shell, PDF and Python-kernel tools stay disabled on Windows until you opt in:'
        Write-Host '       dial config set sandbox.mode off'
        Write-Host '     That runs commands with your normal Windows privileges and no filesystem or network confinement.'
        Write-Host '     Only do this for trusted workspaces. See docs/windows.md.'
        if (-not $env:HOME -and -not $env:USERPROFILE -and -not $env:DIAL_HOME) {
            Write-Host '  Note: HOME and USERPROFILE are not set, so Dial cannot find its config. Set $env:DIAL_HOME to a directory before running Dial.'
        }
    } finally {
        if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

try {
    Install-DialRelease -Version $Version -InstallDir $InstallDir
} finally {
    Remove-Item -LiteralPath Function:\Install-DialRelease -ErrorAction SilentlyContinue
}
