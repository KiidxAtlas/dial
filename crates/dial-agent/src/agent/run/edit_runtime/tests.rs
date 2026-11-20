use super::*;
use serde_json::Value;

fn request(before: &str, proposed: &str) -> EditRepairRequest {
    EditRepairRequest {
        path: "a.rs".into(),
        before: before.into(),
        proposed: proposed.into(),
        diagnostics: json!({"errors":[{"line":2,"column":9,"missing":true}]}),
    }
}

#[test]
fn scoped_request_and_response_preserve_unrelated_bytes() {
    let req = request(
        "// α private\r\nfn old() {}\r\n// β private",
        "// α private\r\nfn new( {}\r\n// β private",
    );
    let repair = repair_request(&req, 512).unwrap();
    assert!(repair.chat.tools.is_empty());
    let payload: Value =
        serde_json::from_str(repair.chat.messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"path":"a.rs", "first_line":2,"before":"fn old() {}\r\n","proposed":"fn new( {}\r\n","errors":req.diagnostics["errors"]})
    );
    assert_eq!(
        repair
            .complete(r#"{"replacement":"fn new() {}\r\n"}"#)
            .unwrap(),
        "// α private\r\nfn new() {}\r\n// β private"
    );
    for invalid in [
        r#"{"source":"entire file"}"#,
        r#"["ok\n"]"#,
        r#"{"replacement":"ok\n","path":"elsewhere"}"#,
        r#"{"replacement":"unterminated"}"#,
    ] {
        assert!(repair.complete(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn insertion_deletion_repeated_lines_and_eof_round_trip_exactly() {
    for (before, proposed) in [
        ("a\na\n", "a\nb\na\n"),
        ("b\na", "a"),
        ("", "β"),
        ("x\n", "x"),
        ("x", "x\n"),
        ("x\n", ""),
    ] {
        let req = request(before, proposed);
        let repair = repair_request(&req, 1).unwrap();
        let payload: Value =
            serde_json::from_str(repair.chat.messages[1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            repair
                .complete(&json!({"replacement":payload["proposed"]}).to_string())
                .unwrap(),
            proposed
        );
        assert!(
            repair
                .complete(&json!({"replacement":payload["before"]}).to_string())
                .is_err()
        );
    }
}

#[test]
fn budget_limits_disclosed_span_not_file_size() {
    let common = "// unrelated\n".repeat(MAX_REGION_BYTES / 10);
    let req = request(&format!("{common}a"), &format!("{common}b"));
    let repair = repair_request(&req, 32).unwrap();
    let large = "x".repeat(MAX_REGION_BYTES + 1);
    assert!(
        repair
            .complete(&json!({"replacement":large}).to_string())
            .is_err()
    );
    assert!(repair_request(&request(&large, "x"), 32).is_err());
    assert!(repair_request(&request("x", &large), 32).is_err());
}

#[test]
fn only_the_unique_parse_restoring_hunk_is_disclosed() {
    let before = "fn old() {}\n// unchanged\nfn first() {}\n";
    let req = request(before, "fn new() {}\n// unchanged\nfn last( {}\n");
    let repair = repair_request(&req, 128).unwrap();
    let payload: Value =
        serde_json::from_str(repair.chat.messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["before"], "fn first() {}\n");
    assert_eq!(payload["proposed"], "fn last( {}\n");
    assert_eq!(payload["first_line"], 3);
    assert_eq!(
        repair
            .complete(r#"{"replacement":"fn last() {}\n"}"#)
            .unwrap(),
        "fn new() {}\n// unchanged\nfn last() {}\n"
    );
    assert!(
        repair
            .complete(r#"{"replacement":"fn first() {}\n"}"#)
            .is_err()
    );
    // Two independent syntax failures: neither single undo is sufficient.
    assert!(
        repair_request(
            &request(before, "fn new( {}\n// unchanged\nfn last( {}\n"),
            128
        )
        .is_err()
    );
    // Both insertions are legal alone but invalid together. Neither is uniquely at fault.
    let mut ambiguous = request("[\n \n]\n", "[\n1\n \n2\n]\n");
    ambiguous.path = "a.json".into();
    assert_eq!(
        ambiguous.repair_span().unwrap_err(),
        "more than one edit can undo the parse regression"
    );
}

#[test]
fn reconstructed_candidate_still_goes_through_edit_validation() {
    use crate::tools::{EditRepair, Workspace, execute};
    use std::{fs, sync::Arc};

    struct Coach(&'static str);
    impl EditRepair for Coach {
        fn repair(&self, request: &EditRepairRequest) -> Result<String, String> {
            repair_request(request, 128)?.complete(&json!({"replacement": self.0}).to_string())
        }
    }
    let before = "// prefix\nfn old() {}\n// suffix\n";
    for (replacement, expected, error) in [
        (
            "fn new() {}\n",
            "// prefix\nfn new() {}\n// suffix\n",
            false,
        ),
        ("fn old() {}\n", before, true),
        ("fn new( {\n", before, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.rs");
        fs::write(&path, before).unwrap();
        let ws = Workspace::new(root.path()).unwrap();
        let mut settings = ws.edit_settings().clone();
        settings.auto_repair.enabled = true;
        settings.auto_repair.coach = true;
        settings.mode = crate::config::types::EditMode::Replace;
        let ws = ws
            .with_edit_settings(settings)
            .with_syntax_guard(true)
            .with_syntax_block(true)
            .with_edit_repair(Arc::new(Coach(replacement)));
        let read = execute(&ws, "read", &json!({"path":"a.rs"}), 5);
        assert!(!read.is_error, "{}", read.text);
        let out = execute(
            &ws,
            "edit",
            &json!({"path":"a.rs","old_string":"fn old() {}","new_string":"fn new( {"}),
            5,
        );
        assert_eq!(out.is_error, error, "{}", out.text);
        assert_eq!(fs::read_to_string(path).unwrap(), expected);
    }
}

#[test]
fn native_edit_with_and_without_local_repair() {
    use crate::tools::{EditRepair, Workspace, execute};
    use std::{fs, sync::Arc};

    struct NoCoach;
    impl EditRepair for NoCoach {
        fn repair(&self, _: &EditRepairRequest) -> Result<String, String> {
            panic!("local-only repair must never call the configured coach")
        }
    }

    // Each expected result is authored separately, not computed by the repairer.
    // None means that syntax block must refuse and the original bytes must survive.
    for (path, before, proposal, repaired) in [
        (
            "calc.py",
            "def add(a,b): return abs(a-b) # )",
            "def add(a,b): return abs(a+b # )",
            Some("def add(a,b): return abs(a+b) # )"),
        ),
        (
            "values.js",
            "const café = [1, 2];",
            "const café = [1, 31;",
            Some("const café = [1, 31];"),
        ),
        (
            "values.py",
            "values = {'a': 4}",
            "values = {'a': 5",
            Some("values = {'a': 5}"),
        ),
        ("ambiguous.py", "v = f(a - b)", "v = f(a + b * c", None),
        ("revert.rs", "fn f() { g(1); }", "fn f() { g(1; }", None),
        ("new_group.py", "v = a - b", "v = (a + b", None),
        ("literal.py", "v = 'a)'", "v = 'b", None),
        ("mixed.py", "a = 0\nv = f(1)", "a = 2\nv = f(2", None),
    ] {
        for enabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let file = root.path().join(path);
            fs::write(&file, before).unwrap();
            let ws = Workspace::new(root.path()).unwrap();
            let mut settings = ws.edit_settings().clone();
            settings.mode = crate::config::types::EditMode::Replace;
            settings.auto_repair.enabled = enabled;
            let ws = ws
                .with_edit_settings(settings)
                .with_syntax_guard(true)
                .with_syntax_block(true)
                .with_edit_repair(Arc::new(NoCoach));
            let read = execute(&ws, "read", &json!({"path":path}), 5);
            assert!(!read.is_error, "{}", read.text);
            let out = execute(
                &ws,
                "edit",
                &json!({"path":path, "old_string":before, "new_string":proposal}),
                5,
            );
            let expected = repaired.filter(|_| enabled);
            assert_eq!(
                out.is_error,
                expected.is_none(),
                "{path}, enabled={enabled}: {}",
                out.text
            );
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                expected.unwrap_or(before),
                "{path}, enabled={enabled}"
            );
            if let Some(fixed) = expected {
                assert!(
                    out.text.contains(fixed),
                    "diff must show committed bytes: {}",
                    out.text
                );
                assert!(!out.text.contains("Auto-repair"), "{}", out.text);
            } else if let Some(correct) = repaired {
                // Ordinary retry, supplied with the correct edit rather than a simulated model.
                let retry = execute(
                    &ws,
                    "edit",
                    &json!({"path":path, "old_string":before, "new_string":correct}),
                    5,
                );
                assert!(!retry.is_error, "{path}: {}", retry.text);
                assert_eq!(fs::read_to_string(&file).unwrap(), correct);
            }
        }
    }
}
