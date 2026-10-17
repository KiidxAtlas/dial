use std::sync::atomic::AtomicBool;

use serde_json::json;

use super::super::gh_common::strings;
use super::super::gh_common::test_support::FakeGh;
use super::*;

/// Five files: modified, added, deleted, renamed, binary.
const CANNED: &str = "diff --git a/src/lib.rs b/src/lib.rs\nindex 111..222 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,3 @@\n fn a() {}\n-fn b() {}\n+fn b() -> u8 { 1 }\n+fn c() {}\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\ndiff --git a/old.txt b/old.txt\ndeleted file mode 100644\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\ndiff --git a/from.txt b/to.txt\nsimilarity index 90%\nrename from from.txt\nrename to to.txt\n--- a/from.txt\n+++ b/to.txt\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n";

const SUMMARY: &str = "# Pull Request Diff: acme/widgets#12 (5 files)\n\n1. src/lib.rs  +2 -1  [modified]\n   pr://acme/widgets/12/diff/1\n\n2. new.txt  +2 -0  [added]\n   pr://acme/widgets/12/diff/2\n\n3. old.txt  +0 -1  [deleted]\n   pr://acme/widgets/12/diff/3\n\n4. to.txt  +1 -1  [renamed]  (renamed from from.txt)\n   pr://acme/widgets/12/diff/4\n\n5. img.png  (binary)  [binary]\n   pr://acme/widgets/12/diff/5\n\n---\nRead all: `pr://acme/widgets/12/diff/all`. Each file is also available as `pr://acme/widgets/12/diff/<i>`.";

const NEW_TXT_SECTION: &str = "diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..333\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n";

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn parse_pr_url_accepts_every_form() {
    let target = |repo: Option<&str>, number, view| PrTarget {
        repo: repo.map(str::to_owned),
        number,
        view,
    };
    let cases = [
        ("pr://12", target(None, 12, PrDiffView::Summary)),
        ("pr://12/diff", target(None, 12, PrDiffView::Summary)),
        ("pr://12/diff/all", target(None, 12, PrDiffView::All)),
        ("pr://12/diff/3", target(None, 12, PrDiffView::File(3))),
        (
            "pr://acme/widgets/12",
            target(Some("acme/widgets"), 12, PrDiffView::Summary),
        ),
        (
            "pr://acme/widgets/12/diff",
            target(Some("acme/widgets"), 12, PrDiffView::Summary),
        ),
        (
            "pr://acme/widgets/12/diff/all",
            target(Some("acme/widgets"), 12, PrDiffView::All),
        ),
        (
            "pr://acme/widgets/12/diff/2",
            target(Some("acme/widgets"), 12, PrDiffView::File(2)),
        ),
        (
            "pr://ghe.example.com/acme/widgets/12/diff/2",
            target(
                Some("ghe.example.com/acme/widgets"),
                12,
                PrDiffView::File(2),
            ),
        ),
        (
            "pr://ghe.example.com/acme/widgets/12",
            target(
                Some("ghe.example.com/acme/widgets"),
                12,
                PrDiffView::Summary,
            ),
        ),
        (
            "pr://ghe/acme/widgets/12/diff/all",
            target(Some("ghe/acme/widgets"), 12, PrDiffView::All),
        ),
    ];
    for (url, expected) in cases {
        assert_eq!(parse_pr_url(url), Some(expected), "{url}");
    }
}

#[test]
fn parse_pr_url_rejects_invalid_forms() {
    let invalid = [
        "",
        "https://github.com/acme/widgets/pull/12",
        "issue://12",
        "pr://",
        "pr://0",
        "pr://0/diff",
        "pr://12/diff/0",
        "pr://12/diff/x",
        "pr://12/diff/all/x",
        "pr://12/diff/",
        "pr://12/diff/all?x=1",
        "pr://12/files",
        "pr://12/acme/widgets",
        "pr://acme",
        "pr://acme/widgets",
        "pr://acme/widgets/0",
        "pr://acme/widgets/abc",
        "pr://acme/widgets/12/files",
        "pr://acme//12",
        "pr://acme/widgets/12/diff/../all",
        "pr://-x/widgets/12",
        "pr://acme/wid gets/12",
        "pr://acme/widgets/12?comments=0",
        "pr://ghe.example.com/acme/widgets",
    ];
    for url in invalid {
        assert_eq!(parse_pr_url(url), None, "{url}");
    }
}

