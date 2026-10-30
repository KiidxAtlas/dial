//! `dial update` (and `/update`, which runs it): brings this executable up to date the way it was
//! installed, following [`release::plan`]: a verified signed release for installer copies, the
//! package manager for Homebrew and Cargo, and `git pull` plus `make update` for source builds.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use dial::release::{self, Installation, REPOSITORY, SIGNING_KEY, UpdatePlan};
use ring::signature;
use sha2::{Digest, Sha256};
use std::process::Command;
use std::{fs, path::Path, time::Duration};

fn target() -> Result<&'static str> {
    // The libc is fixed at compile time: a musl build must fetch the musl archive, never the glibc one.
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") if cfg!(target_env = "musl") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "musl") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
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

/// The version in a `dial <version> (<build info>)` line printed by `--version`.
fn dial_version_token(line: &str) -> Option<&str> {
    line.trim().strip_prefix("dial ")?.split_whitespace().next()
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
    let installation = Installation::of(&executable);
    match release::plan(
        installation,
        SIGNING_KEY.is_some(),
        release::source_checkout().as_deref(),
    ) {
        UpdatePlan::Release => install_release(&executable, version),
        UpdatePlan::PackageManager(installation) => run_package_manager(installation, version),
        UpdatePlan::Checkout(root) => update_checkout(&root, version),
        UpdatePlan::Manual(how) if installation == Installation::Nix => {
            println!("This installation is managed by Nix: {how}.");
            Ok(())
        }
        UpdatePlan::Manual(how) => bail!(
            "this build has no embedded release verification key and its source checkout is gone, so it cannot update itself; {how}"
        ),
    }
}

/// Runs `program ARGS` with this terminal's output (or `/update`'s transcript pipe).
fn run_visible(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let shown = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    println!("$ {shown}");
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let status = command.status().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("`{program}` was not found on PATH; it is needed to run `{shown}`")
        } else {
            anyhow::Error::new(error).context(format!("starting `{shown}`"))
        }
    })?;
    ensure!(status.success(), "`{shown}` failed ({status})");
    Ok(())
}

fn run_package_manager(installation: Installation, version: Option<&str>) -> Result<()> {
    let tag = version.map(release::tag).transpose()?;
    ensure!(
        tag.is_none() || installation != Installation::Homebrew,
        "Homebrew installs the version its formula pins; run `dial update` without --version"
    );
    let argv = installation
        .package_manager_argv(tag.as_deref())
        .context("this installation has no package-manager update command")?;
    println!("Updating Dial with {}.", installation.name());
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    run_visible(&argv[0], &args, None)
}

/// Whether the build from `revision` (dirty state `dirty`) is already `head`.
fn built_from(head: &str, revision: &str, dirty: &str) -> bool {
    dirty == "false"
        && revision.len() >= 7
        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        && head.starts_with(revision)
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fast-forwards the checkout this executable was built from, then rebuilds and reinstalls it
/// with `make update`. Uncommitted changes to tracked files stop it before anything changes, and
/// `git pull --ff-only` refuses a diverged branch, so local work is never merged or discarded.
fn update_checkout(root: &Path, version: Option<&str>) -> Result<()> {
    ensure!(
        version.is_none(),
        "this Dial is built from the source checkout at {}, which updates to the latest commit of its branch; to build a release, check out its tag there and run `make update`",
        root.display()
    );
    println!("Updating the source checkout at {}.", root.display());
    let changed = git_output(root, &["status", "--porcelain", "--untracked-files=no"])?;
    if !changed.is_empty() {
        const SHOWN: usize = 10;
        let lines: Vec<&str> = changed.lines().collect();
        let mut listing = lines[..lines.len().min(SHOWN)].join("\n");
        if lines.len() > SHOWN {
            listing.push_str(&format!("\n… and {} more", lines.len() - SHOWN));
        }
        bail!(
            "the checkout at {} has uncommitted changes to {} tracked files; commit or stash them, then run `dial update` again. Nothing was changed.\n{listing}",
            root.display(),
            lines.len()
        );
    }
    run_visible("git", &["pull", "--ff-only"], Some(root))?;
    let head = git_output(root, &["rev-parse", "HEAD"])?;
    let build = dial::build_info::REVISION;
    if built_from(&head, build, dial::build_info::DIRTY) {
        println!("Dial is up to date: it was built from {build}, the checkout's current commit.");
        return Ok(());
    }
    run_visible("make", &["update"], Some(root))?;
    println!(
        "Updated Dial to {} from {}.",
        &head[..head.len().min(12)],
        root.display()
    );
    Ok(())
}

fn install_release(executable: &Path, version: Option<&str>) -> Result<()> {
    let tag = match version {
        Some(version) => release::tag(version)?,
        None => {
            let latest = release::latest_tag(Duration::from_secs(30))
                .context("checking the latest Dial release")?;
            if !release::is_newer(&latest, release::CURRENT) {
                println!(
                    "Dial {} is up to date (latest release: {latest}).",
                    release::CURRENT
                );
                return Ok(());
            }
            latest
        }
    };
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
    let _install_lock = lock_install_dir(install_dir)?;
    let temp = tempfile::tempdir_in(install_dir)
        .context("creating same-filesystem update staging directory")?;
    let staged = if cfg!(windows) {
        executable.with_file_name("dial.update.new.exe")
    } else {
        temp.path().join("dial.new")
    };
    // Only a staged file outside `temp` needs its own cleanup (Windows stages beside the exe).
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut _staged_cleanup = StagedCleanup(cfg!(windows).then(|| staged.clone()));
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
        output.success,
        "downloaded executable failed its --version check"
    );
    let actual_version = &output.stdout;
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
            .env("DIAL_UPDATE_CURRENT", executable)
            .env("DIAL_UPDATE_STAGED", &staged)
            .env("DIAL_UPDATE_BACKUP", &backup)
            .env("DIAL_UPDATE_PARENT_PID", std::process::id().to_string())
            .env("DIAL_UPDATE_VERSION", tag.trim_start_matches('v'))
            .spawn()
            .context("starting atomic Windows installer")?;
        // The installer script now owns the staged file.
        _staged_cleanup.0 = None;
        println!(
            "Verified Dial {}. It will activate after this process exits; previous binary is backed up at {}.",
            tag.trim_start_matches('v'),
            backup.display()
        );
    }
    #[cfg(unix)]
    {
        activate_unix(executable, &staged, &backup, temp.path(), &|path: &Path| {
            verify_version(path, tag.trim_start_matches('v'))
        })?;
        println!(
            "Updated Dial to {} (backup: {})",
            tag.trim_start_matches('v'),
            backup.display()
        );
    }
    Ok(())
}

