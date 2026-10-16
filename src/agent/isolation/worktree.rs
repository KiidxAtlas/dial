//! `git worktree` isolation backend.
//!
//! The worker's tree is a detached checkout of HEAD created by `git worktree add`, then made to
//! match the project: git-excluded paths are removed, and every uncommitted, untracked and
//! ignored-but-not-excluded file is copied from the project. The parent repository only gains a
//! worktree administrative directory, which is removed when the isolated copy is released.

use crate::workspace_copy::is_excluded_source_path;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Whether the project can be checked out as a worktree: it must be the top of a git repository
/// that has at least one commit.
pub(super) fn probe(project: &Path) -> Result<(), String> {
    let top = git(project, &["rev-parse", "--show-toplevel"])?;
    let top = std::str::from_utf8(&top)
        .map_err(|_| "git printed a non-UTF-8 repository path".to_owned())?
        .trim_end_matches(['\n', '\r']);
    let top = fs::canonicalize(top).map_err(|error| format!("resolving {top}: {error}"))?;
    let root = fs::canonicalize(project)
        .map_err(|error| format!("resolving {}: {error}", project.display()))?;
    if top != root {
        return Err("the project is not the top of its git repository".into());
    }
    git(project, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map_err(|_| "the repository has no commits yet".to_owned())?;
    Ok(())
}

/// Owns the worktree administrative directory until [`Link::remove`] runs or it is dropped.
pub(super) struct Link {
    admin: Option<PathBuf>,
}

impl Link {
    /// Remove the parent repository's administrative entry for the worktree. The worktree's
    /// `.git` file is already gone, so git would otherwise report it as prunable forever.
    pub(super) fn remove(&mut self) -> Result<(), String> {
        let Some(admin) = self.admin.clone() else {
            return Ok(());
        };
        match fs::remove_dir_all(&admin) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(format!(
                "removing git worktree metadata {}: {error}",
                admin.display()
            )),
            _ => {
                self.admin = None;
                Ok(())
            }
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Create the checkout at `dest` (which must not exist) and make it match `project`.
///
/// Copies of uncommitted, untracked and ignored-but-not-excluded files are charged against
/// `max_bytes`; exceeding it fails the whole population.
///
/// `scratch` holds an empty hooks directory: `git worktree add` runs the parent's hooks unless
/// `core.hooksPath` points elsewhere, and an isolated copy must not run user hooks.
pub(super) fn populate(
    project: &Path,
    scratch: &Path,
    dest: &Path,
    max_bytes: u64,
) -> Result<Link, String> {
    let hooks = scratch.join("no-hooks");
    fs::create_dir_all(&hooks).map_err(|error| io_error("creating the hooks directory", error))?;
    let hooks_config = format!("core.hooksPath={}", hooks.display());
    let dest_text = dest
        .to_str()
        .ok_or_else(|| "the isolated copy path is not UTF-8".to_owned())?;
    git(
        project,
        &[
            "-c",
            hooks_config.as_str(),
            "worktree",
            "add",
            "--detach",
            "--quiet",
            dest_text,
            "HEAD",
        ],
    )?;
    // Register the administrative directory before anything else can fail, so it is removed on
    // every path from here on.
    let admin = git(dest, &["rev-parse", "--git-dir"])?;
    let admin = std::str::from_utf8(&admin)
        .map_err(|_| "git printed a non-UTF-8 administrative path".to_owned())?
        .trim_end_matches(['\n', '\r']);
    let link = Link {
        admin: Some(dest.join(admin)),
    };
    // Detach the checkout from the parent repository: the isolated copy gets its own `.git`
    // (created by `isolation_git`), never the parent's refs, index or hooks.
    fs::remove_file(dest.join(".git"))
        .map_err(|error| io_error("detaching the worktree", error))?;

    for tracked in split_nul(&git(project, &["ls-files", "-z"])?) {
        let target = dest.join(tracked?);
        if is_excluded_source_path(dest, &target) {
            remove_file_if_present(&target)
                .map_err(|error| io_error("removing an excluded file", error))?;
        }
    }
    let status = git(
        project,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=traditional",
        ],
    )?;
    let mut remaining = max_bytes;
    let mut entries = split_nul(&status);
    while let Some(entry) = entries.next() {
        let entry = entry?;
        ensure_status_entry(&entry)?;
        let (code, path) = entry.split_at(3);
        sync_path(project, dest, path, &mut remaining)
            .map_err(|error| io_error("copying a changed file", error))?;
        // Renames and copies carry the original path as the next NUL-separated token.
        if code.starts_with("R") || code.starts_with("C") {
            let original = entries
                .next()
                .ok_or_else(|| "git status ended after a rename".to_owned())??;
            sync_path(project, dest, &original, &mut remaining)
                .map_err(|error| io_error("copying a renamed file", error))?;
        }
    }
    Ok(link)
}

/// Bring `dest/relative` in line with `project/relative`. A trailing `/` marks an ignored
/// directory from git status; its files are copied through the same exclusion rules.
fn sync_path(project: &Path, dest: &Path, relative: &str, remaining: &mut u64) -> io::Result<()> {
    let is_directory = relative.ends_with('/');
    let relative = relative.trim_end_matches('/');
    let source = project.join(relative);
    let target = dest.join(relative);
    if is_directory {
        if fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_dir()) {
            copy_tree(&source, &target, dest, remaining)?;
        }
        return Ok(());
    }
    let source_meta = fs::symlink_metadata(&source);
    if source_meta.as_ref().is_ok_and(|meta| meta.is_file())
        && !is_excluded_source_path(dest, &target)
    {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        charge(remaining, fs::symlink_metadata(&source)?.len())?;
        return crate::platform::fs::copy_on_write(&source, &target);
    }
    // Deleted, excluded, symlinked or non-file in the project: the copy must not keep it.
    remove_file_if_present(&target)
}

/// Copy the non-excluded regular files under `source` into `target`. Symlinks are not copied.
/// Each copied file is charged against `remaining` before it is written.
fn copy_tree(source: &Path, target: &Path, root: &Path, remaining: &mut u64) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let to = target.join(entry.file_name());
        if is_excluded_source_path(root, &to) {
            continue;
        }
        let from = entry.path();
        let meta = fs::symlink_metadata(&from)?;
        if meta.is_dir() {
            copy_tree(&from, &to, root, remaining)?;
        } else if meta.is_file() {
            charge(remaining, meta.len())?;
            crate::platform::fs::copy_on_write(&from, &to)?;
        }
    }
    Ok(())
}

