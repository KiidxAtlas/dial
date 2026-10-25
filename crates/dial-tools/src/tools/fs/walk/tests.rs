use super::*;
use crate::tools::testkit::workspace;
use std::fs;

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
    let (dir, ws) = workspace();
    fs::create_dir_all(dir.path().join(".git/info")).unwrap();
    fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    fs::write(dir.path().join(".git/info/exclude"), "excluded.rs\n").unwrap();
    fs::write(dir.path().join(".gitignore"), "*.tmp\nparent.rs\n").unwrap();
    fs::write(dir.path().join("src/.gitignore"), "!keep.tmp\n").unwrap();
    fs::write(dir.path().join("src/.ignore"), "visible.rs\n").unwrap();
    for name in [
        "a.tmp",
        "keep.tmp",
        "parent.rs",
        "excluded.rs",
        "visible.rs",
        ".hidden",
        "nested/z.rs",
    ] {
        fs::write(dir.path().join("src").join(name), "").unwrap();
    }
    let start = dir.path().join("src");
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
    let (dir, ws) = workspace();
    for (name, marker) in [
        ("linked", "gitdir: /repo/.git/worktrees/feature"),
        ("module", "gitdir: ../.git/modules/lib"),
    ] {
        fs::create_dir(dir.path().join(name)).unwrap();
        fs::write(dir.path().join(name).join(".git"), marker).unwrap();
        fs::write(dir.path().join(name).join("a.rs"), "").unwrap();
    }
    fs::create_dir(dir.path().join("target")).unwrap();
    fs::write(dir.path().join("target/output.rs"), "").unwrap();
    let mut walk = Walk::new(&ws);
    let actual = files(&mut walk, dir.path());
    assert!(actual.contains(&"module/a.rs".into()));
    assert!(
        !actual
            .iter()
            .any(|p| p.starts_with("linked/") || p.starts_with("target/"))
    );
    assert!(!walk.truncated);
    assert_eq!(
        files(&mut Walk::new(&ws), &dir.path().join("target")),
        ["output.rs"]
    );
}

#[test]
fn oversized_directory_is_omitted_before_sorting_and_marks_legacy_incomplete() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("large")).unwrap();
    for name in ["z", "b", "a", "c"] {
        fs::write(dir.path().join("large").join(name), "").unwrap();
    }
    fs::write(dir.path().join("small"), "").unwrap();
    let mut consumed = 0;
    assert!(matches!(
        bounded_directory(&dir.path().join("large"), 2, &mut consumed),
        Err(DirectoryError::Oversized)
    ));
    assert_eq!(consumed, 3, "stop at limit + 1, not directory exhaustion");
    let mut walk = Walk::new(&ws);
    walk.directory_limit = 2;
    assert_eq!(files(&mut walk, dir.path()), ["small"]);
    assert!(
        walk.truncated,
        "codebase callers must not publish a complete index"
    );
    let admitted = bounded_directory(&dir.path().join("large"), 4, &mut 0).unwrap();
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
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "").unwrap();
    symlink(outside.path(), dir.path().join("directory")).unwrap();
    symlink(outside.path().join("secret"), dir.path().join("file")).unwrap();
    fs::write(dir.path().join("real"), "").unwrap();
    assert_eq!(files(&mut Walk::new(&ws), dir.path()), ["real"]);
    let mut found = Vec::new();
    let mut search = SearchWalk::new(&ws, true, true, None);
    search.run(dir.path(), &mut |path| {
        found.push(path.to_path_buf());
        true
    });
    assert_eq!(found, [dir.path().join("real")]);
    assert!(search.is_complete());
}

#[test]
fn ignored_oversized_subtree_is_not_enumerated_and_prefix_keeps_ancestor_rules() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("ignored")).unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
    for i in 0..5 {
        fs::write(dir.path().join(format!("ignored/{i}")), "").unwrap();
    }
    let mut walk = Walk::new(&ws);
    walk.directory_limit = 2;
    let mut found = Vec::new();
    walk.run_with_prefix(dir.path(), &dir.path().join("ignored"), &mut |path| {
        found.push(path.to_path_buf());
        true
    });
    assert!(
        !found
            .iter()
            .any(|path| path.starts_with(dir.path().join("ignored")))
    );
    assert!(!walk.truncated);
    assert_eq!(walk.directory_entries, 2);
}
