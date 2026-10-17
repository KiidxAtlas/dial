use super::*;
use serde_json::json;
#[test]
fn append_reuses_prefix_and_changes_explain_invalidation() {
    let mut tracker = Tracker::default();
    let mut chat = Chat {
        messages: vec![
            json!({"role":"system","content":"fixed"}),
            json!({"role":"user","content":"hello"}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    assert_eq!(tracker.observe("a", &chat).invalidation, "initial");
    chat.messages
        .push(json!({"role":"assistant","content":"hello"}));
    let observation = tracker.observe("a", &chat);
    assert_eq!(observation.invalidation, "none");
    assert!(observation.stable_message_bytes > 0);
    chat.messages[1]["content"] = json!("summary");
    assert_eq!(tracker.observe("a", &chat).invalidation, "history_changed");
    chat.tools.push(json!({"type":"function"}));
    assert_eq!(tracker.observe("a", &chat).invalidation, "tools_changed");
    assert_eq!(tracker.observe("b", &chat).invalidation, "model_changed");
    chat.messages[0]["content"] = json!("new instructions");
    assert_eq!(tracker.observe("b", &chat).invalidation, "system_changed");
}