#[test]
fn parse_splits_and_numbers_sections() {
    let files = parse_unified(CANNED);
    let summary: Vec<(&str, usize, usize, ChangeType, Option<&str>)> = files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                f.additions,
                f.deletions,
                f.change,
                f.old_path.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("src/lib.rs", 2, 1, ChangeType::Modified, None),
            ("new.txt", 2, 0, ChangeType::Added, None),
            ("old.txt", 0, 1, ChangeType::Deleted, None),
            ("to.txt", 1, 1, ChangeType::Renamed, Some("from.txt")),
            ("img.png", 0, 0, ChangeType::Binary, None),
        ]
    );
    // Sections tile the text exactly, each starting at its `diff --git` line.
    assert_eq!(files[0].start, 0);
    assert_eq!(files.last().unwrap().end, CANNED.len());
    for pair in files.windows(2) {
        assert_eq!(pair[0].end, pair[1].start);
    }
    assert_eq!(&CANNED[files[1].start..files[1].end], NEW_TXT_SECTION);
}

#[test]
fn parse_empty_text_has_no_files() {
    assert!(parse_unified("").is_empty());
}

#[test]
fn parse_quoted_header_paths_are_decoded() {
    let text = "diff --git \"a/x y.txt\" \"b/x y.txt\"\n--- \"a/x y.txt\"\n+++ \"b/x y.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
    let files = parse_unified(text);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "x y.txt");
    assert_eq!((files[0].additions, files[0].deletions), (1, 1));
    assert_eq!(
        parse_escape(&"\\101".chars().collect::<Vec<_>>(), 0),
        (b"A".to_vec(), 4)
    );
}

#[test]
fn summary_renders_counts_and_links() {
    let files = parse_unified(CANNED);
    assert_eq!(render_summary("acme/widgets", 12, &files), SUMMARY);
    assert_eq!(
        render_summary("acme/widgets", 12, &[]),
        "# Pull Request Diff: acme/widgets#12 (0 files)\n\n_No file changes._\n\n---\nRead all: `pr://acme/widgets/12/diff/all`. Each file is also available as `pr://acme/widgets/12/diff/<i>`."
    );
}

#[test]
fn synthetic_sections_match_patch_shapes() {
    let added = json!({"filename": "a.txt", "status": "added", "additions": 1, "deletions": 0, "patch": "@@ -0,0 +1 @@\n+x"});
    assert_eq!(
        synthetic_section(&added).unwrap(),
        "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+x"
    );
    let renamed = json!({"filename": "new name.txt", "previous_filename": "old.txt", "status": "renamed", "additions": 0, "deletions": 0});
    assert_eq!(
        synthetic_section(&renamed).unwrap(),
        "diff --git a/old.txt \"b/new name.txt\"\nrename from old.txt\nrename to \"new name.txt\"\n* patch unavailable (binary or too large); additions 0, deletions 0"
    );
    assert_eq!(synthetic_section(&json!({"status": "added"})), None);
    assert_eq!(synthetic_path("a/", "tab\there"), "\"a/tab\\there\"");
}

#[test]
fn quoted_rename_paths_round_trip() {
    let renamed = json!({"filename": "dir/new\nname.txt", "previous_filename": "old \"q\".txt", "status": "renamed", "additions": 0, "deletions": 0});
    let files = parse_unified(&synthetic_section(&renamed).unwrap());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "dir/new\nname.txt");
    assert_eq!(files[0].old_path.as_deref(), Some("old \"q\".txt"));
    assert_eq!(files[0].change, ChangeType::Renamed);
}