/// Spend `len` bytes of the dependency budget, failing before anything is copied past it.
fn charge(remaining: &mut u64, len: u64) -> io::Result<()> {
    if len > *remaining {
        return Err(io::Error::other(
            "isolated dependency budget exceeded; increase task.isolation_dependency_bytes or disable dependency copying",
        ));
    }
    *remaining -= len;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_status_entry(entry: &str) -> Result<(), String> {
    if entry.len() > 3 && entry.as_bytes()[2] == b' ' {
        Ok(())
    } else {
        Err(format!("unexpected git status entry: {entry:?}"))
    }
}

/// Split git's NUL-terminated output into UTF-8 entries. Non-UTF-8 paths are reported as errors
/// because they cannot be mapped reliably back to the project.
fn split_nul(output: &[u8]) -> impl Iterator<Item = Result<String, String>> + '_ {
    output
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
        .map(|item| {
            std::str::from_utf8(item)
                .map(str::to_owned)
                .map_err(|_| "git printed a non-UTF-8 path".to_owned())
        })
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.iter()
                .find(|arg| !arg.starts_with('-') && !arg.contains('='))
                .copied()
                .unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn io_error(action: &str, error: io::Error) -> String {
    format!("{action}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A repository with one commit: tracked `kept.txt`, `.env` and `sub/tracked.txt`.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "--quiet"]);
        run(root, &["config", "user.email", "dial@example.invalid"]);
        run(root, &["config", "user.name", "dial"]);
        fs::write(root.join("kept.txt"), "original").unwrap();
        fs::write(root.join("removed.txt"), "gone soon").unwrap();
        fs::write(root.join(".env"), "TOKEN=secret").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/tracked.txt"), "tracked").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "--quiet", "-m", "initial"]);
        dir
    }

    #[test]
    fn probe_rejects_a_repository_without_commits_and_a_subdirectory() {
        let empty = tempfile::tempdir().unwrap();
        run(empty.path(), &["init", "--quiet"]);
        assert!(probe(empty.path()).unwrap_err().contains("no commits"));

        let repo = repository();
        let sub = repo.path().join("sub");
        assert!(
            probe(&sub)
                .unwrap_err()
                .contains("top of its git repository")
        );
        assert!(probe(repo.path()).is_ok());
    }

    #[test]
    fn populate_matches_the_project_and_excludes_secrets() {
        let repo = repository();
        let project = repo.path();
        fs::write(project.join("kept.txt"), "edited uncommitted").unwrap();
        fs::remove_file(project.join("removed.txt")).unwrap();
        fs::write(project.join("new.txt"), "untracked").unwrap();
        fs::write(project.join("ignored.log"), "ignored but not excluded").unwrap();
        fs::create_dir(project.join("cache")).unwrap();
        fs::write(project.join("cache/entry.bin"), "cached").unwrap();
        fs::write(project.join(".gitignore"), "*.log\ncache/\n").unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let dest = scratch.path().join("work");
        let mut link = populate(project, scratch.path(), &dest, u64::MAX).unwrap();

        assert_eq!(
            fs::read_to_string(dest.join("kept.txt")).unwrap(),
            "edited uncommitted"
        );
        assert!(
            !dest.join("removed.txt").exists(),
            "deleted files are removed"
        );
        assert_eq!(
            fs::read_to_string(dest.join("new.txt")).unwrap(),
            "untracked"
        );
        assert_eq!(
            fs::read_to_string(dest.join("ignored.log")).unwrap(),
            "ignored but not excluded"
        );
        assert_eq!(
            fs::read_to_string(dest.join("cache/entry.bin")).unwrap(),
            "cached"
        );
        assert!(
            !dest.join(".env").exists(),
            "excluded secrets are not visible"
        );
        assert_eq!(
            fs::read_to_string(dest.join("sub/tracked.txt")).unwrap(),
            "tracked"
        );
        assert!(
            !dest.join(".git").is_dir(),
            "the parent's git data is not visible"
        );
        assert!(!dest.join(".git").exists(), "the worktree file is detached");

        let admin = project.join(".git/worktrees");
        assert!(admin.is_dir(), "git registered the worktree");
        link.remove().unwrap();
        assert!(!admin.join("work").exists(), "administrative entry removed");
        assert!(link.remove().is_ok(), "remove is idempotent");
    }

    #[test]
    fn populate_stops_at_the_dependency_budget() {
        let repo = repository();
        let project = repo.path();
        fs::write(project.join("new.txt"), "0123456789").unwrap();
        let scratch = tempfile::tempdir().unwrap();

        let error = populate(project, scratch.path(), &scratch.path().join("small"), 4)
            .err()
            .expect("a copy over the budget must fail");
        assert!(
            error.contains("isolated dependency budget exceeded"),
            "unexpected error: {error}"
        );
        assert!(populate(project, scratch.path(), &scratch.path().join("large"), 10).is_ok());
    }
}
