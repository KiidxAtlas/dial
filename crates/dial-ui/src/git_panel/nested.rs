//! Repositories nested inside a project that is not itself a repository (a folder of clones).
//! The search is bounded and never follows links; each candidate is confirmed by Git.
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

/// Folders never searched: dependency and build output, which hold no project repositories.
const SKIP: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "out"];
/// Folders visited at most; the usual monorepo layout fits well within this.
const MAX_FOLDERS: usize = 2_000;
/// Levels below the project searched.
const MAX_DEPTH: usize = 6;

/// Repository roots under `root` (never `root` itself), sorted. A repository's own folders are
/// not searched further: its nested checkouts are that repository's concern.
pub(super) fn repositories(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = 0;
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type` does not follow links, so a link is never entered.
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            visited += 1;
            if visited > MAX_FOLDERS {
                break 'walk;
            }
            let path = entry.path();
            if path.join(".git").exists() {
                if confirmed_top(&path) {
                    found.push(path);
                }
                continue;
            }
            if depth + 1 < MAX_DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    found.sort();
    found
}

/// Whether Git says `dir` is the top of its own work tree (a gitfile may point elsewhere).
fn confirmed_top(dir: &Path) -> bool {
    let Ok(top) =
        crate::tools::vcs::git::run_checked(dir, &["rev-parse", "--show-toplevel"], false)
    else {
        return false;
    };
    let top = PathBuf::from(top.trim_end_matches(['\n', '\r']));
    matches!((top.canonicalize(), dir.canonicalize()), (Ok(a), Ok(b)) if a == b)
}