/// Removes a staged executable that lives outside the staging directory unless the update took
/// ownership of it, so a failed check does not leave an unverified binary beside the install.
struct StagedCleanup(Option<std::path::PathBuf>);

impl Drop for StagedCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// What a `--version` probe reported; stdout is capped by `run_version_check`.
struct VersionOutput {
    success: bool,
    stdout: String,
}

/// Runs `path --version` in its own process group with a deadline and a bounded output capture.
/// A binary that never answers, floods its pipes, or leaves a grandchild holding them open is
/// stopped with its group, so an untrusted download cannot hang or exhaust the update.
fn run_version_check(path: &Path) -> Result<VersionOutput> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
    const OUTPUT_CAP: usize = 64 * 1024;
    let mut command = std::process::Command::new(path);
    command.arg("--version");
    let finished = dial::runtime::process::run_with_control(
        command,
        None,
        dial::runtime::process::Policy::bounded(TIMEOUT, OUTPUT_CAP),
        &|| false,
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        !finished.timed_out,
        "{} did not answer --version within {}s",
        path.display(),
        TIMEOUT.as_secs()
    );
    Ok(VersionOutput {
        success: finished.success(),
        stdout: String::from_utf8_lossy(&finished.stdout).into_owned(),
    })
}

/// Exclusive lease on an install directory, so two `dial update` runs cannot interleave their
/// activation. Released when dropped.
struct InstallLock {
    _file: fs::File,
}

fn lock_install_dir(dir: &Path) -> Result<InstallLock> {
    use fs2::FileExt;
    let path = dir.join(".dial-update.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening update lock {}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(InstallLock { _file: file }),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => bail!(
            "another dial update is changing {}; wait for it to finish and retry",
            dir.display()
        ),
        Err(error) => Err(error).with_context(|| format!("locking {}", path.display())),
    }
}

/// Crash-safe activation of a verified staged executable (Unix). The active name is only ever
/// renamed over, never removed, so an interruption leaves a complete previous or complete new
/// executable at `executable`:
/// 1. copy the current executable to `backup`, verified by digest, without touching the active name;
/// 2. rename the staged file over the active name (atomic on the same filesystem);
/// 3. fsync the install directory and run `verify` on the active name.
///
/// A failure in 3 renames the verified backup back. If that rollback also fails, both errors are
/// reported and the backup stays at `backup` for manual recovery.
#[cfg(unix)]
fn activate_unix(
    executable: &Path,
    staged: &Path,
    backup: &Path,
    staging: &Path,
    verify: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let dir = executable
        .parent()
        .context("running executable has no parent directory")?;
    preserve_backup(executable, backup, staging)?;
    fs::rename(staged, executable).context("replacing the executable with the verified update")?;
    if let Err(error) = sync_dir(dir).and_then(|()| verify(executable)) {
        if let Err(restore) = fs::rename(backup, executable) {
            bail!(
                "{error:#}; restoring the previous executable also failed ({restore}); the previous executable is kept at {}",
                backup.display()
            );
        }
        return Err(error.context("new executable failed activation; previous version restored"));
    }
    Ok(())
}

/// Copies the current executable to `backup` through `staging` (same filesystem) and renames it
/// into place only after its digest matches the original.
#[cfg(unix)]
fn preserve_backup(executable: &Path, backup: &Path, staging: &Path) -> Result<()> {
    let copy = staging.join("rollback.dial");
    fs::copy(executable, &copy).context("copying the current executable for rollback")?;
    fs::File::open(&copy)
        .and_then(|file| file.sync_all())
        .context("syncing the rollback copy")?;
    ensure!(
        digest_file(&copy)? == digest_file(executable)?,
        "rollback copy does not match the current executable"
    );
    fs::rename(&copy, backup)
        .with_context(|| format!("preserving rollback copy at {}", backup.display()))?;
    sync_dir(
        backup
            .parent()
            .context("rollback path has no parent directory")?,
    )
}

#[cfg(unix)]
fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing directory {}", dir.display()))
}

/// Runs the candidate executable and requires it to report exactly `expected` as its version.
#[cfg(unix)]
fn verify_version(path: &Path, expected: &str) -> Result<()> {
    let output = run_version_check(path)?;
    ensure!(
        output.success
            && output
                .stdout
                .lines()
                .any(|line| dial_version_token(line) == Some(expected)),
        "{} does not report Dial {expected} from --version",
        path.display()
    );
    Ok(())
}
#[cfg(test)]
mod tests;
