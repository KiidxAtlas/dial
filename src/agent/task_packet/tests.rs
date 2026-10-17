use super::*;

fn tasks() -> TodoList {
    let mut todo = TodoList::default();
    todo.restore(&json!([{"content":"repair duplicate handling","status":"in_progress"}]))
        .unwrap();
    todo
}

#[test]
fn a_new_instruction_rebuilds_the_packet_without_erasing_compatible_history() {
    let mut cache = PacketCache::default();
    let mut control =
        json!({"original_goal":"write the feature", "current_instruction":"inspect first"});
    let before = cache
        .project(&mut [], &tasks(), &control, "s", None, "check", 4096)
        .unwrap();
    control["current_instruction"] = json!("Stop editing and explain the current results");
    let after = cache
        .project(&mut [], &tasks(), &control, "s", None, "check", 4096)
        .unwrap();
    assert_ne!(before, after);
    let packet: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        packet["current_instruction_excerpt"],
        control["current_instruction"]
    );
    assert_eq!(packet["original_goal"], control["original_goal"]);
    assert!(
        packet["authority"]
            .as_str()
            .unwrap()
            .contains("latest user instruction takes precedence")
    );
}
fn history() -> Vec<Value> {
    let mut messages = vec![
        json!({"role":"user","content":"Never modify public APIs. Preserve Unicode."}),
        json!({"role":"assistant","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":"source".repeat(600),"dial_evidence":{"artifact":"artifact://old","observation":"src/lib.rs"}}),
    ];
    for i in 0..12 {
        messages.push(json!({"role":"assistant","content":format!("recent {i}")}));
    }
    messages
}

#[test]
fn projected_task_numbers_and_completion_match_todo_contract() {
    let mut todo = TodoList::new();
    assert!(!todo.call(&json!({"op":"set","tasks":[{"content":"Inspect","investigation":true},{"content":"Implement"}]})).is_error);
    let mut cache = PacketCache::default();
    let first = cache
        .project(&mut [], &todo, &json!({}), "s", None, "check", 4096)
        .unwrap();
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["active_task"]["index"], 1);
    assert_eq!(first["completion"]["op"], "done");
    assert_eq!(first["completion"], json!({"op":"done"}));
    assert!(!todo.call(&first["completion"]).is_error);
    let second = cache
        .project(&mut [], &todo, &json!({}), "s", None, "check", 4096)
        .unwrap();
    let second: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(second["active_task"]["index"], 2);
    assert_eq!(second["completion"], json!({"op":"done"}));
    assert!(!todo.call(&second["completion"]).is_error);
    assert!(!todo.has_open());
}

#[test]
fn focuses_old_bodies_without_losing_constraints_pairing_or_history() {
    let original = history();
    let mut messages = original.clone();
    let mut cache = PacketCache::default();
    let packet = cache
        .project(
            &mut messages,
            &tasks(),
            &json!({"original_goal":"fix the regression"}),
            "session",
            Some("/state/session.jsonl"),
            "duplicate-key test",
            2048,
        )
        .unwrap();
    assert!(packet.len() <= 2048);
    assert!(packet.contains("/state/session.jsonl"));
    assert_eq!(messages[0], original[0]);
    assert_eq!(messages[1], original[1]);
    assert_eq!(messages[2]["tool_call_id"], "old");
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("artifact://old")
    );
    assert!(
        messages[2]["content"].as_str().unwrap().len()
            < original[2]["content"].as_str().unwrap().len()
    );
    assert_eq!(&messages[3..], &original[3..]);
    assert_eq!(original[2]["content"], "source".repeat(600));
}

#[test]
fn packet_and_placeholders_are_stable_until_task_state_changes() {
    let mut cache = PacketCache::default();
    let mut first = history();
    let packet = cache
        .project(&mut first, &tasks(), &json!({}), "s", None, "check", 2048)
        .unwrap();
    let mut second = history();
    second.push(json!({"role":"tool","tool_call_id":"new","content":"new evidence","dial_evidence":{"artifact":"artifact://new"}}));
    assert_eq!(
        cache.project(&mut second, &tasks(), &json!({}), "s", None, "check", 2048),
        Some(packet)
    );
    assert_eq!(first[2], second[2]);
}

