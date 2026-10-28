use super::*;
use crate::tools::testkit;
use std::fs;
use std::path::PathBuf;

/// `Workspace` stores its canonical root (`/private/var/...` on macOS), while `TempDir` may hand
/// out the `/var` alias. Production callers always pass canonical starts, so fixtures do too.
fn workspace() -> (tempfile::TempDir, PathBuf, Workspace) {
    let (dir, ws) = testkit::workspace();
    let root = dir.path().canonicalize().unwrap();
    (dir, root, ws)
}

fn files(walk: &mut Walk<'_>, start: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk.run(start, &mut |path| {
        found.push(
            path.strip_prefix(start)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        true
    });
    found
}

#[test]
fn legacy_ignore_matches_builder_including_parents_exclude_and_negation() {
    let (_dir, root, ws) = workspace();
    fs::create_dir_all(root.join(".git/info")).unwrap();
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join(".git/info/exclude"), "excluded.rs\n").unwrap();
    fs::write(root.join(".gitignore"), "*.tmp\nparent.rs\n").unwrap();
    fs::write(root.join("src/.gitignore"), "!keep.tmp\n").unwrap();
    fs::write(root.join("src/.ignore"), "visible.rs\n").unwrap();
    for name in [
        "a.tmp",
        "keep.tmp",
        "parent.rs",
        "excluded.rs",
        "visible.rs",
        ".hidden",
        "nested/z.rs",
    ] {
        fs::write(root.join("src").join(name), "").unwrap();
    }
    let start = root.join("src");
    let expected: Vec<_> = WalkBuilder::new(&start)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .git_global(false)
        .ignore(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| {
            entry
                .path()
                .strip_prefix(&start)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let mut walk = Walk::new(&ws);
    let actual = files(&mut walk, &start);
    assert_eq!(actual, expected);
    assert!(actual.contains(&"keep.tmp".into()));
    assert!(actual.contains(&"visible.rs".into()));
    assert!(!actual.contains(&"excluded.rs".into()));
    assert!(!walk.truncated);
}

#[test]
fn policy_prunes_linked_worktrees_but_not_submodules_and_explicit_skipped_roots() {
    let (_dir, root, ws) = workspace();
    for (name, marker) in [
        ("linked", "gitdir: /repo/.git/worktrees/feature"),
        ("module", "gitdir: ../.git/modules/lib"),
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join(".git"), marker).unwrap();
        fs::write(root.join(name).join("a.rs"), "").unwrap();
    }
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("target/output.rs"), "").unwrap();
    let mut walk = Walk::new(&ws);
    let actual = files(&mut walk, &root);
    assert!(actual.contains(&"module/a.rs".into()));
    assert!(
        !actual
            .iter()
            .any(|p| p.starts_with("linked/") || p.starts_with("target/"))
    );
    assert!(!walk.truncated);
    assert_eq!(
        files(&mut Walk::new(&ws), &root.join("target")),
        ["output.rs"]
    );
}

#[test]
fn oversized_directory_is_omitted_before_sorting_and_marks_legacy_incomplete() {
    let (_dir, root, ws) = workspace();
    fs::create_dir(root.join("large")).unwrap();
    for name in ["z", "b", "a", "c"] {
        fs::write(root.join("large").join(name), "").unwrap();
    }
    fs::write(root.join("small"), "").unwrap();
    let mut consumed = 0;
    assert!(matches!(
        bounded_directory(&root.join("large"), 2, &mut consumed),
        Err(DirectoryError::Oversized)
    ));
    assert_eq!(consumed, 3, "stop at limit + 1, not directory exhaustion");
    let mut walk = Walk::new(&ws);
    walk.directory_limit = 2;
    assert_eq!(files(&mut walk, &root), ["small"]);
    assert!(
        walk.truncated,
        "codebase callers must not publish a complete index"
    );
    let admitted = bounded_directory(&root.join("large"), 4, &mut 0).unwrap();
    assert_eq!(
        admitted
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["a", "b", "c", "z"]
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed_or_reported_by_either_walker() {
    use std::os::unix::fs::symlink;
    let (_dir, root, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "").unwrap();
    symlink(outside.path(), root.join("directory")).unwrap();
    symlink(outside.path().join("secret"), root.join("file")).unwrap();
    fs::write(root.join("real"), "").unwrap();
    assert_eq!(files(&mut Walk::new(&ws), &root), ["real"]);
    let mut found = Vec::new();
    let mut search = SearchWalk::new(&ws, true, true, None);
    search.run(&root, &mut |path| {
        found.push(path.to_path_buf());
        true
    });
    assert_eq!(found, [root.join("real")]);
    assert!(search.is_complete());
}

#[test]
fn ignored_oversized_subtree_is_not_enumerated_and_prefix_keeps_ancestor_rules() {
    let (_dir, root, ws) = workspace();
    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    for i in 0..5 {
        fs::write(root.join(format!("ignored/{i}")), "").unwrap();
    }
    let mut walk = Walk::new(&ws);
    walk.directory_limit = 2;
    let mut found = Vec::new();
    walk.run_with_prefix(&root, &root.join("ignored"), &mut |path| {
        found.push(path.to_path_buf());
        true
    });
    assert!(
        !found
            .iter()
            .any(|path| path.starts_with(root.join("ignored")))
    );
    assert!(!walk.truncated);
    assert_eq!(walk.directory_entries, 2);
}
