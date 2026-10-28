use super::*;
use serde_json::json;

fn v2(caps: &[&str]) -> Negotiated {
    Negotiated {
        version: 2,
        capabilities: caps.iter().map(|c| (*c).to_owned()).collect(),
    }
}

#[test]
fn missing_or_one_version_stays_on_legacy_protocol_without_capabilities() {
    let legacy = negotiate(&json!({"events": ["pre_tool_use"]})).unwrap();
    assert_eq!(legacy, Negotiated::legacy());
    let one = negotiate(&json!({"protocolVersion": "1", "capabilities": ["ui.status"]})).unwrap();
    assert!(!one.has(CAP_UI_STATUS), "v1 plugins never get v2 features");
}

#[test]
fn version_two_keeps_only_known_capabilities() {
    let n = negotiate(&json!({
        "protocolVersion": "2",
        "capabilities": ["ui.status", "future.thing", 7]
    }))
    .unwrap();
    assert!(n.has(CAP_UI_STATUS));
    assert!(!n.capabilities.contains("future.thing"));
}

#[test]
fn unsupported_or_malformed_version_is_refused() {
    assert!(negotiate(&json!({"protocolVersion": "9"})).is_err());
    assert!(negotiate(&json!({"protocolVersion": 2})).is_err());
}

#[test]
fn actions_require_the_declared_capability() {
    let reply = json!({"actions": [
        {"type": "send_user_message", "text": "hi"},
        {"type": "ui", "slot": "status", "lines": ["ok"]}
    ]});
    let (actions, refused) = parse_actions(&v2(&[CAP_UI_STATUS]), &reply);
    assert_eq!(
        actions,
        vec![Action::Ui(UiSlot {
            slot: Slot::Status,
            lines: vec!["ok".into()]
        })]
    );
    assert_eq!(
        refused.len(),
        1,
        "undeclared message is refused: {refused:?}"
    );
}

#[test]
fn legacy_plugins_cannot_emit_any_action() {
    let reply = json!({"actions": [{"type": "send_user_message", "text": "hi"}]});
    let (actions, refused) = parse_actions(&Negotiated::legacy(), &reply);
    assert!(actions.is_empty());
    assert_eq!(refused.len(), 1);
}

#[test]
fn malformed_action_kinds_are_refused_not_applied() {
    let caps = v2(&[CAP_SET_MODEL, CAP_SEND_USER_MESSAGE]);
    let reply = json!({"actions": [
        {"type": "set_model", "model": "../../etc"},
        {"type": "send_user_message", "text": "   "},
        {"type": "shell", "cmd": "rm -rf /"},
        {"no": "type"}
    ]});
    let (actions, refused) = parse_actions(&caps, &reply);
    assert!(actions.is_empty());
    assert_eq!(refused.len(), 4);
}

#[test]
fn oversized_message_is_refused() {
    let text = "x".repeat(MAX_MESSAGE_BYTES + 1);
    let reply = json!({"actions": [{"type": "send_user_message", "text": text}]});
    let (actions, refused) = parse_actions(&v2(&[CAP_SEND_USER_MESSAGE]), &reply);
    assert!(actions.is_empty());
    assert_eq!(refused.len(), 1);
}

#[test]
fn action_count_is_bounded_per_reply() {
    let items: Vec<Value> = (0..MAX_ACTIONS_PER_REPLY + 5)
        .map(|_| json!({"type": "send_user_message", "text": "hi"}))
        .collect();
    let (actions, refused) =
        parse_actions(&v2(&[CAP_SEND_USER_MESSAGE]), &json!({"actions": items}));
    assert_eq!(actions.len(), MAX_ACTIONS_PER_REPLY);
    assert_eq!(refused.len(), 1, "one notice for the dropped tail");
}

#[test]
fn status_slot_is_one_line_and_footer_is_bounded() {
    let caps = v2(&[CAP_UI_STATUS, CAP_UI_FOOTER]);
    let (_, refused) = parse_actions(
        &caps,
        &json!({"actions": [{"type": "ui", "slot": "status", "lines": ["a", "b"]}]}),
    );
    assert_eq!(refused.len(), 1);
    let footer: Vec<&str> = (0..=MAX_UI_LINES).map(|_| "f").collect();
    let (_, refused) = parse_actions(
        &caps,
        &json!({"actions": [{"type": "ui", "slot": "footer", "lines": footer}]}),
    );
    assert_eq!(refused.len(), 1);
}

