use super::*;
#[test]
fn persisted_resume_checks_workspace_lease_and_exact_execution_contract() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let sink = crate::agent::workers::child_transcript::ChildTranscript::create(&layout, "parent", "task-1", json!({"id":"task-1", "resume_contract":{
        "version":1,"workspace":identity(project.path()).unwrap(), "task":{"task":"original","solution_space":"s"},
        "tools":["read","write"], "model":"test/model", "definition":null
    }})).unwrap();
    sink.execution_contract(&["read".into()], "test/model")
        .unwrap();
    sink.record(&json!({"role":"user","content":"original"}))
        .unwrap();
    sink.record(&json!({"role":"assistant","content":"finding"}))
        .unwrap();
    let held = lease(&layout, "parent", "task-1").unwrap();
    assert!(load(&layout, "parent", project.path(), "task-1", "follow-up").is_err());
    drop(held);
    let other = tempfile::tempdir().unwrap();
    assert!(load(&layout, "parent", other.path(), "task-1", "follow-up").is_err());
    // A flock is shared with any child forked (not yet exec'd) by a concurrently running test, so
    // the release after `drop` can lag by a few milliseconds.
    let saved = (0..100)
        .find_map(|_| {
            load(&layout, "parent", project.path(), "task-1", "follow-up")
                .ok()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("lease is released once the holder is dropped");
    assert_eq!(saved.tools, ["read"]);
    assert_eq!(saved.messages.len(), 2);
    assert_eq!(saved.task["task"], "follow-up");
    drop(saved);
    sink.record(&json!({"role":"assistant","tool_calls":[{"id":"uncertain-write"}]}))
        .unwrap();
    assert!(load(&layout, "parent", project.path(), "task-1", "follow-up").is_err());
}

#[test]
fn continuation_requires_complete_effect_records() {
    let call = json!({"role":"assistant","tool_calls":[{"id":"write-1"}]});
    assert!(validate_history(std::slice::from_ref(&call)).is_err());
    assert!(
        validate_history(&[
            call,
            json!({"role":"tool","tool_call_id":"write-1","content":"done"})
        ])
        .is_ok()
    );
    assert!(validate_history(&[json!({"role":"tool","tool_call_id":"missing"})]).is_err());
    assert!(
        validate_history(&[
            json!({"role":"assistant","content":"[message truncated: 300000 bytes]"})
        ])
        .is_err()
    );
}
