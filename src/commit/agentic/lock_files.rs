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
mod tests;
