//! Isolated working copies for `task` workers (`isolation = "copy"`).
//!
//! The worker runs against a private copy of the project (the same copier benchmarks use, so dirty
//! and untracked files are included and secrets, `.git`, `.dial` and build caches are not). When it
//! finishes, only the files it changed are merged back, each one only if the project's version is
//! still the one the copy started from. Files the user or another worker changed in the meantime are
//! reported as conflicts and never overwritten; the copy is then kept so nothing is lost. Merges go
//! through the project's checkpoint observer, so `/undo` reverts them like any other edit.
use super::solve::{FileState, Tree, confined_destination, install, read_state};
use crate::tools::Workspace;
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const MAX_FILES: usize = 20_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;

pub struct Isolated {
    dir: tempfile::TempDir,
    root: PathBuf,
    before: Tree,
    workspace: Workspace,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merge {
    pub applied: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
    /// Where the full copy was kept because of conflicts.
    pub kept: Option<PathBuf>,
}

impl Merge {
    pub fn describe(&self) -> String {
        let list = |paths: &[PathBuf]| {
            paths
                .iter()
                .take(20)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = if self.applied.is_empty() {
            "isolated copy: no files were changed".to_owned()
        } else {
            format!(
                "isolated copy: merged {} file(s): {}",
                self.applied.len(),
                list(&self.applied)
            )
        };
        if !self.conflicts.is_empty() {
            text.push_str(&format!("\nCONFLICT: {} file(s) changed in the project while the worker ran and were NOT overwritten: {}", self.conflicts.len(), list(&self.conflicts)));
            if let Some(kept) = &self.kept {
                text.push_str(&format!(
                    "\nThe worker's full copy is kept at {} for manual merging.",
                    kept.display()
                ));
            }
        }
        text
    }
}

/// Contents of every non-excluded regular file under `root`, relative to it. Symlinks are skipped.
fn snapshot(root: &Path) -> Result<Tree> {
    fn walk(root: &Path, dir: &Path, out: &mut Tree, remaining: &mut usize) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if crate::benchmark::is_excluded_source_path(root, &path)
                || path
                    .file_name()
                    .is_some_and(|n| n == crate::tools::SCRATCH_DIR)
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                walk(root, &path, out, remaining)?;
            } else if meta.is_file() {
                ensure!(
                    out.len() < MAX_FILES && meta.len() <= *remaining as u64,
                    "project is too large to isolate (limit {MAX_FILES} files, {} MiB)",
                    MAX_BYTES >> 20
                );
                let bytes = fs::read(&path)?;
                *remaining -= bytes.len();
                out.insert(
                    path.strip_prefix(root)?.to_path_buf(),
                    FileState {
                        bytes,
                        mode: meta.permissions().mode() & 0o777,
                    },
                );
            }
        }
        Ok(())
    }
    let mut out = Tree::new();
    let mut remaining = MAX_BYTES;
    walk(root, root, &mut out, &mut remaining)?;
    Ok(out)
}

