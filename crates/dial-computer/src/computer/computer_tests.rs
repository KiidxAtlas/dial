use super::*;

#[test]
fn computer_timeout_clamps_to_bounds() {
    assert_eq!(clamp_timeout(None), Duration::from_secs(120));
    assert_eq!(clamp_timeout(Some(0.0)), Duration::from_secs(1));
    assert_eq!(clamp_timeout(Some(9999.0)), Duration::from_secs(300));
}

#[test]
fn computer_policy_tiers() {
    assert_eq!(
        computer_policy(&json!({"action": "capabilities"})),
        Policy::Read
    );
    assert_eq!(computer_policy(&json!({"action": "close"})), Policy::Exec);
    assert_eq!(
        computer_policy(&json!({"action": "run", "code": "1"})),
        Policy::Exec
    );
    assert_eq!(
        computer_policy(&json!({"action": "run", "code": "1", "read_only": true})),
        Policy::Exec
    );
    assert_eq!(computer_policy(&Value::Null), Policy::Exec);
    assert_eq!(
        computer_policy(&json!({"action": "call", "chain": [{"method": "nope", "args": []}]})),
        Policy::Exec
    );
}

#[test]
fn computer_run_requires_exactly_one_source() {
    assert!(resolve_run_source(None, None, None).is_err());
    assert!(resolve_run_source(Some("a".into()), Some("b".into()), None).is_err());
    assert_eq!(
        resolve_run_source(Some("1".into()), None, None).unwrap(),
        "1"
    );
}

#[test]
fn computer_inline_cap_keeps_head_and_tail() {
    let text: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
    let capped = cap_inline(text.clone(), |_| Some("artifact://x".into()));
    assert!(capped.len() < text.len());
    assert!(capped.starts_with("line 0\n"));
    assert!(capped.contains("elided"));
    assert!(capped.contains("line 9999"));
    assert!(capped.ends_with("[raw output: artifact://x]"));
    assert_eq!(cap_inline("short".into(), |_| None), "short");
}
#[test]
fn computer_fork_close_does_not_close_parent() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let context = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let parent = Computer {
        session: Arc::new(ComputerSession::new(dir.path().display().to_string())),
    };
    let child = parent.fork_session().expect("computer must be owner-local");
    assert!(!child.call(&context, &json!({"action":"close"})).is_error);
    assert!(!parent.session.closed.load(Ordering::Acquire));
    assert!(
        child
            .call(&context, &json!({"action":"capabilities"}))
            .is_error
    );
    assert!(!parent.call(&context, &json!({"action":"close"})).is_error);
    // No desktop backend or process was created by any of these close-only operations.
}
