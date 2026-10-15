//! Git execution helpers shared by the agentic and legacy commit flows.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{sync::atomic::AtomicBool, time::Duration};

use super::run::CommitError;

/// Run `git <args>` in `cwd` and return stdout. A failed command becomes [`CommitError::Git`]
/// with `context` and the captured stderr (or stdout) so the cause is readable.
pub(crate) fn git_output(cwd: &Path, args: &[&str], context: &str) -> Result<String, CommitError> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0");
    let timeout = Duration::from_secs(if args.contains(&"push") { 1800 } else { 120 });
    let output = crate::runtime::process::run(
        command,
        None,
        timeout,
        64 * 1024 * 1024,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .map_err(|error| CommitError::Git(format!("{context}: {error}")))?;
    check(output, context, timeout)
}

/// The repository's top-level directory. Git prints paths relative to it, but pathspecs such as
/// `git add -- <path>` resolve from the working directory, so the commit flows run from here.
pub(crate) fn repo_top(cwd: &Path) -> Result<PathBuf, CommitError> {
    let top = git_output(cwd, &["rev-parse", "--show-toplevel"], "Commit failed")?;
    Ok(PathBuf::from(top.trim_end_matches(['\n', '\r'])))
}

/// Restores the git index to its captured bytes when dropped. A dry run builds its staged diff with
/// `git add -A`, and this keeps the user's staging area unchanged afterwards.
pub(crate) struct IndexSnapshot {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    restored: bool,
}

impl IndexSnapshot {
    pub(crate) fn capture(cwd: &Path) -> Result<Self, CommitError> {
        const CONTEXT: &str = "Commit preview failed";
        let index = git_output(cwd, &["rev-parse", "--git-path", "index"], CONTEXT)?;
        let path = cwd.join(index.trim_end_matches(['\n', '\r']));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(CommitError::Git(format!(
                    "{CONTEXT}: {}: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            bytes,
            restored: false,
        })
    }

    /// Writes the captured index back and reports failure, so a dry run cannot silently leave the
    /// user's staging area modified. Drop repeats the write only if this was never called.
    pub(crate) fn restore(mut self) -> Result<(), CommitError> {
        self.restored = true;
        write_back(&self.path, self.bytes.as_deref()).map_err(|error| {
            CommitError::Git(format!(
                "Restoring the git index failed: {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn write_back(path: &Path, bytes: Option<&[u8]>) -> std::io::Result<()> {
    match bytes {
        Some(bytes) => {
            // Temp file in the index's own directory, synced, then renamed over it: a crash
            // mid-restore must not leave a truncated `.git/index`. The original mode is kept.
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            let mode = std::fs::metadata(path).ok().map(|meta| meta.permissions());
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            tmp.write_all(bytes)?;
            if let Some(mode) = mode {
                tmp.as_file().set_permissions(mode)?;
            }
            tmp.as_file().sync_all()?;
            tmp.persist(path).map(|_| ()).map_err(|error| error.error)
        }
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

impl Drop for IndexSnapshot {
    fn drop(&mut self) {
        if !self.restored {
            let _ = write_back(&self.path, self.bytes.as_deref());
        }
    }
}

/// Create a commit with `message` on stdin, stripping comment lines (`git commit --cleanup=strip -F -`).
/// With `bypass_hooks`, repository hooks are off and signing is skipped, as the agent `git` tool does.
pub(crate) fn commit_with_message(
    cwd: &Path,
    message: &str,
    bypass_hooks: bool,
) -> Result<(), CommitError> {
    const CONTEXT: &str = "Commit failed";
    let mut command = Command::new("git");
    if bypass_hooks {
        command.args(crate::tools::git::hooks_off_args());
    }
    command.args(["commit", "--cleanup=strip", "-F", "-"]);
    if bypass_hooks {
        command.args(["--no-verify"]);
    }
    command.current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0");
    let timeout = Duration::from_secs(120);
    let output = crate::runtime::process::run(
        command,
        Some(message.as_bytes().to_vec()),
        timeout,
        64 * 1024 * 1024,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .map_err(|error| CommitError::Git(format!("{CONTEXT}: {error}")))?;
    check(output, CONTEXT, timeout).map(|_| ())
}

/// Push the current branch. A refused push (missing upstream, no remote, rejected ref)
/// is reported as [`CommitError::Git`] rather than panicking or escaping as an I/O error.
/// With `bypass_hooks`, the push skips repository hooks, matching the agent `git` tool.
pub(crate) fn push_or_abort(cwd: &Path, bypass_hooks: bool) -> Result<(), CommitError> {
    let mut args: Vec<&str> = Vec::new();
    if bypass_hooks {
        args.extend(crate::tools::git::hooks_off_args());
    }
    args.push("push");
    if bypass_hooks {
        args.push("--no-verify");
    }
    git_output(cwd, &args, "Push failed").map(|_| ())
}

fn check(
    output: crate::runtime::process::Finished,
    context: &str,
    timeout: Duration,
) -> Result<String, CommitError> {
    if output.success() {
        return Ok(output.stdout_text());
    }
    Err(CommitError::Git(output.failure(context, timeout)))
}
