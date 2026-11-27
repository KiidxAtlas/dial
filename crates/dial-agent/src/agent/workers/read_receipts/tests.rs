use super::*;
use crate::runtime::tool_output::ToolOutput;
use serde_json::json;

fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let full = root.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    let workspace = Workspace::new(root.path()).unwrap();
    (root, workspace)
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "read-1".into(),
        name: "read".into(),
        arguments,
        malformed_arguments: false,
    }
}

/// Runs the real `read` tool; the output is what the child would receive.
fn run_read(workspace: &Workspace, arguments: Value) -> (ToolCall, ToolOutput) {
    let output = crate::tools::execute(workspace, "read", &arguments, 10);
    (call(arguments), output)
}

fn text_output(text: &str) -> ToolOutput {
    ToolOutput {
        text: text.into(),
        data: None,
        is_error: false,
        execution: Default::default(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::hex(&Sha256::digest(bytes))
}

const REVISION: &str =
    "[Source raw SHA256:0000000000000000000000000000000000000000000000000000000000000000]";

#[test]
fn receipt_is_the_rows_one_read_displayed_at_its_revision() {
    let text = "a\nb\nc\nd\n";
    let (_root, workspace) = workspace(&[("src/lib.rs", text)]);
    let (call, output) = run_read(&workspace, json!({"path": "src/lib.rs:2-3"}));
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    assert_eq!(receipts.files[0].path, "src/lib.rs");
    assert_eq!(receipts.files[0].revision, sha256_hex(text.as_bytes()));
    assert_eq!(receipts.files[0].ranges, vec![(2, 3)]);
    let status = |span| receipts.span_status(&workspace, "src/lib.rs", span);
    assert_eq!(status((2, 2)), SpanStatus::Current);
    assert_eq!(status((1, 2)), SpanStatus::NoReceipt);
}

#[test]
fn offset_and_limit_windows_bind_the_same_way() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\nc\nd\n")]);
    let (call, output) = run_read(
        &workspace,
        json!({"path": "src/lib.rs", "offset": 2, "limit": 2}),
    );
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    assert_eq!(receipts.files[0].ranges, vec![(2, 3)]);
}

#[test]
fn rows_the_parent_or_store_saw_before_the_worker_are_not_receipts() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\nc\nd\n")]);
    // The parent displays rows 1-2 through the same tool; the edit store witnesses them.
    let parent = crate::tools::execute(&workspace, "read", &json!({"path": "src/lib.rs:1-2"}), 10);
    assert!(!parent.is_error);
    let (call, output) = run_read(&workspace, json!({"path": "src/lib.rs:3-3"}));
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    assert_eq!(receipts.files[0].ranges, vec![(3, 3)]);
    let status = |span| receipts.span_status(&workspace, "src/lib.rs", span);
    assert_eq!(status((1, 2)), SpanStatus::NoReceipt);
    assert_eq!(status((3, 3)), SpanStatus::Current);
}

#[test]
fn file_changed_after_the_worker_read_it_is_changed_not_current() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\nc\n")]);
    let (call, output) = run_read(&workspace, json!({"path": "src/lib.rs:1-2"}));
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    std::fs::write(workspace.root().join("src/lib.rs"), "a\nB\nc\n").unwrap();
    let status = |span| receipts.span_status(&workspace, "src/lib.rs", span);
    assert_eq!(status((1, 2)), SpanStatus::Changed);
    assert_eq!(status((3, 3)), SpanStatus::NoReceipt);
}

#[test]
fn rows_cut_by_the_output_limit_are_not_displayed_in_full() {
    let long = "x".repeat(400_000);
    let (_root, workspace) = workspace(&[("src/lib.rs", &format!("a\n{long}\nc\n"))]);
    let (call, output) = run_read(&workspace, json!({"path": "src/lib.rs:1-3"}));
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    assert_eq!(receipts.files[0].ranges, vec![(1, 1)]);
    let status = |span| receipts.span_status(&workspace, "src/lib.rs", span);
    assert_eq!(status((2, 2)), SpanStatus::NoReceipt);
}

#[test]
fn later_reads_merge_at_one_revision_and_bound_the_file_count() {
    let (_root, ws) = workspace(&[("src/lib.rs", "a\nb\nc\n")]);
    let mut receipts = ReadReceipts::default();
    for arguments in [
        json!({"path": "src/lib.rs:1-1"}),
        json!({"path": "src/lib.rs:3-3"}),
    ] {
        let (call, output) = run_read(&ws, arguments);
        receipts.record(&ws, &call, &output);
    }
    assert_eq!(receipts.files.len(), 1);
    assert_eq!(receipts.files[0].ranges, vec![(1, 1), (3, 3)]);

    let files: Vec<(String, String)> = (0..MAX_FILES + 1)
        .map(|n| (format!("f{n}.txt"), "line\n".to_owned()))
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let (_root, workspace) = workspace(&borrowed);
    let mut receipts = ReadReceipts::default();
    for (path, _) in &files {
        let (call, output) = run_read(&workspace, json!({"path": format!("{path}:1-1")}));
        receipts.record(&workspace, &call, &output);
    }
    assert_eq!(receipts.files.len(), MAX_FILES);
    assert!(receipts.truncated);
}

