use super::*;
fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "test".into(),
        name: name.into(),
        arguments: json!({}),
        malformed_arguments: false,
    }
}
#[test]
fn corrected_calls_keep_the_tool_schema_and_normalized_repeats_stay_withheld() {
    let mut checkpoint = RecoveryCheckpoint::default();
    let mut failed = call("bash");
    failed.arguments = json!({"command":"cargo check", "timeout":10});
    checkpoint.arm(&failed, "failed check");
    let mut retry = failed.clone();
    retry.arguments = json!({"command":"cargo   'check'", "timeout":20});
    assert!(checkpoint.rejection(&retry).is_some());
    retry.arguments = json!({"command":"cargo test"});
    assert!(checkpoint.rejection(&retry).is_none());
    let mut schemas = vec![json!({"function":{"name":"bash"}})];
    let mut names = vec!["bash".into()];
    let mut messages = vec![];
    checkpoint.apply(&mut schemas, &mut names, &mut messages);
    assert_eq!(schemas.len(), 1);
    assert_eq!(names, ["bash"]);
    assert_eq!(messages.len(), 1);
}

#[test]
fn externally_changed_source_releases_only_its_read_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let ws = crate::tools::Workspace::new(dir.path()).unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "original").unwrap();
    let mut checkpoint = RecoveryCheckpoint::default();
    let mut read = call("read");
    read.arguments = json!({"path":"a.txt:1-1"});
    let mut investigation = crate::agent::workflow::investigation::InvestigationGuard::default();
    let mut repeats = crate::agent::safety::guard::RepeatGuard::default();
    checkpoint.arm(&read, "unchanged evidence");
    checkpoint.arm(&call("bash"), "failed command");
    checkpoint.observe_at(
        &ws,
        &read,
        &crate::runtime::tool_output::ToolOutput {
            execution: Default::default(),
            data: None,
            text: "original".into(),
            is_error: false,
        },
        &mut investigation,
        &mut repeats,
    );
    assert!(checkpoint.rejection_at(&ws, &read).is_some());
    std::fs::write(dir.path().join("unrelated.txt"), "other").unwrap();
    assert!(checkpoint.rejection_at(&ws, &read).is_some());
    std::fs::write(&path, "changed").unwrap();
    assert!(checkpoint.rejection_at(&ws, &read).is_none());
    assert!(checkpoint.rejection_at(&ws, &call("bash")).is_some());
}

#[test]
fn only_meaningful_recovery_restores_tools() {
    let mut checkpoint = RecoveryCheckpoint::default();
    checkpoint.arm(&call("read"), "unchanged evidence");
    checkpoint.progress(&call("bash"), false, false, false);
    checkpoint.progress(&call("todo"), true, false, false);
    checkpoint.progress(&call("grep"), true, false, false);
    checkpoint.progress(&call("artifact_read"), true, false, false);
    checkpoint.progress(&call("edit"), true, false, false);
    let mut task = call("todo");
    task.arguments = json!({"op":"set"});
    checkpoint.progress(&task, true, true, false);
    assert!(checkpoint.rejection(&call("read")).is_some());
    let mut other = call("read");
    other.arguments = json!({"path":"another.rs"});
    assert!(checkpoint.rejection(&other).is_none());
    checkpoint.progress(&call("write"), true, false, true);
    assert!(checkpoint.rejection(&call("read")).is_none());
    checkpoint.arm(&call("edit"), "wrong arguments");
    task.arguments = json!({"op":"done"});
    checkpoint.progress(&task, true, true, false);
    assert!(checkpoint.rejection(&call("edit")).is_none());
    checkpoint.arm(&call("read"), "unchanged evidence");
    task.arguments = json!({"op":"block"});
    checkpoint.progress(&task, true, true, false);
    assert!(checkpoint.rejection(&call("read")).is_none());
    checkpoint.arm(&call("finish"), "invalid outcome");
    assert!(checkpoint.rejection(&call("finish")).is_none());
}
