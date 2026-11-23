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
    assert!(results[0].text.starts_with("[Not executed:"));
    assert!(results[0].text.contains("tool_call_id `c5`"));
    assert!(results[0].text.contains("`same result`"));
    assert_eq!(results[0].call_id, "c5");
}

/// Assistant call plus a tool answer carrying a typed origin (`null` when the call executed).
fn typed_turn(
    id: &str,
    name: &str,
    args: &str,
    content: &str,
    origin: serde_json::Value,
) -> Vec<serde_json::Value> {
    vec![
        json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
        json!({"role":"tool","tool_call_id":id,"content":content,"dial_result_origin":origin}),
    ]
}

fn duplicate_of(target: &str) -> serde_json::Value {
    json!({"kind":"duplicate","call_id":target})
}

/// The executed id the guard resolves for the newest read of an identical streak, and its text.
/// `None` means no recoverable executed result exists.
fn quoted_for_streak(messages: &[serde_json::Value]) -> (Option<String>, String) {
    let repeat = RepeatedToolCall::detect(messages, 5, &["wait"]).expect("streak of five");
    let quoted = repeat
        .earlier
        .iter()
        .find(|call| call.name == "read")
        .expect("read call");
    (quoted.id.clone(), quoted.result.clone())
}

/// Whether a fresh identical read would be answered by a duplicate short circuit.
fn short_circuits(messages: &[serde_json::Value]) -> bool {
    RepeatedToolCall::detect(messages, 5, &["wait"])
        .expect("streak of five")
        .short_circuit(&[call("new", "read", json!({"path": "a"}))])
        .is_some()
}

#[test]
fn a_typed_duplicate_follows_its_reference_to_the_executed_result() {
    let read = "{\"path\":\"a\"}";
    let mut messages = typed_turn("target", "read", read, "executed A", json!(null));
    // The target lies before a different call, so it is outside the identical streak.
    messages.extend(turn("x", "read", "{\"path\":\"b\"}"));
    for n in 1..5 {
        messages.extend(typed_turn(
            &format!("c{n}"),
            "read",
            read,
            "[Not executed: identical to your earlier ...]",
            duplicate_of("target"),
        ));
    }
    messages.extend(typed_turn(
        "c5",
        "read",
        read,
        "[Not executed: identical to your earlier ...]",
        duplicate_of("target"),
    ));
    assert_eq!(
        quoted_for_streak(&messages),
        (Some("target".to_owned()), "executed A".to_owned())
    );
    let answer = RepeatedToolCall::detect(&messages, 5, &["wait"])
        .expect("streak of five")
        .short_circuit(&[call("new", "read", json!({"path": "a"}))])
        .expect("recoverable target answers the repeat");
    assert_eq!(answer[0].call_id, "target");
}

#[test]
fn a_typed_duplicate_never_becomes_an_execution_without_a_usable_target() {
    let read = "{\"path\":\"a\"}";
    let dup = "[Not executed: identical to your earlier ...]";
    // Table: the reference is missing, mismatched in signature, or names an unexecuted answer.
    // None may become an executed target, and none may short circuit into a new quote.
    let cases: Vec<(&str, Vec<serde_json::Value>)> = vec![
        ("missing target", {
            let mut m = Vec::new();
            for n in 0..6 {
                m.extend(typed_turn(
                    &format!("c{n}"),
                    "read",
                    read,
                    dup,
                    duplicate_of("gone"),
                ));
            }
            m
        }),
        ("mismatched signature", {
            let mut m = typed_turn("other", "read", "{\"path\":\"b\"}", "B result", json!(null));
            for n in 0..6 {
                m.extend(typed_turn(
                    &format!("c{n}"),
                    "read",
                    read,
                    dup,
                    duplicate_of("other"),
                ));
            }
            m
        }),
        ("unexecuted target", {
            let mut m = typed_turn(
                "skip",
                "read",
                read,
                "This call was not executed",
                json!({"kind":"skipped"}),
            );
            for n in 0..6 {
                m.extend(typed_turn(
                    &format!("c{n}"),
                    "read",
                    read,
                    dup,
                    duplicate_of("skip"),
                ));
            }
            m
        }),
    ];
    for (case, messages) in cases {
        let (id, text) = quoted_for_streak(&messages);
        assert_eq!(id, None, "{case}: no synthetic executed target");
        assert_eq!(text, dup, "{case}: diagnostic only");
        assert!(!short_circuits(&messages), "{case}: the call must run");
    }
}

