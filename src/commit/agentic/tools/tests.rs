use super::*;
#[test]
fn added_and_deleted_files_are_whole_file_only() {
    let diff = "diff --git a/new.rs b/new.rs\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+a\ndiff --git a/mod.rs b/mod.rs\nindex 1..2 100644\n--- a/mod.rs\n+++ b/mod.rs\n@@ -1 +1 @@\n-a\n+b\n";
    assert!(whole_file_only(diff, "new.rs"));
    assert!(!whole_file_only(diff, "mod.rs"));
}

#[test]
fn subject_parses_type_scope_and_summary() {
    let parsed = parse_subject("feat(core): add thing");
    assert_eq!(parsed.commit_type.as_deref(), Some("feat"));
    assert_eq!(parsed.scope.as_deref(), Some("core"));
    assert_eq!(parsed.summary, "add thing");

    let plain = parse_subject("Merge branch main");
    assert_eq!(plain.commit_type, None);
    assert_eq!(plain.summary, "Merge branch main");
}

#[test]
fn priority_ranks_source_above_docs_and_binaries() {
    assert!(file_priority("src/lib.rs") > file_priority("README.md"));
    assert!(file_priority("src/lib.rs") > file_priority("tests/lib_test.rs"));
    assert!(file_priority("logo.png") < 0);
    assert_eq!(file_priority("Cargo.toml"), 70);
}

#[test]
fn file_type_labels_match_extension_rules() {
    assert_eq!(file_type("docs/guide.md"), "documentation");
    assert_eq!(file_type("Cargo.toml"), "configuration");
    assert_eq!(file_type("src/lib.rs"), "implementation");
    assert_eq!(file_type("run.sh"), "script");
}

#[test]
fn long_diffs_are_elided_in_the_middle() {
    let content: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    let truncated = truncate_lines(&content);
    assert!(truncated.starts_with("line 1\n"));
    assert!(truncated.contains("[…15ln elided…]"));
    assert!(truncated.ends_with("line 40"));
}

#[test]
fn numstat_rows_parse_binary_as_none() {
    let rows = parse_numstat("3\t1\tsrc/a.rs\0-\t-\tlogo.png\0");
    assert_eq!(rows[0].additions, Some(3));
    assert_eq!(rows[1].additions, None);
    assert_eq!(rows[1].path, "logo.png");
}

#[test]
fn details_reject_unknown_category() {
    let args = json!({"details": [{"text": "Did a thing.", "changelog_category": "Nope"}]});
    let (details, errors) = parse_details(&args);
    assert_eq!(details.len(), 1);
    assert_eq!(errors, vec!["Unknown changelog category: Nope".to_owned()]);
}

#[test]
fn category_map_drops_unknown_and_trims_deletions() {
    let mut errors = Vec::new();
    let value = json!({"Fixed": ["  crash  "], "Bogus": ["x"]});
    let deletions = category_map(Some(&value), "CHANGELOG.md", true, &mut errors);
    assert_eq!(
        deletions,
        vec![("Fixed".to_owned(), vec!["crash".to_owned()])]
    );
    assert_eq!(
        errors,
        vec!["Unknown deletion category for CHANGELOG.md: Bogus".to_owned()]
    );
}

#[test]
fn shared_file_needs_disjoint_hunks() {
    let disjoint = vec![
        (
            0,
            FileChange::Indices {
                path: "a.rs".into(),
                indices: vec![1],
            },
        ),
        (
            1,
            FileChange::Indices {
                path: "a.rs".into(),
                indices: vec![2],
            },
        ),
    ];
    assert!(can_share_file(&disjoint));
    let overlapping = vec![
        (
            0,
            FileChange::Indices {
                path: "a.rs".into(),
                indices: vec![1],
            },
        ),
        (
            1,
            FileChange::Indices {
                path: "a.rs".into(),
                indices: vec![1],
            },
        ),
    ];
    assert!(!can_share_file(&overlapping));
    let mixed = vec![
        (
            0,
            FileChange::All {
                path: "a.rs".into(),
            },
        ),
        (
            1,
            FileChange::Indices {
                path: "a.rs".into(),
                indices: vec![1],
            },
        ),
    ];
    assert!(!can_share_file(&mixed));
}

#[test]
fn dependencies_reject_self_and_out_of_range() {
    let raw = json!({"dependencies": [0, 5, "x"]});
    let mut errors = Vec::new();
    let kept = parse_dependencies(&raw, 0, 2, "Commit 1: ", &mut errors);
    assert!(kept.is_empty());
    assert_eq!(
        errors,
        vec![
            "Commit 1: cannot depend on itself".to_owned(),
            "Commit 1: dependency index out of range (5)".to_owned(),
            "Commit 1: dependency index must be an integer".to_owned(),
        ]
    );
}
