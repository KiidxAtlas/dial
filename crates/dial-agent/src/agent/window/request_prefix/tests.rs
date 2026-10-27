use super::*;
use serde_json::json;

fn chat() -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":"fixed"}),
            json!({"role":"user","content":"hello"}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    }
}

#[test]
fn append_reuses_prefix_and_changes_explain_invalidation() {
    let mut tracker = Tracker::default();
    let mut chat = chat();
    assert_eq!(
        tracker.observe("a", &chat).invalidation,
        Invalidation::Initial
    );
    chat.messages
        .push(json!({"role":"assistant","content":"hello"}));
    let observation = tracker.observe("a", &chat);
    assert_eq!(observation.invalidation, Invalidation::None);
    assert!(observation.stable_message_bytes > 0);
    chat.messages[1]["content"] = json!("summary");
    assert_eq!(
        tracker.observe("a", &chat).invalidation,
        Invalidation::HistoryChanged
    );
    chat.tools.push(json!({"type":"function"}));
    assert_eq!(
        tracker.observe("a", &chat).invalidation,
        Invalidation::ToolsChanged
    );
    assert_eq!(
        tracker.observe("b", &chat).invalidation,
        Invalidation::ModelChanged
    );
    chat.messages[0]["content"] = json!("new instructions");
    assert_eq!(
        tracker.observe("b", &chat).invalidation,
        Invalidation::SystemChanged
    );
}

#[test]
fn an_empty_model_name_is_initial_only_once() {
    let mut tracker = Tracker::default();
    let chat = chat();
    assert_eq!(
        tracker.observe("", &chat).invalidation,
        Invalidation::Initial
    );
    let repeat = tracker.observe("", &chat);
    assert_eq!(repeat.invalidation, Invalidation::None);
    assert_eq!(repeat.stable_message_bytes, repeat.request_message_bytes);
}

#[test]
fn journaled_invalidation_names_are_unchanged() {
    for (value, name) in [
        (Invalidation::Initial, "initial"),
        (Invalidation::ModelChanged, "model_changed"),
        (Invalidation::SystemChanged, "system_changed"),
        (Invalidation::ToolsChanged, "tools_changed"),
        (Invalidation::HistoryChanged, "history_changed"),
        (Invalidation::None, "none"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), json!(name));
        assert_eq!(value.to_string(), name);
    }
}
