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
fn only_requests_that_break_the_prefix_count_as_resets() {
    let mut tracker = Tracker::default();
    let mut resets = std::collections::BTreeMap::new();
    let mut chat = chat();
    tracker
        .observe("a", &chat)
        .invalidation
        .count_reset(&mut resets);
    chat.messages
        .push(json!({"role":"assistant","content":"hello"}));
    tracker
        .observe("a", &chat)
        .invalidation
        .count_reset(&mut resets);
    assert!(resets.is_empty(), "{resets:?}");
    chat.tools.push(json!({"type":"function"}));
    tracker
        .observe("a", &chat)
        .invalidation
        .count_reset(&mut resets);
    tracker
        .observe("b", &chat)
        .invalidation
        .count_reset(&mut resets);
    tracker
        .observe("c", &chat)
        .invalidation
        .count_reset(&mut resets);
    assert_eq!(
        resets,
        std::collections::BTreeMap::from([("model_changed", 2), ("tools_changed", 1)])
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

#[test]
fn prefix_measurements_partition_bytes_without_retaining_content() {
    let mut tracker = Tracker::default();
    let mut request = chat();
    request
        .messages
        .insert(1, json!({"role":"developer","content":"private policy"}));
    request
        .tools
        .push(json!({"type":"function","function":{"name":"read"}}));
    let initial = tracker.observe("model", &request);
    assert_eq!(initial.stable_message_count, 0);
    assert_eq!(initial.changed_message_bytes, initial.request_message_bytes);
    assert_eq!(
        initial.system_bytes,
        request.messages[..2]
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap().len())
            .sum::<usize>()
    );
    assert_eq!(
        initial.tool_schema_bytes,
        serde_json::to_vec(&request.tools).unwrap().len()
    );
    assert_eq!(initial.tool_count, 1);
    request
        .messages
        .push(json!({"role":"assistant","content":"answer"}));
    let appended = tracker.observe("model", &request);
    assert_eq!(appended.stable_message_count, 3);
    assert_eq!(appended.stable_message_bytes, initial.request_message_bytes);
    assert_eq!(
        appended.changed_message_bytes,
        serde_json::to_vec(request.messages.last().unwrap())
            .unwrap()
            .len()
    );
    request.messages[2]["content"] = json!("corrected question");
    let corrected = tracker.observe("model", &request);
    assert_eq!(corrected.invalidation, Invalidation::HistoryChanged);
    assert_eq!(corrected.stable_message_count, 2);
    assert_eq!(
        corrected.stable_message_bytes + corrected.changed_message_bytes,
        corrected.request_message_bytes
    );
    request.messages[1]["content"] = json!("corrected private policy");
    let policy = tracker.observe("model", &request);
    assert_eq!(policy.invalidation, Invalidation::SystemChanged);
    assert_eq!(policy.stable_message_count, 0);
    assert_eq!(policy.stable_message_bytes, 0);
    let report = serde_json::to_string(&policy).unwrap();
    assert!(!report.contains("private policy"));
    assert!(!report.contains("corrected question"));
}

#[test]
fn empty_and_shortened_requests_have_bounded_prefix_measurements() {
    let mut tracker = Tracker::default();
    let mut request = chat();
    tracker.observe("model", &request);
    request.messages.pop();
    let shorter = tracker.observe("model", &request);
    assert_eq!(shorter.invalidation, Invalidation::HistoryChanged);
    assert_eq!(shorter.stable_message_count, 1);
    assert_eq!(shorter.changed_message_bytes, 0);
    request.messages.clear();
    let empty = tracker.observe("model", &request);
    assert_eq!(empty.stable_message_count, 0);
    assert_eq!(empty.changed_message_bytes, 0);
    assert_eq!(empty.system_bytes, 0);
    assert_eq!(empty.tool_schema_bytes, 2);
    assert_eq!(empty.tool_count, 0);
}

#[test]
fn reusable_digest_buffer_preserves_exact_json_identity() {
    let mut scratch = Vec::new();
    for value in [
        json!({"role":"tool","content":"large context".repeat(4096)}),
        json!({"role":"user","content":"Unicode: λ 🦀; escaped: \" \\ \n"}),
        json!([]),
        json!({"content":[{"type":"text","text":"hook replacement"}]}),
    ] {
        let expected = serde_json::to_vec(&value).unwrap();
        let (hash, bytes) = digest(&value, &mut scratch);
        assert_eq!(bytes, expected.len());
        assert_eq!(hash.as_slice(), Sha256::digest(&expected).as_slice());
    }
}

/// Matched offline preparation microbenchmark; no provider or model inference.
#[test]
#[ignore = "manual request-prefix microbenchmark"]
fn request_prefix_preparation_benchmark() {
    let mut request = chat();
    let text = "source context: \"quoted\" \\ path\n".repeat(1024);
    for index in 0..128 {
        request
            .messages
            .push(json!({"role":"tool","content":text,"tool_call_id":index.to_string()}));
    }
    let mut tracker = Tracker::default();
    tracker.observe("model", &request);
    let mut samples = Vec::new();
    for _ in 0..31 {
        let start = std::time::Instant::now();
        std::hint::black_box(tracker.observe("model", std::hint::black_box(&request)));
        samples.push(start.elapsed());
    }
    samples.sort();
    eprintln!(
        "request-prefix messages={} serialized_bytes={} median_us={} p95_us={}",
        request.messages.len(),
        serde_json::to_vec(&request.messages).unwrap().len(),
        samples[15].as_micros(),
        samples[29].as_micros()
    );
}
