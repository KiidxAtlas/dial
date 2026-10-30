use super::*;

fn dispatch(dir: &std::path::Path) -> StreamingDispatch {
    let read = json!({"type":"function","function":{"name":"read","parameters":{
        "type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}});
    let edit = json!({"type":"function","function":{"name":"edit","parameters":{"type":"object"}}});
    StreamingDispatch::new(
        Workspace::new(dir).unwrap(),
        Config::default(),
        Registry::new(),
        Arc::new(AtomicBool::new(false)),
        Admission {
            hooks: Vec::new(),
            always: Vec::new(),
            schemas: vec![read, edit],
            names: vec!["read".into(), "edit".into()],
            failed_calls: BTreeSet::new(),
        },
    )
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "final".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    }
}

#[test]
fn completed_reads_start_early_and_serve_only_the_identical_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha contents\n").unwrap();
    let mut stream = dispatch(dir.path());
    stream.arguments(1, "c1", "read", r#"{"path":"#);
    stream.arguments(1, "", "", r#""a.txt"}"#);
    assert_eq!(stream.started(), 0, "an open call must not start");
    stream.arguments(2, "c2", "edit", r#"{"path":"a.txt"}"#);
    stream.arguments(3, "c3", "read", "{");
    assert_eq!(
        stream.started(),
        1,
        "only the completed read starts; edits never do"
    );
    // Late fragments for a closed index are ignored rather than forming a new call.
    stream.arguments(1, "", "", r#"{"path":"b.txt"}"#);
    assert_eq!(stream.started(), 1);

    let changed = call("read", json!({"path":"other.txt"}));
    assert!(stream.take(&changed, &mut NullUi).is_none());
    let (output, _) = stream
        .take(&call("read", json!({"path":"a.txt"})), &mut NullUi)
        .expect("identical call reuses the early read");
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("alpha contents"), "{}", output.text);
    assert!(
        stream
            .take(&call("read", json!({"path":"a.txt"})), &mut NullUi)
            .is_none(),
        "a result is served once"
    );
}

#[test]
fn restart_drops_partial_calls_and_invalidation_discards_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut stream = dispatch(dir.path());
    stream.arguments(0, "c0", "read", r#"{"path":"a.txt"}"#);
    stream.restart();
    stream.arguments(1, "c1", "read", r#"{"path":"#);
    assert_eq!(
        stream.started(),
        0,
        "a discarded attempt's call never completes"
    );

    stream.arguments(0, "c0", "read", r#"{"path":"a.txt"}"#);
    stream.arguments(1, "c1", "read", "{}");
    assert_eq!(stream.started(), 1);
    stream.invalidate();
    assert!(
        stream
            .take(&call("read", json!({"path":"a.txt"})), &mut NullUi)
            .is_none(),
        "results after a serial call may be stale"
    );
    stream.arguments(2, "c2", "read", "{}");
    stream.arguments(3, "c3", "read", "{}");
    assert_eq!(stream.started(), 0);
}

#[test]
fn failed_reads_are_not_started_early_but_corrected_calls_remain_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixed.txt"), "fixed\n").unwrap();
    let mut stream = dispatch(dir.path());
    let failed = call("read", json!({"path":"missing.txt"}));
    let mut engine = crate::agent::run::engine::Engine::default();
    let mut output = ToolOutput::err("missing file");
    engine.settle(
        &stream.runner.workspace,
        &failed,
        &mut output,
        false,
        false,
        |_| false,
    );
    stream.admission.failed_calls = engine.failed_calls();
    stream.arguments(0, "repeat", "read", r#"{"path":"missing.txt"}"#);
    stream.arguments(1, "corrected", "read", r#"{"path":"fixed.txt"}"#);
    assert_eq!(
        stream.started(),
        0,
        "the failed read must not execute while streaming"
    );
    stream.arguments(2, "end", "edit", "{}");
    assert_eq!(
        stream.started(),
        1,
        "a corrected read can still execute early"
    );
    assert!(stream.take(&failed, &mut NullUi).is_none());
    let (output, _) = stream
        .take(&call("read", json!({"path":"fixed.txt"})), &mut NullUi)
        .unwrap();
    assert!(!output.is_error, "{}", output.text);
}

#[test]
fn provider_confirmed_single_read_finishes_while_response_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut stream = dispatch(dir.path());
    let started = Instant::now();
    stream.arguments(0, "c0", "read", r#"{"path":"a.txt"}"#);
    assert_eq!(stream.started(), 0, "valid JSON alone never starts a read");
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    assert_eq!(
        stream.started(),
        1,
        "duplicate completion cannot dispatch twice"
    );
    // No response-end or next-index event is delivered. Waiting on the worker channel
    // proves overlap deterministically, without a scheduler-dependent sleep threshold.
    let receiver = stream
        .started
        .get(&key(&call("read", json!({"path":"a.txt"}))))
        .unwrap();
    let (output, read_secs) = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("alpha"));
    eprintln!(
        "single-read overlap: read={read_secs:.6}s, completed while response pending at {:.6}s",
        started.elapsed().as_secs_f64()
    );
}

#[test]
fn explicit_completion_uses_authoritative_arguments_and_preserves_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut stream = dispatch(dir.path());
    stream.arguments(0, "c0", "read", r#"{"path":"old.txt"}"#);
    stream.completed(0, "c0", "read", "{");
    stream.completed(1, "c1", "edit", "{}");
    stream.completed(2, "c2", "read", "{}");
    assert_eq!(
        stream.started(),
        0,
        "malformed, mutating and schema-invalid calls stay deferred"
    );
    stream.invalidate();
    stream.completed(3, "c3", "read", r#"{"path":"a.txt"}"#);
    assert_eq!(
        stream.started(),
        0,
        "invalidation also blocks explicit completion"
    );
}

#[test]
fn explicit_completion_never_bypasses_hooks_denials_or_interrupts() {
    let dir = tempfile::tempdir().unwrap();
    for event in ["pre_tool_use", "post_tool_use"] {
        let mut stream = dispatch(dir.path());
        stream
            .admission
            .hooks
            .push(crate::config::types::HookConfig {
                event: event.into(),
                matcher: Some("read".into()),
                command: vec!["must-not-run".into()],
                timeout_secs: 1,
                fail_closed: None,
            });
        stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
        assert_eq!(stream.started(), 0, "{event} requires ordinary dispatch");
    }
    let mut stream = dispatch(dir.path());
    stream
        .admission
        .always
        .push(crate::agent::safety::approval::PermissionRule {
            effect: crate::agent::safety::approval::Effect::Deny,
            tool: "read".into(),
            command_prefix: Vec::new(),
            paths: Vec::new(),
        });
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    assert_eq!(stream.started(), 0);
    let mut stream = dispatch(dir.path());
    stream.runner.interrupt.store(true, Ordering::SeqCst);
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    let (output, _) = stream
        .take(&call("read", json!({"path":"a.txt"})), &mut NullUi)
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.text, interrupted_tool().text);
}
