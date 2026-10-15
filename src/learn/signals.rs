//! Implicit feedback for "Teach this model": a turn whose file changes later land in a commit
//! exactly as the turn left them was accepted by the user.
//!
//! Git does the content addressing. At the end of a turn we record HEAD from before the turn and
//! the blob id of every project file the turn changed; the turn counts as committed once HEAD has
//! moved and holds those same blobs (or no longer holds a file the turn deleted). Edits the user
//! made before committing change the blobs, so a corrected turn is not credited.

use std::collections::HashMap;

/// Runs `git -C <project root> <args>` and returns stdout on success.
pub type Git<'a> = &'a dyn Fn(&[&str]) -> Option<String>;

/// A finished turn waiting to see whether its changes get committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnChange {
    pub trajectory: i64,
    /// HEAD before the turn started.
    base: String,
    /// Project-relative path → blob id as the turn left it (`None`: the turn deleted it).
    files: Vec<(String, Option<String>)>,
}

/// The current commit, or `None` outside a repository or before its first commit.
pub fn head(git: Git<'_>) -> Option<String> {
    let head = git(&["rev-parse", "--verify", "--quiet", "HEAD"])?;
    let head = head.trim();
    (!head.is_empty()).then(|| head.to_owned())
}

/// Snapshot the files a turn changed. `None` when nothing it changed differs from `base`, or
/// when git cannot answer; paths outside the project root (added directories) are ignored.
pub fn capture(
    git: Git<'_>,
    root: &std::path::Path,
    trajectory: i64,
    base: String,
    changed: &[String],
) -> Option<TurnChange> {
    let paths: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| !path.is_empty() && !std::path::Path::new(path).is_absolute())
        .collect();
    if paths.is_empty() {
        return None;
    }
    let present: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| root.join(path).is_file())
        .collect();
    let mut blobs = HashMap::new();
    if !present.is_empty() {
        let mut args = vec!["hash-object", "--"];
        args.extend(&present);
        let ids = git(&args)?;
        let ids: Vec<&str> = ids.lines().collect();
        if ids.len() != present.len() {
            return None;
        }
        blobs.extend(present.iter().zip(ids).map(|(p, id)| (*p, id.to_owned())));
    }
    let files: Vec<(String, Option<String>)> = paths
        .iter()
        .map(|path| ((*path).to_owned(), blobs.get(path).cloned()))
        .collect();
    let before = tree_blobs(git, &base, &files)?;
    let changed = files
        .iter()
        .any(|(path, blob)| before.get(path.as_str()) != blob.as_ref());
    changed.then_some(TurnChange {
        trajectory,
        base,
        files,
    })
}

/// `Some(true)` once HEAD moved past the turn's base and holds exactly the turn's result;
/// `Some(false)` while it does not; `None` when git cannot tell.
pub fn committed(git: Git<'_>, change: &TurnChange) -> Option<bool> {
    let head = head(git)?;
    if head == change.base {
        return Some(false);
    }
    let now = tree_blobs(git, &head, &change.files)?;
    Some(
        change
            .files
            .iter()
            .all(|(path, blob)| now.get(path.as_str()) == blob.as_ref()),
    )
}

/// Blob ids of `files` in `revision`; files absent there are missing from the map.
fn tree_blobs(
    git: Git<'_>,
    revision: &str,
    files: &[(String, Option<String>)],
) -> Option<HashMap<String, String>> {
    let mut args = vec!["ls-tree", "-r", "-z", revision, "--"];
    args.extend(files.iter().map(|(path, _)| path.as_str()));
    let listing = git(&args)?;
    Some(
        listing
            .split('\0')
            .filter_map(|entry| {
                let (meta, path) = entry.split_once('\t')?;
                let mut meta = meta.split(' ');
                let (_mode, kind, id) = (meta.next()?, meta.next()?, meta.next()?);
                (kind == "blob").then(|| (path.to_owned(), id.to_owned()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, process::Command};

    fn git_in(root: &Path) -> impl Fn(&[&str]) -> Option<String> + '_ {
        move |args| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8(output.stdout).ok())?
        }
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        {
            let git = git_in(dir.path());
            for args in [
                &["init", "-q"][..],
                &["config", "user.email", "test@example.com"],
                &["config", "user.name", "Test"],
                &["config", "commit.gpgsign", "false"],
            ] {
                git(args).unwrap();
            }
        }
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        commit_all(dir.path());
        dir
    }

    fn commit_all(root: &Path) {
        let git = git_in(root);
        git(&["add", "-A"]).unwrap();
        git(&["commit", "-q", "-m", "work"]).unwrap();
    }

    #[test]
    fn committing_the_turns_exact_result_counts() {
        let dir = repo();
        let git = git_in(dir.path());
        let base = head(&git).unwrap();
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/new.rs"), "fn main() {}\n").unwrap();
        fs::remove_file(dir.path().join("gone.txt")).unwrap();
        let changed = ["a.txt", "src/new.rs", "gone.txt"].map(String::from);
        let change = capture(&git, dir.path(), 7, base, &changed).unwrap();
        assert_eq!(committed(&git, &change), Some(false), "not committed yet");
        commit_all(dir.path());
        assert_eq!(committed(&git, &change), Some(true));
        assert_eq!(change.trajectory, 7);
    }

    #[test]
    fn edits_before_the_commit_do_not_count() {
        let dir = repo();
        let git = git_in(dir.path());
        let base = head(&git).unwrap();
        fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let change = capture(&git, dir.path(), 1, base, &["a.txt".into()]).unwrap();
        fs::write(dir.path().join("a.txt"), "two, fixed by hand\n").unwrap();
        commit_all(dir.path());
        assert_eq!(committed(&git, &change), Some(false));
    }

    #[test]
    fn a_turn_that_leaves_files_as_committed_is_not_tracked() {
        let dir = repo();
        let git = git_in(dir.path());
        let base = head(&git).unwrap();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let changed = ["a.txt".to_owned(), "/elsewhere/b.txt".to_owned()];
        assert!(capture(&git, dir.path(), 1, base.clone(), &changed).is_none());
        assert!(capture(&git, dir.path(), 1, base, &[]).is_none());
    }

    #[test]
    fn outside_a_repository_nothing_is_tracked() {
        let dir = tempfile::tempdir().unwrap();
        assert!(head(&git_in(dir.path())).is_none());
    }
}
