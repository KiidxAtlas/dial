//! Verified self-update for binaries installed by the release installer.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use ring::signature;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const REPOSITORY: &str = "KiidxAtlas/dial";
const SIGNING_KEY: Option<&str> = option_env!("DIAL_RELEASE_PUBLIC_KEY");

fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => bail!(
            "automatic updates are not available for this platform; see https://github.com/{REPOSITORY}/releases"
        ),
    }
}

/// Release archives are tens of megabytes; ureq's default 10 MB body cap would reject them.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

fn fetch(url: &str) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .header("User-Agent", "dial-updater")
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(900)))
        .build()
        .call()
        .with_context(|| format!("download failed: {url}"))?;
    let mut body = response.into_body();
    body.with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .context("reading download response")
}

fn release_tag(version: Option<&str>) -> Result<String> {
    if let Some(version) = version {
        let tag = if version.starts_with('v') {
            version.to_owned()
        } else {
            format!("v{version}")
        };
        ensure!(
            tag[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
            "invalid version tag"
        );
        return Ok(tag);
    }
    let value: serde_json::Value = ureq::get(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/latest"
    ))
    .header("User-Agent", "dial-updater")
    .config()
    .timeout_global(Some(std::time::Duration::from_secs(30)))
    .build()
    .call()?
    .into_body()
    .read_json()?;
    value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("GitHub latest release has no tag_name")
}

/// The version in a `dial <version> (<build info>)` line printed by `--version`.
fn dial_version_token(line: &str) -> Option<&str> {
    line.trim().strip_prefix("dial ")?.split_whitespace().next()
}

fn installation_kind(path: &Path) -> &'static str {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.contains("/Cellar/") || text.contains("/homebrew/") {
        "Homebrew"
    } else if text.contains("/.cargo/bin/") {
        "Cargo"
    } else if text.contains("/nix/store/") {
        "Nix"
    } else {
        "script"
    }
}

fn verify_signature_with_key(sums: &[u8], signature_text: &[u8], public_key: &str) -> Result<()> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(public_key.trim())
        .context("invalid embedded release public key")?;
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(std::str::from_utf8(signature_text)?.trim())
        .context("invalid release signature encoding")?;
    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(sums, &signature_bytes)
        .map_err(|_| anyhow::anyhow!("release signature verification failed"))
}

fn verify_signature(sums: &[u8], signature_text: &[u8]) -> Result<()> {
    let key = SIGNING_KEY.context("this build has no configured release verification key; refusing self-update. Install through the trusted package manager or configure DIAL_RELEASE_PUBLIC_KEY at release build time.")?;
    verify_signature_with_key(sums, signature_text, key)
}

