use super::*;
use crate::agent::prompts::modes::goal::INTENT_POLICY;

fn record_turn(control: &mut Value, text: &str, index: usize) {
    record(control, text, json!(format!("turn-{index}")), Kind::Turn);
}

#[test]
fn latest_keeps_raw_text_and_earlier_messages_stay_ordered_across_json_roundtrip() {
    let raw = "fix \"parser\"\n\tuse \\ path 🚀 ünïcode";
    let mut control = json!({});
    record_turn(&mut control, "first ✓", 0);
    record(&mut control, raw, json!("entry-raw"), Kind::Steering);

    let encoded = serde_json::to_string(&control["user_intent"]).unwrap();
    let decoded: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded["latest"]["text"], raw);
    assert_eq!(decoded["latest"]["entry_id"], "entry-raw");
    assert_eq!(decoded["latest"]["kind"], "user_steering");
    assert_eq!(decoded["earlier"][0]["text"], "first ✓");
    assert_eq!(decoded["earlier"][0]["kind"], "user_turn");
    assert_eq!(decoded["omitted_messages"], 0);
}

#[test]
fn legacy_effective_instruction_bootstraps_the_trail_once_as_unknown_context() {
    let mut control = json!({
        "current_instruction":"old drafted prompt",
        "current_instruction_entry_id":"legacy-entry"
    });
    record(&mut control, "new request", json!("new-entry"), Kind::Turn);
    let intent = &control["user_intent"];
    // Legacy text has unknown provenance: it lives in legacy_context, not the admitted trail.
    assert_eq!(intent["legacy_context"]["text"], "old drafted prompt");
    assert_eq!(intent["legacy_context"]["entry_id"], "legacy-entry");
    assert_eq!(
        intent["legacy_context"]["provenance"],
        "unknown_effective_instruction"
    );
    assert_eq!(intent["earlier"], json!([]));
    assert_eq!(intent["latest"]["text"], "new request");

    // Once a user message has been admitted, the legacy context is carried, not re-bootstrapped.
    record(&mut control, "third", json!("third-entry"), Kind::Turn);
    assert_eq!(
        control["user_intent"]["legacy_context"]["text"],
        "old drafted prompt"
    );
    assert_eq!(control["user_intent"]["earlier"][0]["text"], "new request");

    let mut bare = json!({});
    record_turn(&mut bare, "first", 0);
    assert_eq!(bare["user_intent"]["earlier"], json!([]));
    assert!(bare["user_intent"]["legacy_context"].is_null());
}

#[test]
fn oversized_legacy_text_is_dropped_explicitly_without_panic_or_admission() {
    let huge = "L".repeat(20_000);
    let mut control = json!({
        "current_instruction":huge,
        "current_instruction_entry_id":"legacy-big"
    });
    record(&mut control, "after", json!("after-entry"), Kind::Turn);
    let intent = &control["user_intent"];
    assert_eq!(intent["legacy_context"]["text_omitted"], true);
    assert!(intent["legacy_context"]["text"].is_null());
    assert_eq!(intent["legacy_context"]["entry_id"], "legacy-big");
    assert_eq!(
        intent["legacy_context"]["provenance"],
        "unknown_effective_instruction"
    );
    assert_eq!(intent["earlier"], json!([]));
    assert_eq!(intent["latest"]["text"], "after");
    let bytes = serde_json::to_vec(&intent["legacy_context"]).unwrap().len();
    assert!(bytes <= MAX_EARLIER_BYTES, "{bytes} bytes");
}

#[test]
fn older_trail_is_bounded_by_count_and_bytes_with_explicit_omission_count() {
    let mut control = json!({});
    for index in 0..12 {
        record_turn(&mut control, &format!("m{index}"), index);
    }
    let intent = &control["user_intent"];
    let texts: Vec<String> = intent["earlier"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(texts, (3..11).map(|i| format!("m{i}")).collect::<Vec<_>>());
    assert_eq!(intent["omitted_messages"], 3);
    assert_eq!(intent["latest"]["text"], "m11");

    // Stable small entry IDs: the byte bound must come from the texts, not from ID length.
    let mut control = json!({});
    let big = "x".repeat(6_000);
    for index in 0..5 {
        record_turn(&mut control, &big, index);
    }
    let intent = &control["user_intent"];
    assert_eq!(intent["earlier"].as_array().unwrap().len(), 2);
    assert_eq!(intent["omitted_messages"], 2);
    let bytes = serde_json::to_vec(&intent["earlier"]).unwrap().len();
    assert!(bytes <= MAX_EARLIER_BYTES, "{bytes} bytes");
    assert_eq!(intent["latest"]["text"], big);
}

#[test]
fn continuation_appends_policy_and_trail_only_when_a_user_message_was_recorded() {
    assert_eq!(
        continuation("Continue active goal.", &json!({}), &[]),
        "Continue active goal."
    );
    let mut control = json!({});
    record_turn(&mut control, "ship it", 0);
    let prompt = continuation("Continue active goal.", &control, &[]);
    assert!(prompt.starts_with("Continue active goal.\n\n"));
    assert!(prompt.contains(INTENT_POLICY));
    assert!(prompt.ends_with(&control["user_intent"].to_string()));
}

#[test]
fn repeated_continuations_dedupe_against_visible_history_and_reattach_after_compaction() {
    let mut control = json!({});
    record_turn(&mut control, "ship it", 0);
    let first = continuation("Continue.", &control, &[]);
    // The provenance block is already visible in the branch: a repeat must not duplicate it.
    let visible = vec![json!({"role":"user","content":first.clone()})];
    assert_eq!(continuation("Continue.", &control, &visible), "Continue.");
    // Compaction removed the note from history: it reattaches on the next continuation.
    let after_compaction: Vec<Value> = Vec::new();
    assert!(continuation("Continue.", &control, &after_compaction).contains(INTENT_POLICY));
}

#[test]
fn changed_provenance_is_readmitted_even_when_earlier_history_shows_the_old_note() {
    let mut control = json!({});
    record_turn(&mut control, "ship it", 0);
    let first = continuation("Continue.", &control, &[]);
    let visible = vec![json!({"role":"user","content":first})];
    record(
        &mut control,
        "use v2 instead",
        json!("steer-1"),
        Kind::Steering,
    );
    let updated = continuation("Continue.", &control, &visible);
    assert!(updated.contains("use v2 instead"), "{updated}");
}
