use super::*;
use serde_json::json;

fn turn(id: &str, name: &str, args: &str) -> Vec<serde_json::Value> {
    vec![
        json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
        json!({"role":"tool","tool_call_id":id,"content":"same   result"}),
    ]
}

#[test]
fn five_identical_turns_trip_with_the_intent_field_ignored() {
    let mut messages = Vec::new();
    for n in 0..5 {
        messages.extend(turn(
            &format!("c{n}"),
            "read",
            &format!("{{\"path\":\"a\",\"i\":\"step {n}\"}}"),
        ));
    }
    let found =
        RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("five identical turns trip");
    assert_eq!(found.tool_name, "read");
    assert_eq!(found.count, 5);
    assert_eq!(found.result_summary, "same result");
    assert!(RepeatedToolCall::detect(&messages, 6, &["wait"]).is_none());
}

#[test]
fn a_different_turn_or_an_exempt_turn_breaks_the_streak() {
    let mut messages = Vec::new();
    for n in 0..4 {
        messages.extend(turn(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
    }
    messages.extend(turn("x", "read", "{\"path\":\"b\"}"));
    assert!(RepeatedToolCall::detect(&messages, 5, &["wait"]).is_none());

    let mut waits = Vec::new();
    for n in 0..6 {
        waits.extend(turn(&format!("w{n}"), "wait", "{}"));
    }
    assert!(RepeatedToolCall::detect(&waits, 5, &["wait"]).is_none());
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        malformed_arguments: false,
    }
}

#[test]
fn an_identical_repeat_gets_the_earlier_result_and_its_own_call_id() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(turn(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
    }
    let repeat = RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("streak of six");
    assert_eq!(repeat.count, 6);
    let repeated = [call("new", "read", json!({"path": "a", "i": "again"}))];
    let results = repeat.short_circuit(&repeated).expect("identical repeat");
    assert_eq!(results.len(), 1);
    assert!(results[0].starts_with("[Not executed:"));
    assert!(results[0].contains("tool_call_id `c5`"));
    assert!(results[0].contains("`same result`"));
}

#[test]
fn a_different_call_resets_the_repeat_and_runs() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(turn(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
    }
    let repeat = RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("streak of six");
    assert!(
        repeat
            .short_circuit(&[call("x", "read", json!({"path": "b"}))])
            .is_none()
    );
    assert!(
        repeat
            .short_circuit(&[call("x", "grep", json!({"path": "a"}))])
            .is_none()
    );
    let mixed = [
        call("x", "read", json!({"path": "a"})),
        call("y", "read", json!({"path": "b"})),
    ];
    assert!(repeat.short_circuit(&mixed).is_none());
    messages.extend(turn("x", "read", "{\"path\":\"b\"}"));
    assert!(RepeatedToolCall::detect(&messages, 5, &["wait"]).is_none());
}

#[test]
fn malformed_or_empty_calls_are_never_short_circuited() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(turn(&format!("c{n}"), "read", "{\"path\":\"a\"}"));
    }
    let repeat = RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("streak of six");
    let mut malformed = call("x", "read", json!({"path": "a"}));
    malformed.malformed_arguments = true;
    assert!(repeat.short_circuit(&[malformed]).is_none());
    assert!(repeat.short_circuit(&[]).is_none());
}

#[test]
fn nested_intent_keys_are_payload_and_never_form_or_short_circuit_a_repeat() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(turn(
            &format!("c{n}"),
            "query",
            "{\"filter\":{\"i\":1,\"__intent\":\"same\"}}",
        ));
    }
    let repeat =
        RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("identical nested payloads trip");
    assert_eq!(repeat.count, 6);
    // A genuinely different nested payload executes, even though its top-level shape matches.
    assert!(
        repeat
            .short_circuit(&[call(
                "x",
                "query",
                json!({"filter": {"i": 2, "__intent": "same"}})
            )])
            .is_none()
    );
    assert!(
        repeat
            .short_circuit(&[call("x", "query", json!({"filter": {"__intent": "other"}}))])
            .is_none()
    );
    assert!(
        repeat
            .short_circuit(&[call(
                "x",
                "query",
                json!({"filter": {"i": 1, "__intent": "same"}})
            )])
            .is_some()
    );

    let mut varying = Vec::new();
    for n in 0..6 {
        varying.extend(turn(
            &format!("v{n}"),
            "query",
            &format!("{{\"filter\":{{\"i\":{n}}}}}"),
        ));
    }
    assert!(RepeatedToolCall::detect(&varying, 5, &["wait"]).is_none());
}

#[test]
fn top_level_presentation_differences_still_count_as_repeats() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(turn(
            &format!("c{n}"),
            "read",
            &format!("{{\"path\":\"a\",\"__intent\":\"step {n}\"}}"),
        ));
    }
    assert!(RepeatedToolCall::detect(&messages, 5, &["wait"]).is_some());
}

#[test]
fn a_multi_call_repeat_pairs_each_call_with_its_own_earlier_result() {
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(
            json!([
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":format!("r{n}"),"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}},
                    {"id":format!("g{n}"),"type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"x\"}"}}]},
                {"role":"tool","tool_call_id":format!("r{n}"),"content":"file text"},
                {"role":"tool","tool_call_id":format!("g{n}"),"content":"grep hits"}
            ])
            .as_array()
            .unwrap()
            .clone(),
        );
    }
    let repeat = RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("streak of six");
    let reordered = [
        call("g", "grep", json!({"pattern": "x"})),
        call("r", "read", json!({"path": "a"})),
    ];
    let results = repeat.short_circuit(&reordered).expect("same call set");
    assert!(results[0].contains("`grep`") && results[0].contains("tool_call_id `g5`"));
    assert!(results[0].contains("`grep hits`"));
    assert!(results[1].contains("tool_call_id `r5`") && results[1].contains("`file text`"));
}
