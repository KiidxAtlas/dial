use super::*;

const START: &str = r#"{"type":"message_start","message":{"usage":{"input_tokens":4,"cache_creation_input_tokens":10,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":7},"output_tokens":1}}}"#;
const START_PLAIN: &str = r#"{"type":"message_start","message":{"usage":{"input_tokens":4,"cache_creation_input_tokens":10,"cache_read_input_tokens":0,"output_tokens":1}}}"#;
const DELTA: &str =
    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#;
const DELTA_1H: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5,"cache_creation":{"ephemeral_1h_input_tokens":9}}}"#;
const STOP: &str = r#"{"type":"message_stop"}"#;

fn stream(events: &[&str]) -> Reply {
    let mut acc = Accumulator::default();
    for event in events {
        acc.feed(event, &mut |_: &str| true, &mut |_: &StreamEvent| true)
            .unwrap();
    }
    acc.finish().unwrap()
}

#[test]
fn stream_reports_both_cache_ttl_buckets_from_message_start() {
    let reply = stream(&[START, DELTA, STOP]);
    assert_eq!(reply.cache_creation_tokens, Some(10));
    assert_eq!(reply.cache_creation_5m_tokens, Some(3));
    assert_eq!(reply.cache_creation_1h_tokens, Some(7));
    assert_eq!(reply.prompt_tokens, Some(14));
    assert_eq!(reply.completion_tokens, Some(5));
}

#[test]
fn message_delta_with_one_bucket_keeps_the_other() {
    let reply = stream(&[START, DELTA_1H, STOP]);
    assert_eq!(reply.cache_creation_5m_tokens, Some(3));
    assert_eq!(reply.cache_creation_1h_tokens, Some(9));
    assert_eq!(reply.cache_creation_tokens, Some(10));
}

#[test]
fn stream_without_split_reports_no_buckets() {
    let reply = stream(&[START_PLAIN, DELTA, STOP]);
    assert_eq!(reply.cache_creation_tokens, Some(10));
    assert_eq!(reply.cache_creation_5m_tokens, None);
    assert_eq!(reply.cache_creation_1h_tokens, None);
}

#[test]
fn cache_writes_total_is_sum_of_buckets_when_total_absent() {
    let usage = serde_json::json!({
        "cache_creation": {"ephemeral_5m_input_tokens": 2, "ephemeral_1h_input_tokens": 5}
    });
    assert_eq!(
        crate::llm::provider::anthropic_cache_writes(&usage),
        (Some(7), Some(2), Some(5))
    );
}

#[test]
fn cache_writes_absent_everywhere_stay_none() {
    assert_eq!(
        crate::llm::provider::anthropic_cache_writes(&serde_json::json!({})),
        (None, None, None)
    );
}
