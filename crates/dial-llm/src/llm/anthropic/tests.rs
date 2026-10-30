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

#[test]
fn tool_completion_precedes_message_end_and_honors_cancellation() {
    for cancel in [false, true] {
        let mut acc = Accumulator::default();
        let mut seen = Vec::new();
        for event in [
            START,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c0","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\"}"}}"#,
        ] {
            acc.feed(event, &mut |_| true, &mut |e| {
                seen.push(e.clone());
                true
            })
            .unwrap();
        }
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallComplete { .. }))
        );
        let result = acc.feed(
            r#"{"type":"content_block_stop","index":0}"#,
            &mut |_| true,
            &mut |e| {
                seen.push(e.clone());
                !cancel
            },
        );
        assert_eq!(result.is_err(), cancel);
        assert!(
            matches!(seen.iter().rev().find(|e| !matches!(e, StreamEvent::Activity)).unwrap(), StreamEvent::ToolCallComplete { index:0, name, arguments, .. }
            if name == "read" && arguments == r#"{"path":"a.txt"}"#)
        );
        assert!(
            acc.finish().is_err(),
            "call completion is not response completion"
        );
    }
}

#[test]
fn invisible_stream_activity_excludes_keepalives_and_honors_cancellation() {
    let mut acc = Accumulator::default();
    let mut seen = Vec::new();
    for (event, active) in [
        (START, true),
        (
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            true,
        ),
        (r#"{"type":"ping"}"#, false),
        (
            r#"{"type":"message_delta","delta":{},"usage":{"output_tokens":2}}"#,
            false,
        ),
        (
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":""}}"#,
            false,
        ),
        (
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}"#,
            true,
        ),
    ] {
        seen.clear();
        acc.feed(
            event,
            &mut |_| panic!("signature is not visible text"),
            &mut |event| {
                seen.push(event.clone());
                true
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            if active {
                vec![StreamEvent::Activity]
            } else {
                vec![]
            },
            "{event}"
        );
    }
    let error = acc.feed(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"next"}}"#,
        &mut |_| true,
        &mut |_| false,
    ).unwrap_err();
    assert!(error.downcast_ref::<Interrupted>().is_some());
}