#[test]
fn typed_origin_decides_over_prose_for_new_records() {
    // A typed executed record whose text looks like a duplicate answer is still an execution.
    let read = "{\"path\":\"a\"}";
    let dup = "[Not executed: identical to your earlier ...]";
    let mut messages = Vec::new();
    for n in 0..5 {
        messages.extend(typed_turn(&format!("c{n}"), "read", read, dup, json!(null)));
    }
    messages.extend(typed_turn("c5", "read", read, dup, json!(null)));
    assert_eq!(
        quoted_for_streak(&messages),
        (Some("c5".to_owned()), dup.to_owned())
    );

    // A typed unexecuted record with ordinary text is not quoted as an execution.
    let mut messages = Vec::new();
    for n in 0..6 {
        messages.extend(typed_turn(
            &format!("c{n}"),
            "read",
            read,
            "plain text",
            json!({"kind":"skipped"}),
        ));
    }
    assert_eq!(quoted_for_streak(&messages).0, None);
    assert!(!short_circuits(&messages));
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
    assert!(results[0].text.contains("`grep`") && results[0].text.contains("tool_call_id `g5`"));
    assert!(results[0].text.contains("`grep hits`"));
    assert!(
        results[1].text.contains("tool_call_id `r5`") && results[1].text.contains("`file text`")
    );
}

#[test]
fn the_quoted_result_skips_references_and_barrier_refusals_that_ran_nothing() {
    let reply = |id: &str, content: &str| {
        vec![
            json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"bash","arguments":"{\"command\":\"python3 -m venv.venv\"}"}}]}),
            json!({"role":"tool","tool_call_id":id,"content":content}),
        ]
    };
    let mut messages = reply("c0", "python3: No module named venv.venv");
    for n in 1..5 {
        messages.extend(reply(
            &format!("c{n}"),
            "invalid tool call: Failed-operation retry barrier: this unchanged `bash` call already failed and was not executed again.",
        ));
    }
    let stub = crate::agent::safety::stream_guards::render_duplicate_call_result(
        "bash",
        "c4",
        "invalid tool call: Failed-operation retry barrier: …",
    );
    messages.extend(reply("c5", &stub));
    let repeat = RepeatedToolCall::detect(&messages, 5, &["wait"]).expect("streak of six");
    assert_eq!(repeat.result_summary, "python3: No module named venv.venv");
    let results = repeat
        .short_circuit(&[call(
            "new",
            "bash",
            json!({"command": "python3 -m venv.venv"}),
        )])
        .expect("identical repeat");
    assert!(
        results[0].text.contains("tool_call_id `c0`"),
        "{}",
        results[0].text
    );
    assert!(
        results[0].text.contains("No module named venv.venv"),
        "{}",
        results[0].text
    );
    assert_eq!(
        results[0].text.matches("[Not executed").count(),
        1,
        "{}",
        results[0].text
    );

    // With no executed result in the streak, the newest one is still quoted.
    let mut only_refusals = Vec::new();
    for n in 0..5 {
        only_refusals.extend(reply(
            &format!("r{n}"),
            "invalid tool call: Failed-operation retry barrier: x",
        ));
    }
    let repeat = RepeatedToolCall::detect(&only_refusals, 5, &["wait"]).expect("streak of five");
    assert!(repeat.result_summary.starts_with("invalid tool call:"));
}

#[test]
fn alternating_identical_call_sets_are_a_cycle_once_three_full_cycles_repeat() {
    let alternate = |rounds: usize| {
        let mut messages = Vec::new();
        for n in 0..rounds {
            let (name, args) = if n % 2 == 0 {
                ("read", "{\"path\":\"a\"}")
            } else {
                ("bash", "{\"command\":\"cargo test\"}")
            };
            messages.extend(turn(&format!("c{n}"), name, args));
        }
        messages
    };
    assert!(CallCycle::detect(&alternate(5), &["wait"]).is_none());
    let cycle = CallCycle::detect(&alternate(6), &["wait"]).expect("A,B three times");
    assert_eq!(cycle.period, 2);
    assert_eq!(cycle.turns, 6);
    assert_eq!(cycle.tools, ["bash", "read"]);
    assert!(cycle.guidance().contains("bash → read"));
    // The identical streak and a break in the pattern are not cycles.
    let mut same = Vec::new();
    for n in 0..6 {
        same.extend(turn(&format!("s{n}"), "read", "{\"path\":\"a\"}"));
    }
    assert!(CallCycle::detect(&same, &["wait"]).is_none());
    let mut broken = alternate(6);
    broken.extend(turn("x", "read", "{\"path\":\"b\"}"));
    assert!(CallCycle::detect(&broken, &["wait"]).is_none());
}

