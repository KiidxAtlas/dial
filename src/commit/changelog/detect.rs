//! Maps staged files to the nearest `CHANGELOG.md` above each file.

use std::collections::BTreeMap;
use std::path::Path;

/// A changelog file and the staged files it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangelogTarget {
    /// Changelog path relative to the repository root, `/`-separated.
    pub path: String,
    /// Staged files governed by this changelog, relative to the repository root.
    pub files: Vec<String>,
}

/// Nearest `CHANGELOG.md` at or above the directory of `file`, checking up to the repository root.
fn find_changelog(cwd: &Path, file: &str) -> Option<String> {
    let mut dir = Path::new(file).parent().unwrap_or(Path::new(""));
    loop {
        let candidate = dir.join("CHANGELOG.md");
        if cwd.join(&candidate).is_file() {
            return Some(candidate.to_string_lossy().replace('\\', "/"));
        }
        if dir.as_os_str().is_empty() {
            return None;
        }
        dir = dir.parent().unwrap_or(Path::new(""));
    }
}

/// Group staged files by the changelog that covers them. Staged changelogs themselves are never targets.
pub(crate) fn detect_targets(cwd: &Path, staged: &[String]) -> Vec<ChangelogTarget> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in staged {
        let basename = file.rsplit('/').next().unwrap_or(file);
        if basename.to_ascii_lowercase().ends_with("changelog.md") {
            continue;
        }
        if let Some(path) = find_changelog(cwd, file) {
            grouped.entry(path).or_default().push(file.clone());
        }
    }
    grouped
        .into_iter()
        .map(|(path, files)| ChangelogTarget { path, files })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_files_under_nearest_changelog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("packages/a/src")).expect("mkdir");
        std::fs::write(root.join("CHANGELOG.md"), "# Changelog\n").expect("write root");
        std::fs::write(root.join("packages/a/CHANGELOG.md"), "# Changelog\n").expect("write pkg");
        let staged = vec![
            "packages/a/src/lib.rs".to_owned(),
            "src/main.rs".to_owned(),
            "packages/a/CHANGELOG.md".to_owned(),
        ];
        let targets = detect_targets(root, &staged);
        assert_eq!(
            targets,
            vec![
                ChangelogTarget {
                    path: "CHANGELOG.md".to_owned(),
                    files: vec!["src/main.rs".to_owned()],
                },
                ChangelogTarget {
                    path: "packages/a/CHANGELOG.md".to_owned(),
                    files: vec!["packages/a/src/lib.rs".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn files_without_any_changelog_are_not_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = vec!["notes/todo.txt".to_owned()];
        assert!(detect_targets(dir.path(), &staged).is_empty());
    }
}
