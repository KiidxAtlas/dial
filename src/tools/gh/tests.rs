use super::super::gh_common::test_support::FakeGh;
use super::*;

#[test]
fn schema_declares_github_with_all_ops_and_required_op() {
    let schema = GitHub.schema();
    assert_eq!(schema["function"]["name"], "github");
    let params = &schema["function"]["parameters"];
    assert_eq!(params["required"], json!(["op"]));
    let ops = params["properties"]["op"]["enum"].as_array().unwrap();
    assert_eq!(ops.len(), 12);
    for read in READ_OPS {
        assert!(
            ops.iter().any(|op| op.as_str() == Some(*read)),
            "{read} missing from op enum"
        );
    }
}

#[test]
fn read_ops_need_no_approval_and_everything_else_does() {
    for op in READ_OPS {
        assert!(
            !GitHub.needs_approval(&json!({"op": op})),
            "{op} should not need approval"
        );
    }
    for op in ["pr_create", "pr_checkout", "pr_push"] {
        assert!(
            GitHub.needs_approval(&json!({"op": op})),
            "{op} should need approval"
        );
    }
    assert!(GitHub.needs_approval(&json!({})));
    assert!(GitHub.needs_approval(&json!({"op": "delete_everything"})));
    assert!(GitHub.needs_approval(&json!({"op": 7})));
}

#[test]
fn only_pr_checkout_modifies_the_workspace() {
    assert!(GitHub.may_modify_workspace(&json!({"op": "pr_checkout"})));
    for op in [
        "pr_push",
        "pr_create",
        "repo_view",
        "file_read",
        "run_watch",
    ] {
        assert!(!GitHub.may_modify_workspace(&json!({"op": op})), "{op}");
    }
    assert!(!GitHub.may_modify_workspace(&json!({})));
}

#[test]
fn read_ops_are_shared_and_mutating_ops_are_exclusive() {
    for op in READ_OPS {
        assert_eq!(
            GitHub.concurrency(&json!({"op": op})),
            Concurrency::Shared,
            "{op}"
        );
    }
    assert_eq!(
        GitHub.concurrency(&json!({"op": "pr_push"})),
        Concurrency::Exclusive
    );
    assert_eq!(GitHub.concurrency(&json!({})), Concurrency::Exclusive);
}

#[test]
fn missing_and_unknown_ops_are_error_results() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let missing = quiet(&ws, &json!({}), &cancel);
    assert!(missing.is_error);
    assert_eq!(missing.text, "github: `op` is required");
    let unknown = quiet(&ws, &json!({"op": "nope"}), &cancel);
    assert!(unknown.is_error);
    assert!(unknown.text.starts_with("github: unknown op \"nope\""));
}

#[test]
fn tools_registers_github_once() {
    let config = Config::default();
    let tools = tools(&config);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "github");
}

#[test]
fn pr_diff_cache_hit_serves_without_calling_gh() {
    let gh = FakeGh::new("", "", 0);
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let key = super::super::gh_cache::pr_diff_key(
        std::path::Path::new("/w"),
        &json!({"pr": "77001", "repo": "octo/repo"}),
    )
    .unwrap();
    super::super::gh_cache::cached(&key, || {
        Ok(ToolOutput {
            execution: Default::default(),
            text: "seeded".into(),
            is_error: false,
            data: None,
        })
    })
    .unwrap();
    let out = quiet(
        &ws,
        &json!({"op": "pr_diff", "pr": "77001", "repo": "octo/repo"}),
        &cancel,
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "seeded");
    assert!(
        gh.calls().is_empty(),
        "cache hit must not call gh: {:?}",
        gh.calls()
    );
}

#[test]
fn pr_diff_cache_miss_reaches_gh() {
    let gh = FakeGh::new("", "", 0);
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let _ = quiet(
        &ws,
        &json!({"op": "pr_diff", "pr": "77002", "repo": "octo/repo"}),
        &cancel,
    );
    assert!(!gh.calls().is_empty(), "cache miss must call gh");
}

#[test]
fn pr_push_invalidates_the_cached_pr_diff() {
    let _gh = FakeGh::new("", "", 0);
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let key = super::super::gh_cache::pr_diff_key(
        std::path::Path::new("/w"),
        &json!({"pr": "77003", "repo": "octo/repo"}),
    )
    .unwrap();
    super::super::gh_cache::cached(&key, || {
        Ok(ToolOutput {
            execution: Default::default(),
            text: "old".into(),
            is_error: false,
            data: None,
        })
    })
    .unwrap();
    let _ = quiet(&ws, &json!({"op": "pr_push", "pr": "77003"}), &cancel);
    let refilled = super::super::gh_cache::cached(&key, || {
        Ok(ToolOutput {
            execution: Default::default(),
            text: "new".into(),
            is_error: false,
            data: None,
        })
    })
    .unwrap();
    assert_eq!(refilled.text, "new");
}

#[test]
fn run_watch_forwards_progress_snapshots() {
    let _gh = FakeGh::new(
        r#"{"id":123,"name":"CI","status":"completed","conclusion":"success","head_branch":"main","head_sha":"abc"}"#,
        "",
        0,
    );
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut seen: Vec<String> = Vec::new();
    let out = execute(
        &ws,
        &json!({"op": "run_watch", "run": "https://github.com/octo/repo/actions/runs/123"}),
        &cancel,
        &mut |text: &str| seen.push(text.to_owned()),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        !seen.is_empty(),
        "run_watch must report at least one progress snapshot"
    );
}

/// Run one call without progress reporting, as the non-streaming `Tool::call` path does.
fn quiet(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> ToolOutput {
    execute(ws, args, cancel, &mut |_: &str| {})
}
