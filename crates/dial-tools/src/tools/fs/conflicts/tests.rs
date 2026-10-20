use super::*;
use crate::tools::testkit::*;
use serde_json::json;
use std::fs;
use std::process::Command;

const TWO_HUNKS: &str = "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature\nb\n<<<<<<< ours\nx\n||||||| base\ny\n=======\n>>>>>>> theirs\nc\n";

#[test]
fn hunks_record_each_side_including_diff3_base_and_empty_sides() {
    assert_eq!(
        hunks(TWO_HUNKS).unwrap(),
        [
            Hunk {
                span: 2..=6,
                ours_label: "HEAD".into(),
                ours: Some(3..=3),
                base: None,
                theirs_label: "feature".into(),
                theirs: Some(5..=5),
            },
            Hunk {
                span: 8..=13,
                ours_label: "ours".into(),
                ours: Some(9..=9),
                base: Some(Some(11..=11)),
                theirs_label: "theirs".into(),
                theirs: None,
            },
        ]
    );
    // Markers need exactly seven characters; longer rules are content.
    assert!(hunks("<<<<<<<< not\n========\n").unwrap().is_empty());
}

#[test]
fn malformed_markers_are_reported_not_guessed() {
    assert!(
        hunks("<<<<<<< a\nx\n=======\ny\n")
            .unwrap_err()
            .contains("line 1 is never closed")
    );
    assert!(
        hunks("<<<<<<< a\n<<<<<<< b\n")
            .unwrap_err()
            .contains("nested conflict marker at line 2")
    );
}

#[test]
fn conflict_read_summarizes_hunks_and_anchors_only_their_lines() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("m.txt"), TWO_HUNKS).unwrap();
    let out = run(&ws, "read", json!({"path": "conflict://m.txt"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.starts_with(
            "[conflict://m.txt: 2 conflict hunks]\n\
             1. lines 2-6: ours (HEAD) 3-3; theirs (feature) 5-5\n\
             2. lines 8-13: ours (ours) 9-9; base 11-11; theirs (theirs) empty\n"
        ),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("<<<<<<< HEAD") && out.text.contains("[lines 2-6, 8-13 of 14]"),
        "{}",
        out.text
    );

    fs::write(dir.path().join("clean.txt"), "fine\n").unwrap();
    let clean = run(&ws, "read", json!({"path": "conflict://clean.txt"}));
    assert_eq!(clean.text, "No conflict markers in clean.txt.");
    assert!(run(&ws, "read", json!({"path": "conflict://m.txt", "limit": 5})).is_error);
}

#[test]
fn bare_conflict_lists_unmerged_git_paths() {
    let (dir, ws) = workspace();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(status.status.success() || args[0] == "merge", "{status:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("f.txt"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    git(&["checkout", "-qb", "other"]);
    fs::write(dir.path().join("f.txt"), "other\n").unwrap();
    git(&["commit", "-qam", "other"]);
    git(&["checkout", "-q", "main"]);
    fs::write(dir.path().join("f.txt"), "main\n").unwrap();
    git(&["commit", "-qam", "main"]);
    git(&["merge", "-q", "other"]);
    let out = run(&ws, "read", json!({"path": "conflict://"}));
    assert_eq!(
        out.text, "1 unmerged path:\nconflict://f.txt: 1 hunk",
        "{}",
        out.text
    );
}
