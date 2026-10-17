use super::*;

#[test]
fn split_file_diffs_starts_a_chunk_per_file() {
    let raw = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-p\n+q\n";
    let chunks = split_file_diffs(raw);
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].starts_with("diff --git a/a.txt"));
    assert!(chunks[1].starts_with("diff --git a/b.txt"));
    assert!(chunks[1].ends_with("+q\n"));
}

#[test]
fn diff_path_reads_the_new_side() {
    let head = vec!["diff --git a/src/x.rs b/src/x.rs".to_owned()];
    assert_eq!(diff_path(&head).as_deref(), Some("src/x.rs"));
    assert_eq!(diff_path(&[]), None);
}

#[test]
fn file_patch_keeps_header_and_selected_hunks_in_order() {
    let doc = Doc {
        head: vec![
            "diff --git a/f b/f".to_owned(),
            "--- a/f".to_owned(),
            "+++ b/f".to_owned(),
        ],
        hunks: vec![
            Hunk {
                header: "@@ -1 +1 @@".to_owned(),
                lines: vec!["-a".to_owned(), "+b".to_owned()],
            },
            Hunk {
                header: "@@ -9 +9 @@".to_owned(),
                lines: vec!["-c".to_owned(), "+d".to_owned()],
            },
        ],
        ..Doc::default()
    };
    assert_eq!(
        file_patch(&doc, &[1]),
        "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -9 +9 @@\n-c\n+d\n"
    );
}

#[test]
fn ai_summary_reports_counts_and_no_match() {
    let none = AiSummary {
        total_files: 3,
        ..AiSummary::default()
    };
    assert_eq!(none.message(), "AI staging matched no changes");
    let some = AiSummary {
        total_files: 3,
        matched_files: 2,
        total_hunks: 5,
        staged_hunks: 2,
        whole_files: 1,
    };
    assert_eq!(
        some.message(),
        "AI staged 2 of 5 hunks and 1 whole files (2 of 3 changed files matched)"
    );
}

#[test]
fn generated_text_round_trips_through_validation_flag() {
    let generated = Generated {
        text: "fix: x".to_owned(),
        validation_error: Some("bad scope".to_owned()),
        staged_all: false,
    };
    assert_eq!(generated.text, "fix: x");
    assert_eq!(generated.validation_error.as_deref(), Some("bad scope"));
    assert!(!generated.staged_all);
}