#[test]
fn does_not_hide_mutations_errors_or_nonretrievable_evidence() {
    for mode in ["mutation", "error", "no_artifact"] {
        let mut messages = history();
        match mode {
            "mutation" => messages[1]["tool_calls"][0]["function"]["name"] = json!("edit"),
            "error" => {
                messages[2]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("observation");
            }
            _ => {
                messages[2]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("artifact");
            }
        }
        let original = messages[2].clone();
        PacketCache::default().project(
            &mut messages,
            &tasks(),
            &json!({}),
            "s",
            None,
            "check",
            2048,
        );
        assert_eq!(messages[2], original);
    }
}

#[test]
fn automatic_index_is_small_unique_and_preserves_explicit_evidence() {
    let mut messages = history();
    for i in 0..20 {
        messages.push(json!({"role":"tool","tool_call_id":format!("call_{i}"),
            "content":"source excerpt ".repeat(100),
            "dial_evidence":{"artifact":format!("artifact://{i:064}"),"observation":"src/lib.rs"}}));
    }
    // Duplicate the newest artifact: it must occupy only one slot.
    messages.push(messages.last().unwrap().clone());
    let original = messages.clone();
    let control = json!({"working_state":{"constraints":["preserve APIs"],
        "evidence":["artifact://explicit"]},"original_goal":"fix the regression"});
    let packet = PacketCache::default()
        .project(
            &mut messages,
            &tasks(),
            &control,
            "session",
            None,
            "check",
            4096,
        )
        .unwrap();
    let parsed: Value = serde_json::from_str(&packet).unwrap();
    let evidence = parsed["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), RECENT_EVIDENCE + 1);
    assert_eq!(evidence[0]["reference"], "artifact://explicit");
    assert_eq!(evidence[1]["reference"], format!("artifact://{:064}", 19));
    assert!(evidence.iter().all(|item| item.get("excerpt").is_none()));
    assert!(
        parsed["constraints"]
            .as_array()
            .unwrap()
            .contains(&json!("preserve APIs"))
    );
    assert_eq!(parsed["original_goal"], "fix the regression");
    // The newest results and original user message are untouched.
    assert_eq!(messages[0], original[0]);
    assert_eq!(
        &messages[messages.len() - RECENT_MESSAGES..],
        &original[original.len() - RECENT_MESSAGES..]
    );
    assert!(packet.len() < 1600, "packet bytes: {}", packet.len());
    // Reconstruct the previous fill-to-budget index using this exact fixture for comparison.
    let mut previous = parsed.clone();
    previous["evidence"] = json!([{"reference":"artifact://explicit"}]);
    for message in original.iter().rev() {
        let Some(reference) = message["dial_evidence"]["artifact"].as_str() else {
            continue;
        };
        let item = json!({"tool_call_id":message["tool_call_id"],"reference":reference,
            "excerpt":clipped(message["content"].as_str().unwrap_or(""), 160)});
        if !add_bounded(&mut previous, "evidence", item, 4096) {
            break;
        }
    }
    let old_bytes = previous.to_string().len();
    eprintln!("task packet fixture: {old_bytes} -> {} bytes", packet.len());
    assert!(packet.len() * 2 < old_bytes);
}

#[test]
fn bounds_utf8_and_small_budgets_without_touching_history() {
    let mut messages = history();
    let original = messages.clone();
    assert!(
        PacketCache::default()
            .project(&mut messages, &tasks(), &json!({}), "s", None, "check", 20)
            .is_none()
    );
    assert_eq!(messages, original);
    let packet = PacketCache::default()
        .project(
            &mut messages,
            &tasks(),
            &json!({"working_state":{"constraints":["é".repeat(10000)]}}),
            "s",
            None,
            "check",
            1024,
        )
        .unwrap();
    assert!(packet.len() <= 1024);
    assert!(serde_json::from_str::<Value>(&packet).is_ok());
}