impl Isolated {
    pub fn create(project: &Workspace) -> Result<Self> {
        let dir = crate::benchmark::clone_project(project.root())?;
        let root = dir.path().canonicalize()?;
        let before = snapshot(&root)?;
        let workspace = project.rebased(&root)?;
        Ok(Self {
            dir,
            root,
            before,
            workspace,
        })
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Merge the copy's changes into `project`. Never overwrites a file that changed there.
    pub fn merge(self, project: &Workspace) -> Result<Merge> {
        let after = snapshot(&self.root)?;
        let changed: BTreeSet<&PathBuf> = self
            .before
            .keys()
            .chain(after.keys())
            .filter(|p| self.before.get(*p) != after.get(*p))
            .collect();
        let mut merge = Merge::default();
        let _serial = project.serial_operation();
        for relative in changed {
            let result = (|| -> Result<bool> {
                let path = confined_destination(project.root(), relative)?;
                let current = read_state(&path)?;
                if current.as_ref() == after.get(relative) {
                    return Ok(true);
                }
                if current.as_ref() != self.before.get(relative) {
                    return Ok(false);
                }
                if after.contains_key(relative) {
                    project.observe_write(&path)
                } else {
                    project.observe_delete(&path)
                }
                .map_err(anyhow::Error::msg)?;
                install(&path, after.get(relative))?;
                Ok(true)
            })();
            match result {
                Ok(true) => merge.applied.push(relative.clone()),
                Ok(false) | Err(_) => merge.conflicts.push(relative.clone()),
            }
        }
        if !merge.conflicts.is_empty() {
            merge.kept = Some(self.dir.keep());
        }
        Ok(merge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a1").unwrap();
        fs::write(dir.path().join("b.txt"), "b1").unwrap();
        fs::write(dir.path().join("gone.txt"), "x").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn changes_merge_back_and_the_copy_is_discarded() {
        let (dir, ws) = project();
        let iso = Isolated::create(&ws).unwrap();
        assert_ne!(iso.root(), ws.root());
        fs::write(iso.root().join("a.txt"), "a2").unwrap();
        fs::write(iso.root().join("new.txt"), "n").unwrap();
        fs::remove_file(iso.root().join("gone.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a1",
            "project untouched while the worker runs"
        );
        let root = iso.root().to_path_buf();
        let merge = iso.merge(&ws).unwrap();
        assert_eq!(merge.applied.len(), 3);
        assert!(merge.conflicts.is_empty() && merge.kept.is_none());
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a2");
        assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!root.exists(), "copy removed");
    }

    #[test]
    fn concurrent_project_edits_are_never_overwritten() {
        let (dir, ws) = project();
        let iso = Isolated::create(&ws).unwrap();
        fs::write(iso.root().join("a.txt"), "worker").unwrap();
        fs::write(iso.root().join("b.txt"), "worker").unwrap();
        fs::write(dir.path().join("a.txt"), "user").unwrap();
        let merge = iso.merge(&ws).unwrap();
        assert_eq!(merge.applied, vec![PathBuf::from("b.txt")]);
        assert_eq!(merge.conflicts, vec![PathBuf::from("a.txt")]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "user"
        );
        let kept = merge.kept.clone().expect("copy kept for manual merge");
        assert_eq!(fs::read_to_string(kept.join("a.txt")).unwrap(), "worker");
        assert!(merge.describe().contains("CONFLICT"));
        fs::remove_dir_all(kept).unwrap();
    }

    #[test]
    fn two_workers_on_disjoint_files_both_land_and_identical_edits_are_not_conflicts() {
        let (dir, ws) = project();
        let (one, two) = (
            Isolated::create(&ws).unwrap(),
            Isolated::create(&ws).unwrap(),
        );
        fs::write(one.root().join("a.txt"), "one").unwrap();
        fs::write(two.root().join("b.txt"), "two").unwrap();
        fs::write(two.root().join("a.txt"), "one").unwrap();
        assert!(one.merge(&ws).unwrap().conflicts.is_empty());
        let merge = two.merge(&ws).unwrap();
        assert!(merge.conflicts.is_empty(), "{merge:?}");
        assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "two");
    }

    #[test]
    fn build_output_and_secrets_in_the_copy_never_merge() {
        let (dir, ws) = project();
        fs::write(dir.path().join(".env"), "SECRET").unwrap();
        let iso = Isolated::create(&ws).unwrap();
        assert!(!iso.root().join(".env").exists(), "secrets are not copied");
        fs::create_dir_all(iso.root().join("target")).unwrap();
        fs::write(iso.root().join("target/out.bin"), "junk").unwrap();
        fs::write(iso.root().join(".env"), "PLANTED").unwrap();
        let merge = iso.merge(&ws).unwrap();
        assert!(
            merge.applied.is_empty() && merge.conflicts.is_empty(),
            "{merge:?}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET"
        );
    }
}