fn checksum(sums: &str, archive: &str) -> Result<String> {
    for line in sums.lines() {
        let (hash, name) = line.split_once(char::is_whitespace).unwrap_or(("", ""));
        if name.trim_start().trim_start_matches('*') == archive {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
                "malformed release checksum"
            );
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!("release checksum manifest does not contain {archive}")
}

pub fn run(version: Option<&str>) -> Result<()> {
    let executable = std::env::current_exe().context("locating running executable")?;
    let kind = installation_kind(&executable);
    if kind != "script" {
        let command = match kind {
            "Homebrew" => "brew upgrade dial",
            "Cargo" => "cargo install --locked dial",
            "Nix" => "update the dial flake input and rebuild",
            _ => unreachable!(),
        };
        println!("This installation is managed by {kind}. Update it with: {command}");
        return Ok(());
    }
    let tag = release_tag(version)?;
    let platform = target()?;
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive_name = format!("dial-{tag}-{platform}.{extension}");
    let base = format!("https://github.com/{REPOSITORY}/releases/download/{tag}");
    let sums = fetch(&format!("{base}/SHA256SUMS"))?;
    let signature_text = fetch(&format!("{base}/SHA256SUMS.sig"))?;
    verify_signature(&sums, &signature_text)?;
    let expected = checksum(std::str::from_utf8(&sums)?, &archive_name)?;
    let archive = fetch(&format!("{base}/{archive_name}"))?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    ensure!(actual == expected, "release archive checksum mismatch");

    let install_dir = executable
        .parent()
        .context("running executable has no parent directory")?;
    let temp = tempfile::tempdir_in(install_dir)
        .context("creating same-filesystem update staging directory")?;
    let staged = if cfg!(windows) {
        executable.with_file_name("dial.update.new.exe")
    } else {
        temp.path().join("dial.new")
    };
    #[cfg(windows)]
    {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&archive))?;
        let member = format!("dial-{tag}-{platform}/dial.exe");
        let mut binary = zip
            .by_name(&member)
            .context("release ZIP is missing its dial.exe")?;
        ensure!(
            binary.is_file(),
            "release executable is not a regular ZIP file"
        );
        let mut output = fs::File::create(&staged)?;
        std::io::copy(&mut binary, &mut output)?;
        output.sync_all()?;
    }
    #[cfg(not(windows))]
    {
        let decoder = flate2::read::GzDecoder::new(archive.as_slice());
        let mut tar = tar::Archive::new(decoder);
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path != format!("dial-{tag}-{platform}/dial") {
                continue;
            }
            ensure!(
                entry.header().entry_type().is_file(),
                "release executable is not a regular file"
            );
            let mut output = fs::File::create(&staged)?;
            std::io::copy(&mut entry, &mut output)?;
            output.sync_all()?;
            found = true;
            break;
        }
        ensure!(found, "release archive is missing dial");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        }
    }
    let output = run_version_check(&staged).context("checking downloaded executable")?;
    ensure!(
        output.status.success(),
        "downloaded executable failed its --version check"
    );
    let actual_version = String::from_utf8_lossy(&output.stdout);
    let expected = tag.trim_start_matches('v');
    ensure!(
        actual_version
            .lines()
            .any(|line| dial_version_token(line) == Some(expected)),
        "downloaded executable version does not match {tag}"
    );
    let backup = executable.with_extension("dial.bak");
    #[cfg(windows)]
    {
        let script = r#"
$current = $env:DIAL_UPDATE_CURRENT
$staged = $env:DIAL_UPDATE_STAGED
$backup = $env:DIAL_UPDATE_BACKUP
Wait-Process -Id ([int]$env:DIAL_UPDATE_PARENT_PID) -ErrorAction SilentlyContinue
if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
Move-Item -LiteralPath $current -Destination $backup
try {
  Move-Item -LiteralPath $staged -Destination $current
  $output = & $current --version 2>&1
  if ($LASTEXITCODE -ne 0 -or ($output -join "`n") -notmatch [regex]::Escape($env:DIAL_UPDATE_VERSION)) { throw "new binary failed version check" }
} catch {
  if (Test-Path -LiteralPath $current) { Remove-Item -LiteralPath $current -Force }
  Move-Item -LiteralPath $backup -Destination $current
  Write-Error $_
  exit 1
}
"#;
        std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ])
            .env("DIAL_UPDATE_CURRENT", &executable)
            .env("DIAL_UPDATE_STAGED", &staged)
            .env("DIAL_UPDATE_BACKUP", &backup)
            .env("DIAL_UPDATE_PARENT_PID", std::process::id().to_string())
            .env("DIAL_UPDATE_VERSION", tag.trim_start_matches('v'))
            .spawn()
            .context("starting atomic Windows installer")?;
        println!(
            "Verified Dial {}. It will activate after this process exits; previous binary is backed up at {}.",
            tag.trim_start_matches('v'),
            backup.display()
        );
    }
    #[cfg(not(windows))]
    {
        if backup.exists() {
            fs::remove_file(&backup)?;
        }
        fs::rename(&executable, &backup).context("moving current executable to rollback backup")?;
        match fs::rename(&staged, &executable) {
            Ok(()) => {
                let check = run_version_check(&executable);
                let expected = tag.trim_start_matches('v');
                if !check.is_ok_and(|o| {
                    o.status.success()
                        && String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .any(|line| dial_version_token(line) == Some(expected))
                }) {
                    let _ = fs::remove_file(&executable);
                    fs::rename(&backup, &executable)
                        .context("new version failed; restoring previous executable")?;
                    bail!(
                        "new executable failed its post-install version check; previous version restored"
                    )
                }
            }
            Err(error) => {
                fs::rename(&backup, &executable)
                    .context("install failed; restoring previous executable")?;
                return Err(error.into());
            }
        }
        println!(
            "Updated Dial to {} (backup: {})",
            tag.trim_start_matches('v'),
            backup.display()
        );
    }
    Ok(())
}

/// Runs `path --version` in its own process group with a deadline. A binary that never answers is
/// killed with its group, so a stuck download cannot hang the update.
fn run_version_check(path: &Path) -> Result<std::process::Output> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
    let mut command = std::process::Command::new(path);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = dial::platform::process::spawn_group(&mut command)
        .with_context(|| format!("starting {}", path.display()))?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(output) => Ok(output?),
        Err(_) => {
            let _ = dial::platform::process::kill_group(pid);
            bail!(
                "{} did not answer --version within {}s",
                path.display(),
                TIMEOUT.as_secs()
            )
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_selects_exact_asset_and_normalizes_hash() {
        let manifest = format!(
            "{}  dial-v1-x.tar.gz\n{}  dial-v1-y.tar.gz\n",
            "A".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(
            checksum(&manifest, "dial-v1-x.tar.gz").unwrap(),
            "a".repeat(64)
        );
        assert!(checksum(&manifest, "../dial-v1-x.tar.gz").is_err());
    }

    #[test]
    fn signature_verification_accepts_only_the_signed_manifest() {
        let key = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
        let signature = "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw==";
        assert!(verify_signature_with_key(b"", signature.as_bytes(), key).is_ok());
        assert!(verify_signature_with_key(b"changed", signature.as_bytes(), key).is_err());
        assert!(verify_signature_with_key(b"", b"not base64!", key).is_err());
    }

    #[test]
    fn installation_detection_preserves_package_manager_ownership() {
        assert_eq!(
            installation_kind(Path::new("/opt/homebrew/Cellar/dial/bin/dial")),
            "Homebrew"
        );
        assert_eq!(
            installation_kind(Path::new("/home/user/.cargo/bin/dial")),
            "Cargo"
        );
        assert_eq!(
            installation_kind(Path::new("/nix/store/hash-dial/bin/dial")),
            "Nix"
        );
        assert_eq!(
            installation_kind(Path::new("/home/user/.local/bin/dial")),
            "script"
        );
        assert_eq!(
            installation_kind(Path::new(r"C:\Users\user\.cargo\bin\dial.exe")),
            "Cargo"
        );
    }
}