#[test]
fn ui_text_loses_control_characters_and_is_clamped() {
    let long = "\u{1b}[31m".to_owned() + &"y".repeat(MAX_UI_LINE_CHARS * 2);
    let (actions, _) = parse_actions(
        &v2(&[CAP_UI_TITLE]),
        &json!({"actions": [{"type": "ui", "slot": "title", "lines": [long]}]}),
    );
    let Action::Ui(slot) = &actions[0] else {
        panic!("expected ui action");
    };
    let line = &slot.lines[0];
    assert!(!line.chars().any(char::is_control));
    assert_eq!(line.chars().count(), MAX_UI_LINE_CHARS);
}

#[test]
fn provider_names_must_be_plain_identifiers() {
    let caps = v2(&[CAP_REGISTER_PROVIDER]);
    let (actions, refused) = parse_actions(
        &caps,
        &json!({"actions": [
            {"type": "register_provider", "name": "acme/gw", "config": {"base_url": "https://x"}},
            {"type": "register_provider", "name": "bad name", "config": {}},
            {"type": "register_provider", "name": "ok", "config": "not-object"}
        ]}),
    );
    assert_eq!(actions.len(), 1);
    assert_eq!(refused.len(), 2);
}

#[test]
fn suggestions_skip_malformed_items_and_are_capped() {
    let items: Vec<Value> = (0..MAX_AUTOCOMPLETE_ITEMS + 3)
        .map(|i| json!({"value": format!("v{i}")}))
        .chain([json!({"label": "no value"}), json!({"value": ""})])
        .collect();
    let out = parse_suggestions(&json!({"items": items}));
    assert_eq!(out.len(), MAX_AUTOCOMPLETE_ITEMS);
    assert_eq!(out[0].label, "v0", "missing label falls back to the value");
}

#[test]
fn valid_name_accepts_identifiers_only() {
    assert!(valid_name("openai/gpt-5.1"));
    assert!(!valid_name(""));
    assert!(!valid_name("a b"));
    assert!(!valid_name(&"n".repeat(MAX_NAME_CHARS + 1)));
}

#[test]
fn append_entry_renderer_is_bounded_and_stripped_of_controls() {
    let reply = json!({"actions": [{
        "type": "append_entry",
        "kind": "build",
        "text": "build ok",
        "renderer": {
            "title": "Build\u{1b}[2J",
            "fields": [{"label": "status", "value": "green\u{7}"}],
            "markdown": "# done\u{1b}]8;;x\u{7}\nbody"
        }
    }]});
    let (actions, refused) = parse_actions(&v2(&["actions.append_entry"]), &reply);
    assert!(refused.is_empty(), "{refused:?}");
    let Action::AppendEntry {
        renderer: Some(view),
        ..
    } = &actions[0]
    else {
        panic!("expected a rendered entry, got {actions:?}");
    };
    assert_eq!(view.title.as_deref(), Some("Build[2J"));
    assert_eq!(view.fields, vec![("status".to_owned(), "green".to_owned())]);
    assert_eq!(view.markdown.as_deref(), Some("# done]8;;x\nbody"));
}

#[test]
fn append_entry_renderer_rejects_oversized_or_malformed_views() {
    let too_many: Vec<Value> = (0..=MAX_RENDER_FIELDS)
        .map(|i| json!({"label": "k", "value": i.to_string()}))
        .collect();
    let cases = [
        json!({"fields": too_many}),
        json!({"markdown": "x".repeat(MAX_RENDER_MARKDOWN_BYTES + 1)}),
        json!("not an object"),
    ];
    for renderer in cases {
        let reply = json!({"actions": [{
            "type": "append_entry",
            "kind": "build",
            "text": "t",
            "renderer": renderer
        }]});
        let (actions, refused) = parse_actions(&v2(&["actions.append_entry"]), &reply);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(refused.len(), 1);
    }
}
