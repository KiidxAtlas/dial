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
            plugins: None,
            always: Vec::new(),
            schemas: vec![read, edit],
            names: vec!["read".into(), "edit".into()],
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
