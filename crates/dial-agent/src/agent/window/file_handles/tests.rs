use super::*;

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    }
}

#[test]
fn replace_mode_reads_introduce_references_without_touching_rows() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    std::fs::write(root.path().join("note.rs"), "source").unwrap();
    std::fs::write(root.path().join("note.rs:2"), "literal").unwrap();
    let rows = "     1\tlet name = \"@f999\";\n";
    assert_eq!(
        model_result(&ws, &call("read", json!({"path":"note.rs:1-4"})), rows),
        format!("[File references: @f1(note.rs)]\n{rows}")
    );
    assert_eq!(
        model_result(&ws, &call("read", json!({"path":"note.rs:2"})), rows),
        format!("[File references: @f2(note.rs:2)]\n{rows}")
    );
    let mut read = call("read", json!({"path":"@f2"}));
    expand(&ws, &mut read).unwrap();
    assert_eq!(read.arguments["path"], "note.rs:2");
    assert_eq!(
        model_result(&ws, &call("read", json!({"path":"note.rs:raw"})), rows),
        rows
    );
    assert_eq!(
        model_result(&ws, &call("read", json!({"path":"missing.rs"})), rows),
        rows
    );
}
#[test]
fn read_headers_issue_reusable_handles_without_changing_source_text() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let raw = "[src/very/long/file.rs#ABCD]\n1:let path = \"src/very/long/file.rs\";\n";
    let model = model_result(
        &ws,
        &call("read", json!({"path":"src/very/long/file.rs"})),
        raw,
    );
    assert_eq!(
        model,
        "[@f1#ABCD]\n1:let path = \"src/very/long/file.rs\";\n"
    );
    let mut read = call("read", json!({"path":"@f1:1-20"}));
    expand(&ws, &mut read).unwrap();
    assert_eq!(read.arguments["path"], "src/very/long/file.rs:1-20");
    let mut edit = call(
        "edit",
        json!({"input":"[@f1#ABCD]\nPUT 1.=1:\n+let path = \"@f1\";\n"}),
    );
    expand(&ws, &mut edit).unwrap();
    assert_eq!(
        edit.arguments["input"],
        "[src/very/long/file.rs#ABCD]\nPUT 1.=1:\n+let path = \"@f1\";\n"
    );
}

#[test]
fn expansion_is_atomic_and_never_rewrites_shell_patterns_or_plugin_arguments() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("file.rs").unwrap();
    let mut read = call("read", json!({"paths":["@f1:2-4","@f999"]}));
    let before = read.arguments.clone();
    assert!(expand(&ws, &mut read).unwrap_err().contains("unknown"));
    assert_eq!(read.arguments, before);
    for (name, args) in [
        ("bash", json!({"command":"cat @f1"})),
        ("grep", json!({"pattern":"@f1","path":"file.rs"})),
        ("plugin_probe", json!({"path":"@f1","input":"@f1"})),
        ("write", json!({"path":"new.rs","content":"@f1"})),
    ] {
        let mut tool = call(name, args.clone());
        expand(&ws, &mut tool).unwrap();
        assert_eq!(tool.arguments, args);
    }
}

#[test]
fn multi_root_search_and_edit_moves_expand_only_protocol_positions() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("one.rs").unwrap();
    ws.file_handle("two.rs").unwrap();
    let mut search = call("grep", json!({"path":"@f1;@f2","pattern":"@f1"}));
    expand(&ws, &mut search).unwrap();
    assert_eq!(
        search.arguments,
        json!({"path":"one.rs;two.rs","pattern":"@f1"})
    );
    let mut edit = call(
        "edit",
        json!({"input":"[@f1#ABCD]\r\nMV @f2\r\nPUT 1.=1:\r\n+MV @f1\r\n"}),
    );
    expand(&ws, &mut edit).unwrap();
    assert_eq!(
        edit.arguments["input"],
        "[one.rs#ABCD]\r\nMV two.rs\r\nPUT 1.=1:\r\n+MV @f1\r\n"
    );
}

