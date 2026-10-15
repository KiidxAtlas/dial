//! Lock-file handling for split commits.
//!
//! The agent never sees lock files, so a plan built from its output would leave them staged and
//! unplanned. [`assign_lock_files_to_plan`] attaches each orphaned lock file to the commit whose
//! sibling manifest matches, falling back to the last commit.

use super::state::{FileChange, SplitCommitPlan};

/// Lock-file basename → ordered sibling manifests. The first manifest present in a group wins.
pub(crate) const LOCK_FILE_MANIFESTS: &[(&str, &[&str])] = &[
    ("Cargo.lock", &["Cargo.toml"]),
    ("package-lock.json", &["package.json"]),
    ("yarn.lock", &["package.json"]),
    ("pnpm-lock.yaml", &["package.json"]),
    ("bun.lock", &["package.json"]),
    ("bun.lockb", &["package.json"]),
    ("go.sum", &["go.mod"]),
    ("poetry.lock", &["pyproject.toml"]),
    ("Pipfile.lock", &["Pipfile"]),
    ("uv.lock", &["pyproject.toml"]),
    ("composer.lock", &["composer.json"]),
    ("Gemfile.lock", &["Gemfile"]),
    ("flake.lock", &["flake.nix"]),
    ("pubspec.lock", &["pubspec.yaml"]),
    ("Podfile.lock", &["Podfile"]),
    ("mix.lock", &["mix.exs"]),
    (
        "gradle.lockfile",
        &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
    ),
];

/// True when the basename of `path` is a known lock file.
pub(crate) fn is_lock_file(path: &str) -> bool {
    let basename = basename(path);
    LOCK_FILE_MANIFESTS
        .iter()
        .any(|(name, _)| *name == basename)
}

/// Attach staged lock files missing from `plan` to a commit. No-op on empty plans.
pub(crate) fn assign_lock_files_to_plan(plan: &mut SplitCommitPlan, staged_files: &[String]) {
    if plan.commits.is_empty() {
        return;
    }
    let mut planned: Vec<String> = plan
        .commits
        .iter()
        .flat_map(|commit| commit.changes.iter().map(|change| change.path().to_owned()))
        .collect();
    let orphaned: Vec<&String> = staged_files
        .iter()
        .filter(|file| !planned.contains(file) && is_lock_file(file))
        .collect();
    for lock_file in orphaned {
        let basename = basename(lock_file);
        let dir = directory(lock_file);
        let manifests = manifests_for(basename);
        let target = find_manifest_commit_index(plan, dir, manifests);
        plan.commits[target].changes.push(FileChange::All {
            path: lock_file.clone(),
        });
        planned.push(lock_file.clone());
    }
}

fn manifests_for(basename: &str) -> &'static [&'static str] {
    LOCK_FILE_MANIFESTS
        .iter()
        .find(|(name, _)| *name == basename)
        .map(|(_, manifests)| *manifests)
        .unwrap_or(&[])
}

fn find_manifest_commit_index(plan: &SplitCommitPlan, lock_dir: &str, manifests: &[&str]) -> usize {
    // Same directory first: the strongest signal (workspace crate `Cargo.toml` beside `Cargo.lock`).
    for manifest in manifests {
        for (index, commit) in plan.commits.iter().enumerate() {
            for change in &commit.changes {
                if basename(change.path()) == *manifest && directory(change.path()) == lock_dir {
                    return index;
                }
            }
        }
    }
    // Any directory: a monorepo may lock at the root while the manifest sits in a subpath.
    for manifest in manifests {
        for (index, commit) in plan.commits.iter().enumerate() {
            for change in &commit.changes {
                if basename(change.path()) == *manifest {
                    return index;
                }
            }
        }
    }
    plan.commits.len() - 1
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::super::state::{SplitCommitGroup, SplitCommitPlan};
    use super::*;

    fn commit(paths: &[&str]) -> SplitCommitGroup {
        SplitCommitGroup {
            changes: paths
                .iter()
                .map(|path| FileChange::All {
                    path: (*path).to_owned(),
                })
                .collect(),
            commit_type: "chore".to_owned(),
            scope: None,
            summary: "Updated deps".to_owned(),
            details: Vec::new(),
            issue_refs: Vec::new(),
            rationale: None,
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn lock_file_joins_manifest_commit_in_same_directory() {
        let mut plan = SplitCommitPlan {
            commits: vec![commit(&["src/a.rs"]), commit(&["crates/x/Cargo.toml"])],
            warnings: Vec::new(),
        };
        let staged = vec![
            "src/a.rs".to_owned(),
            "crates/x/Cargo.toml".to_owned(),
            "crates/x/Cargo.lock".to_owned(),
        ];
        assign_lock_files_to_plan(&mut plan, &staged);
        assert!(
            plan.commits[1]
                .changes
                .iter()
                .any(|c| c.path() == "crates/x/Cargo.lock")
        );
    }

    #[test]
    fn orphan_lock_file_falls_back_to_last_commit() {
        let mut plan = SplitCommitPlan {
            commits: vec![commit(&["a.rs"]), commit(&["b.rs"])],
            warnings: Vec::new(),
        };
        let staged = vec!["a.rs".to_owned(), "b.rs".to_owned(), "yarn.lock".to_owned()];
        assign_lock_files_to_plan(&mut plan, &staged);
        assert!(
            plan.commits[1]
                .changes
                .iter()
                .any(|c| c.path() == "yarn.lock")
        );
    }

    #[test]
    fn non_lock_files_are_ignored() {
        let mut plan = SplitCommitPlan {
            commits: vec![commit(&["a.rs"])],
            warnings: Vec::new(),
        };
        assign_lock_files_to_plan(&mut plan, &["a.rs".to_owned(), "other.txt".to_owned()]);
        assert_eq!(plan.commits[0].changes.len(), 1);
    }
}
