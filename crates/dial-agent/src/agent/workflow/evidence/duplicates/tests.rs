use super::*;

fn exchange(id: &str, tool: &str) -> Vec<Value> {
    vec![
        json!({"role":"assistant","tool_calls":[{"id":id,"function":{"name":tool,"arguments":"{\"pattern\":\"needle\"}"}}]}),
        json!({"role":"tool","tool_call_id":id,"content":"needle: original evidence\n".repeat(400),"dial_evidence":{"artifact":format!("artifact://{id}"),"is_error":false}}),
    ]
}

fn calm() -> Policy {
    Policy {
        warm_tokens: 0,
        pressure: false,
        protect_tokens: usize::MAX,
        supersede_reads: false,
        drop_useless: false,
    }
}

fn project_early(messages: &[Value], memo: &mut Supersession) -> Vec<Value> {
    project_duplicates(messages, memo);
    let mut projected = messages.to_vec();
    project(&mut projected, memo, &calm());
    projected
}

#[test]
fn appended_repeat_shrinks_before_pressure_without_rewriting_the_sent_prefix() {
    let mut history = exchange("first", "grep");
    let original = history.clone();
    let mut memo = Supersession::default();
    let first = project_early(&history, &mut memo);
    history.extend(exchange("repeat", "grep"));
    let before = history.clone();
    let second = project_early(&history, &mut memo);
    assert_eq!(
        &second[..first.len()],
        first,
        "previously sent prefix must not change"
    );
    assert_eq!(history, before, "durable input must remain exact");
    assert_eq!(&history[..2], original);
    let text = second[3]["content"].as_str().unwrap();
    assert!(
        text.contains("Exact repeat")
            && text.contains("first")
            && text.contains("artifact://repeat"),
        "{text}"
    );
    assert_eq!(second[3]["tool_call_id"], "repeat");
    assert_eq!(second[2], crate::llm::messages::canonical(&history[2]));
    assert!(memo.holds_full("first"));
    assert!(
        !memo.holds_full("repeat"),
        "pointers cannot supply source witnesses"
    );
    let before_tokens: usize = before.iter().map(estimate).sum();
    let after_tokens: usize = second.iter().map(estimate).sum();
    assert!(
        after_tokens < before_tokens * 3 / 4,
        "before={before_tokens}, after={after_tokens}"
    );
    assert_eq!(project_early(&history, &mut memo), second);
    // Switching efficiency off still reapplies already-issued pointers through the existing owner.
    let mut disabled = history.clone();
    project(&mut disabled, &mut memo, &calm());
    assert_eq!(disabled, second);
}

#[test]
fn only_exact_successful_plain_read_only_pairs_are_eligible() {
    for case in [
        "arguments",
        "content",
        "error",
        "unknown",
        "status",
        "missing_success",
        "artifact",
        "image",
        "replay",
        "assistant_replay",
        "mutation",
        "unpaired",
        "small",
        "retrieval",
        "retrieval_many",
        "tagged",
    ] {
        let tool = if case == "mutation" { "edit" } else { "grep" };
        let mut history = exchange("first", tool);
        history.extend(exchange("repeat", tool));
        match case {
            "arguments" => {
                history[2]["tool_calls"][0]["function"]["arguments"] =
                    json!("{\"pattern\":\"different\"}")
            }
            "content" => history[3]["content"] = json!("different".repeat(1500)),
            "error" => history[3]["is_error"] = json!(true),
            "unknown" => history[3]["dial_execution"] = json!({"effects":"unknown"}),
            "status" => history[3]["dial_execution_status"] = json!("blocked"),
            "missing_success" => {
                history[3]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("is_error");
            }
            "artifact" => {
                history[3]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("artifact");
            }
            "image" => history[3]["dial_images"] = json!([{"path":"frame.png"}]),
            "replay" => history[3]["dial_provider_content"] = json!([]),
            "assistant_replay" => history[2]["dial_provider_content"] = json!([]),
            "unpaired" => history[2]["tool_calls"] = json!([]),
            "small" => {
                history[1]["content"] = json!("tiny");
                history[3]["content"] = json!("tiny");
            }
            "retrieval" => {
                for index in [0, 2] {
                    history[index]["tool_calls"][0]["function"] =
                        json!({"name":"read","arguments":"{\"path\":\"artifact://original\"}"});
                }
            }
            "retrieval_many" => {
                for index in [0, 2] {
                    history[index]["tool_calls"][0]["function"] = json!({
                        "name":"read",
                        "arguments":{"paths":["src/a.rs", "artifact://original"]}
                    });
                }
            }
            "tagged" => {
                history[1]["dial_evidence"]["observation"] = json!("resource");
                history[3]["dial_evidence"]["observation"] = json!("resource");
            }
            _ => {}
        }
        let projected = project_early(&history, &mut Supersession::default());
        let expected: Vec<_> = history
            .iter()
            .map(crate::llm::messages::canonical)
            .collect();
        assert_eq!(projected, expected, "must preserve {case}");
    }
}

#[test]
fn frozen_pointers_survive_pressure_and_anchor_removal_without_reclassifying_old_results() {
    let mut history = exchange("first", "grep");
    history.extend(exchange("repeat", "grep"));
    let mut memo = Supersession::default();
    let early = project_early(&history, &mut memo);
    let mut pressure = history.clone();
    project(
        &mut pressure,
        &mut memo,
        &Policy {
            pressure: true,
            protect_tokens: 0,
            drop_useless: true,
            supersede_reads: true,
            ..calm()
        },
    );
    assert_eq!(
        pressure[3], early[3],
        "pressure must not rewrite a frozen repeat pointer"
    );
    let restored = project_early(&history[2..], &mut memo);
    assert_eq!(
        restored[1], early[3],
        "pointer remains retrievable without its anchor in view"
    );
    let mut seen = Supersession::default();
    let second_alone = project_early(&history[2..], &mut seen);
    let later = project_early(&history, &mut seen);
    assert_eq!(
        later[3], second_alone[1],
        "do not rewrite an already-seen full result"
    );
}