#[test]
fn model_projection_keeps_user_history_and_unregistered_paths_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("long/file.rs").unwrap();
    let mut original = vec![
        json!({"role":"user","content":"change long/file.rs"}),
        json!({"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"read","arguments":"{\"path\":\"long/file.rs\"}"}}]}),
        json!({"role":"tool","tool_call_id":"c","content":"[long/file.rs#ABCD]\n1:code","dial_model_content":"[@f1#ABCD]\n1:code"}),
        json!({"role":"assistant","tool_calls":[{"id":"d","function":{"name":"read","arguments":{"path":"new/file.rs"}}}]}),
    ];
    for message in &mut original {
        capture_calls(&ws, message);
    }
    let snapshot = ws.file_handle_snapshot();
    let mut projected = original.clone();
    project(&ws, &mut projected);
    assert_eq!(original[2]["content"], "[long/file.rs#ABCD]\n1:code");
    assert_eq!(projected[0], original[0]);
    assert_eq!(projected[2]["content"], "[@f1#ABCD]\n1:code");
    let args: Value = serde_json::from_str(
        projected[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args["path"], "@f1");
    assert_eq!(projected[3], original[3]);
    assert_eq!(
        ws.file_handle_snapshot(),
        snapshot,
        "previews must not allocate identities"
    );
}

#[test]
fn search_rows_compact_locations_but_not_matched_source() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    std::fs::create_dir_all(root.path().join("long")).unwrap();
    std::fs::write(root.path().join("long/file.rs"), "code").unwrap();
    let text = "long/file.rs:12:use long/file.rs;\nlong/file.rs-13-@f999\n[coverage: done]\n";
    assert_eq!(
        model_result(&ws, &call("grep", json!({})), text),
        "@f1(long/file.rs):12:use long/file.rs;\n@f1-13-@f999\n[coverage: done]\n"
    );
    assert_eq!(model_result(&ws, &call("bash", json!({})), text), text);
}

#[test]
fn resume_restores_issued_references_and_projection_does_not_resurrect_superseded_reads() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("long/file.rs").unwrap();
    let serialized = serde_json::to_value(ws.file_handle_snapshot()).unwrap();
    let resumed = Workspace::new(root.path()).unwrap();
    resumed
        .restore_file_handles(&serde_json::from_value(serialized).unwrap())
        .unwrap();
    let mut tool = call("read", json!({"path":"@f1:1-2"}));
    expand(&resumed, &mut tool).unwrap();
    assert_eq!(tool.arguments["path"], "long/file.rs:1-2");
    let mut messages = vec![
        json!({"role":"tool","tool_call_id":"old","content":format!("[long/file.rs#ABCD]\n{}", "source row\n".repeat(100)),"dial_model_content":format!("[@f1#ABCD]\n{}", "source row\n".repeat(100)),"dial_evidence":{"observation":"same","artifact":"artifact://old","is_error":false}}),
        json!({"role":"tool","tool_call_id":"new","content":"[long/file.rs#ABCD]","dial_model_content":"[@f1#ABCD]","dial_evidence":{"observation":"same","artifact":"artifact://new","is_error":false}}),
    ];
    project(&resumed, &mut messages);
    crate::agent::workflow::evidence::project(
        &mut messages,
        &mut Default::default(),
        &crate::agent::workflow::evidence::Policy {
            warm_tokens: usize::MAX,
            pressure: true,
            protect_tokens: 0,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Superseded")
    );
    assert_eq!(messages[1]["content"], "[@f1#ABCD]");
}

#[test]
fn raw_source_that_looks_like_a_snapshot_header_is_never_rewritten() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let source = "[src/file.rs#ABCD]\nsource text\n";
    let raw = call("read", json!({"path":"source.txt:raw"}));
    assert_eq!(model_result(&ws, &raw, source), source);
    assert!(ws.existing_file_handle("src/file.rs").is_none());
}

#[test]
fn quoted_header_names_roundtrip_through_handle_edits() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let read = call("read", json!({"path":" file.rs"}));
    assert_eq!(
        model_result(&ws, &read, "[\" file.rs\"#ABCD]\n1:source"),
        "[@f1#ABCD]\n1:source"
    );
    assert_eq!(ws.file_handle_path("@f1").unwrap(), " file.rs");
    let mut edit = call(
        "edit",
        json!({"input":"  [@f1#ABCD]  \nPUT 1.=1:\n+code\n"}),
    );
    expand(&ws, &mut edit).unwrap();
    assert_eq!(
        edit.arguments["input"],
        "  [\" file.rs\"#ABCD]  \nPUT 1.=1:\n+code\n"
    );
}