#[test]
fn a_three_step_cycle_is_found_but_progressing_arguments_are_not() {
    let mut messages = Vec::new();
    for n in 0..9 {
        let path = ["a", "b", "c"][n % 3];
        messages.extend(turn(
            &format!("c{n}"),
            "read",
            &format!("{{\"path\":\"{path}\"}}"),
        ));
    }
    let cycle = CallCycle::detect(&messages, &["wait"]).expect("A,B,C three times");
    assert_eq!(cycle.period, 3);
    let mut paging = Vec::new();
    for n in 0..9 {
        paging.extend(turn(
            &format!("p{n}"),
            "read",
            &format!("{{\"path\":\"a\",\"offset\":{}}}", n * 100),
        ));
    }
    assert!(CallCycle::detect(&paging, &["wait"]).is_none());
}

fn turn_with(id: &str, name: &str, args: &str, result: &str) -> Vec<serde_json::Value> {
    vec![
        json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
        json!({"role":"tool","tool_call_id":id,"content":result}),
    ]
}

/// Oldest first: `A` is the same test run, the other letters are distinct reads.
fn irregular(pattern: &str, result_of_a: impl Fn(usize) -> String) -> Vec<serde_json::Value> {
    let mut messages = Vec::new();
    for (n, step) in pattern.chars().enumerate() {
        messages.extend(if step == 'A' {
            turn_with(
                &format!("c{n}"),
                "bash",
                "{\"command\":\"cargo test\"}",
                &result_of_a(n),
            )
        } else {
            turn_with(
                &format!("c{n}"),
                "read",
                &format!("{{\"path\":\"{step}\"}}"),
                "contents",
            )
        });
    }
    messages
}

#[test]
fn an_identical_call_and_result_recurring_irregularly_is_reported() {
    let failing = |_| "test a ... FAILED".to_owned();
    let messages = irregular("ABACAADAEA", failing);
    assert!(RepeatedToolCall::detect(&messages, 5, &["wait"]).is_none());
    assert!(CallCycle::detect(&messages, &["wait"]).is_none());
    let found = RecurringCall::detect(&messages, &["wait"]).expect("six of ten");
    assert_eq!(
        (found.tool_name.as_str(), found.count, found.turns),
        ("bash", 6, 10)
    );
    assert!(found.guidance().contains("recurring_tool_call"));
    assert!(found.guidance().contains("cargo"));
    // Five occurrences, or an older sixth outside the ten-turn window, are not a recurrence.
    assert!(RecurringCall::detect(&irregular("ABACADAEAF", failing), &["wait"]).is_none());
    assert!(RecurringCall::detect(&irregular("AABACADAEAF", failing), &["wait"]).is_none());
}

#[test]
fn a_rerun_whose_result_changes_or_an_exempt_tool_is_not_a_recurrence() {
    let progressing = irregular("ABACAADAEA", |n| format!("{n} tests failed"));
    assert!(RecurringCall::detect(&progressing, &["wait"]).is_none());
    let mut waits = Vec::new();
    for n in 0..10 {
        let (name, args) = if n % 3 == 0 {
            ("read", format!("{{\"path\":\"{n}\"}}"))
        } else {
            ("wait", "{}".to_owned())
        };
        waits.extend(turn_with(&format!("w{n}"), name, &args, "same"));
    }
    assert!(RecurringCall::detect(&waits, &["wait"]).is_none());
}

#[test]
fn recurrence_requires_observed_results_in_the_current_user_turn() {
    let history = irregular("ABACAADAEA", |_| "same".into());
    let mut new_turn = history.clone();
    new_turn.push(json!({"role":"user","content":"Run the check again"}));
    new_turn.extend(turn_with(
        "new",
        "bash",
        "{\"command\":\"cargo test\"}",
        "same",
    ));
    assert!(RecurringCall::detect(&new_turn, &["wait"]).is_none());

    let missing: Vec<_> = history
        .iter()
        .filter(|m| m["role"] != "tool")
        .cloned()
        .collect();
    assert!(RecurringCall::detect(&missing, &["wait"]).is_none());

    let mut transient = history.clone();
    transient.push(json!({"role":"user","content":"harness notice","dial_transient":true}));
    assert!(RecurringCall::detect(&transient, &["wait"]).is_some());

    let mut synthetic = history;
    for message in &mut synthetic {
        if message["role"] == "tool" {
            message["content"] = json!(crate::agent::prompts::recovery::DUPLICATE_RESULT_LEAD);
        }
    }
    assert!(RecurringCall::detect(&synthetic, &["wait"]).is_none());
}

#[test]
fn recurrence_compares_full_results_and_counts_a_batch_once() {
    let prefix = "x".repeat(1000);
    let changed = irregular("ABACAADAEA", |n| format!("{prefix}{n}"));
    assert!(RecurringCall::detect(&changed, &["wait"]).is_none());
    let mut batch = turn("first", "read", "{\"path\":\"a\"}");
    let call = batch[0]["tool_calls"][0].clone();
    batch[0]["tool_calls"] = json!(vec![call; 6]);
    assert!(RecurringCall::detect(&batch, &["wait"]).is_none());
}
