use super::*;

fn claude_fixture() -> Vec<Value> {
    vec![
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s1","cwd":"/work",
               "message":{"role":"user","content":"fix the bug"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1",
               "message":{"id":"m1","model":"claude-x","content":[{"type":"thinking","thinking":"hmm"}]}}),
        // Abandoned branch off u1: not on the active chain, so it must not be imported.
        json!({"type":"assistant","uuid":"dead","parentUuid":"u1",
               "message":{"id":"m9","content":[{"type":"text","text":"abandoned"}]}}),
        json!({"type":"assistant","uuid":"a2","parentUuid":"a1",
               "message":{"id":"m1","model":"claude-x","content":[
                   {"type":"text","text":"Looking."},
                   {"type":"tool_use","id":"t1","name":"Read","input":{"path":"a.rs"}}]}}),
        json!({"type":"user","uuid":"u2","parentUuid":"a2",
               "message":{"role":"user","content":[
                   {"type":"tool_result","tool_use_id":"t1","content":"fn main(){}"}]}}),
        json!({"type":"assistant","uuid":"a3","parentUuid":"u2",
               "message":{"id":"m2","content":[{"type":"text","text":"Done."}]}}),
        json!({"type":"custom-title","customTitle":"Bug hunt"}),
    ]
}

#[test]
fn claude_imports_active_branch_and_merges_split_replies() {
    let t = convert_claude(&claude_fixture());
    assert_eq!(t.messages.len(), 4, "{:?}", t.messages);
    assert_eq!(
        t.messages[0],
        json!({"role":"user","content":"fix the bug"})
    );
    assert_eq!(t.messages[1]["content"], "Looking.");
    assert_eq!(t.messages[1]["tool_calls"][0]["id"], "t1");
    assert_eq!(t.messages[1]["tool_calls"][0]["function"]["name"], "Read");
    assert_eq!(
        t.messages[1]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.rs"}"#
    );
    assert_eq!(
        t.messages[2],
        json!({"role":"tool","tool_call_id":"t1","content":"fn main(){}"})
    );
    assert_eq!(t.messages[3]["content"], "Done.");
    assert_eq!(t.title.as_deref(), Some("Bug hunt"));
    assert_eq!(t.model.as_deref(), Some("claude-x"));
    assert_eq!(t.source_id.as_deref(), Some("s1"));
    assert_eq!(t.source_cwd.as_deref(), Some("/work"));
    assert_eq!(t.skipped_summary(), "1 thinking");
}

#[test]
fn claude_counts_sidechain_and_error_records() {
    let records = vec![
        json!({"type":"user","uuid":"u1","message":{"role":"user","content":"hi"}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":true,
               "message":{"id":"x","content":[{"type":"text","text":"sub"}]}}),
        json!({"type":"assistant","uuid":"a2","parentUuid":"a1","isApiErrorMessage":true,
               "message":{"id":"y","content":[{"type":"text","text":"API Error"}]}}),
    ];
    let t = convert_claude(&records);
    assert_eq!(t.messages.len(), 1);
    assert_eq!(
        t.skipped_summary(),
        "1 API error record, 1 sidechain record"
    );
}

fn codex_fixture() -> Vec<Value> {
    vec![
        json!({"type":"session_meta","payload":{"id":"c1","cwd":"/work"}}),
        json!({"type":"turn_context","payload":{"model":"gpt-x"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":"list files"}]}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"list files"}}),
        json!({"type":"response_item","payload":{"type":"reasoning","summary":[]}}),
        json!({"type":"response_item","payload":{"type":"function_call","name":"shell",
               "arguments":"{\"cmd\":\"ls\"}","call_id":"call1"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output",
               "call_id":"call1","output":"a.rs"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
               "content":[{"type":"output_text","text":"Here."}]}}),
        json!({"type":"event_msg","payload":{"type":"agent_message","message":"Here."}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"second"}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":"third"}]}}),
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        json!({"type":"event_msg","payload":{"type":"thread_name_updated","thread_name":"Listing"}}),
    ]
}

#[test]
fn codex_dedupes_events_and_applies_rollback() {
    let t = convert_codex(&codex_fixture());
    let roles: Vec<&str> = t
        .messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant", "user"]);
    assert_eq!(t.messages[0]["content"], "list files");
    assert_eq!(
        t.messages[1]["tool_calls"][0]["function"]["arguments"],
        r#"{"cmd":"ls"}"#
    );
    assert_eq!(t.messages[2]["content"], "a.rs");
    assert_eq!(t.messages[3]["content"], "Here.");
    assert_eq!(t.messages[4]["content"], "second");
    assert_eq!(t.title.as_deref(), Some("Listing"));
    assert_eq!(t.model.as_deref(), Some("gpt-x"));
    assert_eq!(t.source_id.as_deref(), Some("c1"));
    assert_eq!(t.skipped_summary(), "1 reasoning");
}

#[test]
fn codex_compaction_marks_cut_point() {
    let records = vec![
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":"a"}]}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
               "content":[{"type":"output_text","text":"b"}]}}),
        json!({"type":"compacted","payload":{"message":" Summary of a and b. "}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":"c"}]}}),
    ];
    let t = convert_codex(&records);
    assert_eq!(t.messages.len(), 3);
    assert_eq!(
        t.compaction,
        Some(Compaction {
            summary: "Summary of a and b.".into(),
            at: 2
        })
    );
}

#[test]
fn codex_rollback_drops_compaction_past_the_cut() {
    let records = vec![
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":"a"}]}}),
        json!({"type":"compacted","payload":{}}),
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
    ];
    let t = convert_codex(&records);
    assert!(t.messages.is_empty());
    assert_eq!(t.compaction, None);
}

#[test]
fn image_parts_are_counted_not_imported() {
    let records = vec![
        json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_image","image_url":"data:..."},{"type":"input_text","text":"see"}]}}),
    ];
    let t = convert_codex(&records);
    assert_eq!(t.messages, vec![json!({"role":"user","content":"see"})]);
    assert_eq!(t.skipped_summary(), "1 image");
}
