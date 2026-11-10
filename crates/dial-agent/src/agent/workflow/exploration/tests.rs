use super::*;

#[test]
fn terminal_siblings_excludes_rewind_by_identity_for_both_surfaces() {
    for (name, arguments) in [
        ("rewind", json!({"report":"findings"})),
        ("checkpoint", json!({"action":"rewind","report":"findings"})),
    ] {
        let messages = vec![
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"complete","type":"function","function":{"name":name,"arguments":arguments.to_string()}},
                {"id":"sibling","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"complete","content":"requested"}),
            json!({"role":"tool","tool_call_id":"sibling","content":"retained"}),
        ];
        let siblings = terminal_siblings(&messages, "complete").unwrap();
        assert_eq!(siblings.len(), 2);
        assert_eq!(siblings[0]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(siblings[0]["tool_calls"][0]["id"], "sibling");
        assert_eq!(siblings[1]["tool_call_id"], "sibling");
    }
}

#[test]
fn private_lifecycle_accepts_canonical_rewind_without_changing_call_identity() {
    let call = ToolCall {
        id: "accepted-call".into(),
        name: "checkpoint".into(),
        arguments: json!({"action":"rewind","report":"Verified findings"}),
        malformed_arguments: false,
    };
    let mut lifecycle = Lifecycle {
        active: Some(Checkpoint {
            owner_id: "owner".into(),
            entry_id: "entry".into(),
            message_count: 0,
            started_at: "timestamp".into(),
            goal: "Inspect".into(),
            instruction_entry: None,
        }),
        ..Default::default()
    };
    let output = lifecycle.request(
        &call,
        "owner",
        "policy".into(),
        "timestamp".into(),
        &AtomicBool::new(false),
    );
    assert!(!output.is_error, "{}", output.text);
    let Some(Request::Rewind {
        call_id, report, ..
    }) = lifecycle.pending
    else {
        panic!("no rewind request")
    };
    assert_eq!(call_id, "accepted-call");
    assert_eq!(report, "Verified findings");
}