#[test]
fn native_read_compact_edit_and_resume_keep_real_paths_and_read_witnesses() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let path = "deep/project/module/file.txt";
    std::fs::create_dir_all(root.path().join("deep/project/module")).unwrap();
    std::fs::write(root.path().join(path), "before\nunchanged\n").unwrap();
    let read_call = call("read", json!({"path":path}));
    let output = crate::tools::execute(&ws, "read", &read_call.arguments, 20);
    assert!(!output.is_error, "{}", output.text);
    let compact = model_result(&ws, &read_call, &output.text);
    let header = compact.lines().next().unwrap();
    assert!(header.starts_with("[@f1#"), "{compact}");
    assert!(
        output.text.starts_with(&format!("[{path}#")),
        "user evidence must keep the real path"
    );
    let mut edit = call(
        "edit",
        json!({"input":format!("{header}\nPUT 1.=1:\n+after\n")}),
    );
    expand(&ws, &mut edit).unwrap();
    assert!(edit.arguments["input"].as_str().unwrap().contains(path));
    let edited = crate::tools::execute(&ws, "edit", &edit.arguments, 20);
    assert!(!edited.is_error, "{}", edited.text);
    assert_eq!(
        std::fs::read(root.path().join(path)).unwrap(),
        b"after\nunchanged\n"
    );
    let resumed = Workspace::new(root.path()).unwrap();
    resumed
        .restore_file_handles(&ws.file_handle_snapshot())
        .unwrap();
    let mut next = call("read", json!({"path":"@f1:1-2"}));
    expand(&resumed, &mut next).unwrap();
    assert_eq!(next.arguments["path"], format!("{path}:1-2"));
    let reread = crate::tools::execute(&resumed, "read", &next.arguments, 20);
    assert!(!reread.is_error, "{}", reread.text);
    assert!(reread.text.contains("1:after"));
}

#[test]
fn discovery_labels_do_not_depend_on_another_worker_registering_the_path() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    std::fs::write(root.path().join("file.rs"), "code").unwrap();
    ws.file_handle("file.rs").unwrap();
    assert_eq!(
        model_result(&ws, &call("glob", json!({})), "file.rs\n"),
        "@f1(file.rs)\n"
    );
}

#[test]
fn recorded_call_projection_is_stable_after_new_registry_allocations() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let mut messages = vec![
        json!({"role":"assistant","tool_calls":[{"function":{"name":"read","arguments":{"path":"file.rs"}}}]}),
    ];
    capture_calls(&ws, &mut messages[0]);
    let mut before = messages.clone();
    project(&ws, &mut before);
    ws.file_handle("file.rs").unwrap();
    let mut after = messages.clone();
    project(&ws, &mut after);
    assert_eq!(before, after);
}

#[test]
fn symbol_rename_names_are_not_paths() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("file.rs").unwrap();
    let mut rename = call(
        "lsp",
        json!({"action":"rename","path":"@f1","new_name":"@f999"}),
    );
    expand(&ws, &mut rename).unwrap();
    assert_eq!(rename.arguments["path"], "file.rs");
    assert_eq!(rename.arguments["new_name"], "@f999");
}

#[test]
fn structured_edit_modes_expand_paths_without_changing_replacement_text() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("file.rs").unwrap();
    let mut edit = call(
        "edit",
        json!({"path":"@f1","old_string":"@f1","new_string":"@f999"}),
    );
    expand(&ws, &mut edit).unwrap();
    assert_eq!(
        edit.arguments,
        json!({"path":"file.rs","old_string":"@f1","new_string":"@f999"})
    );
}

#[test]
fn recorded_read_selectors_compact_but_signed_replay_stays_untouched() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    ws.file_handle("long/file.rs").unwrap();
    let message = json!({"role":"assistant","tool_calls":[{"function":{"name":"read","arguments":{"paths":["long/file.rs:1-3","long/file.rs:raw:8-9"]}}}]});
    let mut signed = message.clone();
    signed["dial_provider_content"] = json!([{"type":"thinking","signature":"opaque"}]);
    let before = signed.clone();
    capture_calls(&ws, &mut signed);
    project(&ws, std::slice::from_mut(&mut signed));
    assert_eq!(signed, before);
    let mut ordinary = message;
    capture_calls(&ws, &mut ordinary);
    project(&ws, std::slice::from_mut(&mut ordinary));
    assert_eq!(
        ordinary["tool_calls"][0]["function"]["arguments"]["paths"],
        json!(["@f1:1-3", "@f1:raw:8-9"])
    );
}
