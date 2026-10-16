//! Git execution helpers shared by the agentic and legacy commit flows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{sync::atomic::AtomicBool, time::Duration};

use super::run::CommitError;

/// Run `git <args>` in `cwd` and return stdout. A failed command becomes [`CommitError::Git`]
/// with `context` and the captured stderr (or stdout) so the cause is readable.
pub(crate) fn git_output(cwd: &Path, args: &[&str], context: &str) -> Result<String, CommitError> {
    git_output_indexed(cwd, None, args, context)
}

/// [`git_output`] with an optional private index. `index` sets `GIT_INDEX_FILE`, so the command
/// reads and writes that index instead of the repository's own (see [`PreviewIndex`]).
pub(crate) fn git_output_indexed(
    cwd: &Path,
    index: Option<&Path>,
    args: &[&str],
    context: &str,
) -> Result<String, CommitError> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
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

/// A private copy of the repository's index for a dry run. Preview staging and diffs run with
/// `GIT_INDEX_FILE` pointing at this copy, so the user's index is never written: a concurrent
/// `git add` by another terminal or editor is never overwritten, and a crash leaves nothing staged.
/// The copy lives in a temporary directory that is removed on drop.
pub(crate) struct PreviewIndex {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl PreviewIndex {
    /// Copies the repository's current index bytes. An absent index (a fresh repository) yields an
    /// absent copy, which git creates on first use.
    pub(crate) fn capture(cwd: &Path) -> Result<Self, CommitError> {
        const CONTEXT: &str = "Commit preview failed";
        let index = git_output(cwd, &["rev-parse", "--git-path", "index"], CONTEXT)?;
        let source = cwd.join(index.trim_end_matches(['\n', '\r']));
        let dir =
            tempfile::tempdir().map_err(|error| CommitError::Git(format!("{CONTEXT}: {error}")))?;
        let path = dir.path().join("index");
        match std::fs::copy(&source, &path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CommitError::Git(format!(
                    "{CONTEXT}: {}: {error}",
                    source.display()
                )));
            }
        }
        Ok(Self { _dir: dir, path })
    }

    /// The private index file, for `GIT_INDEX_FILE`.
    pub(crate) fn path(&self) -> &Path {
        &self.path
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