#[test]
fn too_large_errors_are_detected() {
    assert!(is_too_large_error(
        "gh: HTTP 406: Sorry, the diff exceeded the maximum number of lines (20000)"
    ));
    assert!(is_too_large_error("{\"code\":\"too_large\"}"));
    assert!(!is_too_large_error("gh: HTTP 4060 unrelated"));
    assert!(!is_too_large_error("Could not resolve to a PullRequest"));
}

#[test]
fn run_summary_invokes_gh_pr_diff_and_renders_listing() {
    let fake = FakeGh::new(CANNED, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "pr_diff",
        &ws,
        &json!({"repo": "acme/widgets", "pr": 12}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!out.is_error);
    assert_eq!(out.text, SUMMARY);
    assert_eq!(
        fake.last_args(),
        strings(&[
            "pr",
            "diff",
            "12",
            "--color",
            "never",
            "--repo",
            "acme/widgets"
        ])
    );
}

#[test]
fn run_file_slice_from_url_is_verbatim_section() {
    let _fake = FakeGh::new(CANNED, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "pr_diff",
        &ws,
        &json!({"pr": "pr://acme/widgets/12/diff/2"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(out.text, NEW_TXT_SECTION);
}

#[test]
fn run_all_returns_full_diff_verbatim() {
    let _fake = FakeGh::new(CANNED, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "pr_diff",
        &ws,
        &json!({"repo": "acme/widgets", "pr": 12, "all": true}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(out.text, CANNED);
}

#[test]
fn run_file_out_of_range_is_an_error() {
    let _fake = FakeGh::new(CANNED, "", 0);
    let (_dir, ws) = workspace();
    let err = run(
        "pr_diff",
        &ws,
        &json!({"repo": "acme/widgets", "pr": 12, "file": 9}),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(
        err,
        "pr://acme/widgets/12/diff/9 is out of range; PR has 5 files. Use pr://acme/widgets/12/diff to list available indices."
    );
}

#[test]
fn run_file_zero_is_out_of_range() {
    let _fake = FakeGh::new(CANNED, "", 0);
    let (_dir, ws) = workspace();
    let err = run(
        "pr_diff",
        &ws,
        &json!({"repo": "acme/widgets", "pr": 12, "file": 0}),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(err.contains("diff/0 is out of range"), "{err}");
}

#[test]
fn run_rejects_conflicting_view_and_repo_before_calling_gh() {
    let (_dir, ws) = workspace();
    let cancel = AtomicBool::new(false);
    let err = run(
        "pr_diff",
        &ws,
        &json!({"pr": "pr://12/diff/2", "file": 3}),
        &cancel,
    )
    .unwrap_err();
    assert!(err.contains("already selects a diff view"), "{err}");
    let err = run(
        "pr_diff",
        &ws,
        &json!({"pr": "pr://acme/widgets/12", "repo": "other/repo"}),
        &cancel,
    )
    .unwrap_err();
    assert!(err.contains("conflicts with the pr:// URL repo"), "{err}");
    let err = run(
        "pr_diff",
        &ws,
        &json!({"pr": 12, "all": true, "file": 1}),
        &cancel,
    )
    .unwrap_err();
    assert!(err.contains("not both"), "{err}");
}

#[test]
fn run_rejects_unknown_op_and_bad_pr() {
    let (_dir, ws) = workspace();
    let cancel = AtomicBool::new(false);
    assert!(run("pr_view", &ws, &json!({"pr": 1}), &cancel).is_err());
    assert!(run("pr_diff", &ws, &json!({}), &cancel).is_err());
    assert!(
        run(
            "pr_diff",
            &ws,
            &json!({"pr": "https://github.com/acme/widgets/pull/1"}),
            &cancel
        )
        .is_err()
    );
}