#[test]
fn multi_target_raw_and_failed_reads_record_nothing() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\n"), ("src/other.rs", "x\n")]);
    let cases = [
        json!({"paths": [{"path": "src/lib.rs:1-1"}, {"path": "src/other.rs:1-1"}]}),
        json!({"path": "src/lib.rs:1-2", "raw": true}),
        json!({"path": "src/lib.rs:1-2", "fresh": true}),
        json!({"path": "src/gone.rs:1-1"}),
    ];
    for arguments in cases {
        let (call, output) = run_read(&workspace, arguments.clone());
        let mut receipts = ReadReceipts::default();
        receipts.record(&workspace, &call, &output);
        assert_eq!(receipts, ReadReceipts::default(), "{arguments}");
    }
}

#[test]
fn crafted_or_mismatched_native_output_is_declined_or_bounded() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\nc\n"), ("src/other.rs", "x\n")]);
    let lib = "[src/lib.rs#ABCD]";
    let cases: Vec<(&str, Value, String, Option<Vec<(usize, usize)>>)> = vec![
        (
            "header names another file",
            json!({"path": "src/lib.rs:1-1"}),
            format!("[src/other.rs#ABCD]\n{REVISION}\n1:x\n"),
            None,
        ),
        (
            "no native source line (lookalike header in content)",
            json!({"path": "src/lib.rs:1-1"}),
            format!("{lib}\n1:a\n"),
            None,
        ),
        (
            "malformed revision",
            json!({"path": "src/lib.rs:1-1"}),
            format!("{lib}\n[Source raw SHA256:xyz]\n1:a\n"),
            None,
        ),
        (
            "notebook projection line before rows",
            json!({"path": "src/lib.rs:1-1"}),
            format!("{lib}\n{REVISION}\n[Notebook cell-source projection]\n1:a\n"),
            None,
        ),
        (
            "rows after a footer are not displayed by this read",
            json!({"path": "src/lib.rs:1-3"}),
            format!(
                "{lib}\n{REVISION}\n1:a\n2:b\n[lines 1-2 of 3; continue with `src/lib.rs:3-`]\n3:c\n"
            ),
            Some(vec![(1, 2)]),
        ),
        (
            "appended log rows with a gap are not counted",
            json!({"path": "src/lib.rs:1-3"}),
            format!("{lib}\n{REVISION}\n1:a\n3:c\n"),
            Some(vec![(1, 1)]),
        ),
        (
            "rows outside the requested window are not counted",
            json!({"path": "src/lib.rs", "offset": 2, "limit": 1}),
            format!("{lib}\n{REVISION}\n2:b\n3:c\n"),
            Some(vec![(2, 2)]),
        ),
        (
            "a cut row ends the body",
            json!({"path": "src/lib.rs:1-2"}),
            format!("{lib}\n{REVISION}\n1:a\n2:bbb …[line truncated]\n"),
            Some(vec![(1, 1)]),
        ),
        (
            "pointer text without rows",
            json!({"path": "src/lib.rs:1-2"}),
            format!("{lib}\n{REVISION}\n(rows 1-2 already visible)\n"),
            None,
        ),
    ];
    for (name, arguments, text, expected) in cases {
        let mut receipts = ReadReceipts::default();
        receipts.record(&workspace, &call(arguments), &text_output(&text));
        let ranges = receipts.files.first().map(|receipt| receipt.ranges.clone());
        assert_eq!(ranges, expected, "{name}");
    }
}

#[test]
fn stored_receipts_round_trip_and_older_results_have_none() {
    let (_root, workspace) = workspace(&[("src/lib.rs", "a\nb\n")]);
    let (call, output) = run_read(&workspace, json!({"path": "src/lib.rs:1-2"}));
    let mut receipts = ReadReceipts::default();
    receipts.record(&workspace, &call, &output);
    let stored = serde_json::to_value(&receipts).unwrap();
    assert_eq!(ReadReceipts::from_value(&stored), receipts);
    assert_eq!(
        ReadReceipts::from_value(&Value::Null),
        ReadReceipts::default()
    );
}
