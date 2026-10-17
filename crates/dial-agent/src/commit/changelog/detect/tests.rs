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
