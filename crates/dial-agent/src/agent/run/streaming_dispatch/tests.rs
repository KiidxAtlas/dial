use super::*;

#[test]
fn an_effect_closes_the_read_prefix_and_live_denials_never_start_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut stream = dispatch(dir.path());
    let mut approvals = Vec::new();
    stream.completed_if(0, "denied", "read", r#"{"path":"a.txt"}"#, &mut |call| {
        approvals.push(call.id.clone());
        false
    });
    assert_eq!(stream.statistics(), (0, 0));
    stream.completed_if(1, "allowed", "read", r#"{"path":"a.txt"}"#, &mut |_| true);
    stream.completed(2, "effect", "edit", r#"{"path":"other"}"#);
    stream.completed(3, "later", "read", r#"{"path":"other"}"#);
    assert_eq!(approvals, vec!["denied"]);
    assert_eq!(stream.statistics(), (1, 0));
    assert!(
        stream
            .take(&call("read", json!({"path":"other"})), &mut NullUi)
            .is_none()
    );
    assert!(
        stream
            .take(&call("read", json!({"path":"a.txt"})), &mut NullUi)
            .is_some()
    );
    assert_eq!(stream.statistics(), (1, 1));
}

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
        Default::default(),
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
    let (output, read_secs) = receiver
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
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

#[test]
fn bookkeeping_keeps_the_read_prefix_and_later_reads_open() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut stream = dispatch(dir.path());
    stream.completed(0, "plan", "todo", r#"{"todos":[]}"#);
    stream.completed(1, "c1", "read", r#"{"path":"a.txt"}"#);
    assert_eq!(stream.started(), 1, "todo observes no workspace state");
    let todo = call("todo", json!({"todos":[]}));
    stream.ran(&todo, false, None);
    assert!(stream.has(&call("read", json!({"path":"a.txt"}))));
    assert!(!stream.cancelled());
}

#[test]
fn a_serial_call_drops_only_conflicting_reads_and_unknown_effects_cancel_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.txt", "b.txt"] {
        std::fs::write(dir.path().join(file), "source\n").unwrap();
    }
    let mut stream = dispatch(dir.path());
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    stream.completed(1, "c1", "read", r#"{"path":"b.txt"}"#);
    assert_eq!(stream.started(), 2);
    let workspace = Workspace::new(dir.path()).unwrap();
    let write = call("write", json!({"path":"b.txt","content":"new"}));
    let footprint = crate::agent::run::dependency_dispatch::footprint_for(
        &workspace,
        crate::config::types::EditMode::default(),
        &Registry::new(),
        &write,
    );
    assert!(footprint.is_some());
    stream.ran(&write, false, footprint.clone());
    assert!(stream.has(&call("read", json!({"path":"a.txt"}))));
    assert!(!stream.has(&call("read", json!({"path":"b.txt"}))));
    assert!(!stream.cancelled(), "a disjoint read keeps running");

    // A hooked call may touch anything, even with a resolved footprint.
    stream.ran(&write, true, footprint);
    assert!(!stream.has(&call("read", json!({"path":"a.txt"}))));
    assert!(stream.cancelled());
}

#[test]
fn a_cancelled_stream_never_delivers_an_extracted_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let mut stream = dispatch(dir.path());
    stream.completed(0, "c0", "read", r#"{"path":"a.txt"}"#);
    let early = stream
        .extract(&call("read", json!({"path":"a.txt"})))
        .unwrap();
    stream.ran(&call("bash", json!({"command":"touch a.txt"})), false, None);
    assert!(stream.cancelled());
    assert!(
        early.receive().is_none(),
        "the caller reruns the read instead of using a cancelled result"
    );
}

fn barrier_footprint(dir: &std::path::Path, call: &ToolCall) -> Footprint {
    crate::agent::run::dependency_dispatch::footprint_for(
        &Workspace::new(dir).unwrap(),
        crate::config::types::EditMode::default(),
        &Registry::new(),
        call,
    )
    .unwrap()
}

fn read(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({ "path": path }),
        malformed_arguments: false,
    }
}

#[test]
fn reads_beside_a_pending_approval_start_only_when_disjoint_and_admitted() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        std::fs::write(dir.path().join(file), format!("{file} source\n")).unwrap();
    }
    let mut stream = dispatch(dir.path());
    // The stream sealed at the pending write while the reply streamed.
    stream.completed(0, "w", "write", r#"{"path":"b.txt","content":"new"}"#);
    let write = call("write", json!({"path":"b.txt","content":"new"}));
    let barrier = barrier_footprint(dir.path(), &write);
    let later = [
        read("r1", "a.txt"),
        read("r2", "b.txt"),
        call("todo", json!({"todos":[]})),
        read("r3", "c.txt"),
        call("bash", json!({"command":"touch d.txt"})),
        read("r4", "d.txt"),
    ];
    let mut asked = Vec::new();
    let started = stream.start_beside(std::slice::from_ref(&barrier), &later, &mut |call| {
        asked.push(call.id.clone());
        call.id != "r3"
    });
    assert_eq!(started, 1, "only the disjoint, controller-admitted read");
    assert_eq!(asked, ["r1", "r3"], "the conflicting read is never offered");
    assert!(stream.has(&read("r1", "a.txt")));
    assert!(
        !stream.has(&read("r2", "b.txt")),
        "it conflicts with the barrier"
    );
    assert!(
        !stream.has(&read("r3", "c.txt")),
        "the controller refused it"
    );
    assert!(
        !stream.has(&read("r4", "d.txt")),
        "never past a later effect"
    );
    // The approved barrier ran with its declared footprint: the held read survives both checks.
    stream.ran(&write, false, Some(barrier));
    let mut wrote = ToolOutput::ok("wrote".into());
    wrote.execution.changed_paths.push("b.txt".into());
    stream.settled_beside(&wrote);
    let held = stream.take(&read("r1", "a.txt"), &mut NullUi).unwrap();
    assert!(held.0.text.contains("a.txt source"), "{}", held.0.text);
}

