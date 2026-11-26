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

#[cfg(windows)]
mod windows;

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
        UpdatePlan::Manual(how)
            if matches!(installation, Installation::Nix | Installation::Pacman) =>
        {
            println!(
                "This installation is managed by {}: {how}.",
                installation.name()
            );
            Ok(())
        }
        UpdatePlan::Manual(how) => bail!("automatic update unavailable: {how}"),
    }
}

/// How long `dial update --check` waits for GitHub.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// `dial update --check`: reports whether a newer release exists and how `dial update` would
/// install it, without changing anything. With `json` the answer is one [`release::UpdateStatus`]
/// object and an unreachable GitHub is reported in its `error` field rather than as a failure, so
/// a frontend always gets a parseable reply.
pub fn check(json: bool) -> Result<()> {
    let installation = Installation::current();
    let status = release::status(
        installation,
        &release::current_plan(),
        release::CURRENT,
        release::latest_tag(CHECK_TIMEOUT).map_err(|error| format!("{error:#}")),
    );
    if json {
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    let Some(latest) = &status.latest else {
        bail!(
            "could not read the latest release: {}",
            status.error.as_deref().unwrap_or("unknown error")
        );
    };
    if status.update_available {
        println!(
            "Dial {latest} is available (you have {}); {}.",
            status.current,
            if status.can_update {
                "run `dial update`"
            } else {
                status.update_command.as_str()
            }
        );
    } else {
        println!(
            "Dial {} is up to date (latest release {latest}).",
            status.current
        );
    }
    Ok(())
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
    let install_dir = executable
        .parent()
        .context("running executable has no parent directory")?;
    let _install_lock = lock_install_dir(install_dir)?;
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

    let temp = tempfile::tempdir_in(install_dir)
        .context("creating same-filesystem update staging directory")?;
    let staged = temp.path().join(if cfg!(windows) {
        "dial.new.exe"
    } else {
        "dial.new"
    });
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
    let expected = tag.trim_start_matches('v');
    verify_version(&staged, expected).context("checking downloaded executable")?;
    let backup = executable.with_extension("dial.bak");
    activate(executable, &staged, &backup, temp.path(), &|path| {
        verify_version(path, expected)
    })?;
    println!("Updated Dial to {expected} (backup: {})", backup.display());
    Ok(())
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

impl Drop for InstallLock {
    fn drop(&mut self) {
        // A concurrent fork can retain this open-file description until exec.
        // End our lease explicitly rather than waiting for all those copies to close.
        let _ = self._file.unlock();
    }
}

fn lock_install_dir(dir: &Path) -> Result<InstallLock> {
    let path = dir.join(".dial-update.lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Also interoperate with the PowerShell installer: FileShare.None excludes both
        // readers and writers, and prevents renaming the lock file out from under the owner.
        options.share_mode(0);
    }
    let file = match options.open(&path) {
        #[cfg(windows)]
        Err(error)
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32) =>
        {
            bail!(
                "another Dial installer or update is changing {}; wait for it to finish and retry",
                dir.display()
            )
        }
        result => result.with_context(|| format!("opening update lock {}", path.display()))?,
    };
    match file.try_lock().map_err(std::io::Error::from) {
        Ok(()) => Ok(InstallLock { _file: file }),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => bail!(
            "another dial update is changing {}; wait for it to finish and retry",
            dir.display()
        ),
        Err(error) => Err(error).with_context(|| format!("locking {}", path.display())),
    }
}

/// Publish the verified candidate and confirm the *installed* name before releasing the lock
/// or reporting success. Unix preserves a digest-checked rollback copy first; Windows asks the
/// filesystem to preserve the old (possibly mapped) image as part of the replacement.
fn activate(
    executable: &Path,
    staged: &Path,
    backup: &Path,
    _staging: &Path,
    verify: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    #[cfg(unix)]
    {
        preserve_backup(executable, backup, _staging)?;
        fs::rename(staged, executable)
            .context("replacing the executable with the verified update")?;
    }
    #[cfg(windows)]
    windows::replace(executable, staged, backup)?;
    let check = || {
        #[cfg(unix)]
        sync_dir(
            executable
                .parent()
                .context("running executable has no parent directory")?,
        )?;
        verify(executable)
    };
    if let Err(error) = check() {
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