#[test]
fn a_denied_barrier_keeps_held_reads_and_reached_or_unknown_effects_discard_them() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.path().join(file), "source\n").unwrap();
    }
    let write = call("write", json!({"path":"c.txt","content":"new"}));
    let barrier = barrier_footprint(dir.path(), &write);
    let later = [read("r1", "a.txt"), read("r2", "b.txt")];

    // Denied: nothing ran, so every held read stays valid.
    let mut stream = dispatch(dir.path());
    assert_eq!(
        stream.start_beside(std::slice::from_ref(&barrier), &later, &mut |_| true),
        2
    );
    stream.settled_beside(
        &ToolOutput::err("denied").with_effects(crate::runtime::execution::Effects::None),
    );
    assert!(stream.has(&later[0]) && stream.has(&later[1]));

    // Approved, but its recorded changes reached a held read's file: only that one re-runs.
    let mut stream = dispatch(dir.path());
    assert_eq!(
        stream.start_beside(std::slice::from_ref(&barrier), &later, &mut |_| true),
        2
    );
    let mut reached = ToolOutput::ok("wrote".into());
    reached.execution.changed_paths.push("b.txt".into());
    stream.settled_beside(&reached);
    assert!(stream.has(&later[0]));
    assert!(!stream.has(&later[1]));
    assert!(!stream.cancelled());

    // Unknown effects may have reached anything.
    let mut stream = dispatch(dir.path());
    assert_eq!(
        stream.start_beside(std::slice::from_ref(&barrier), &later, &mut |_| true),
        2
    );
    stream.settled_beside(
        &ToolOutput::ok("ran".into()).with_effects(crate::runtime::execution::Effects::Unknown),
    );
    assert!(!stream.has(&later[0]));
    assert!(stream.cancelled());

    // An invalidated stream never starts reads beside a barrier.
    assert_eq!(
        stream.start_beside(std::slice::from_ref(&barrier), &later, &mut |_| true),
        0
    );
}
