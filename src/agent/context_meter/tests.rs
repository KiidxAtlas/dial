use super::*;

#[test]
fn native_progress_calibrates_context_without_double_counting_text() {
    let mut context = StreamingContext::new(1000);
    let mut progress = crate::llm::ModelProgress::new(crate::llm::InferencePhase::Generating);
    progress.prompt_tokens = Some(64);
    progress.output_tokens = Some(12);
    assert_eq!(context.telemetry(&progress), (76, false));
    assert_eq!(context.observe("a visible answer"), 76);
    assert!(!context.estimated());
    assert_eq!(context.finish(Some(64), Some(14)), (78, false));
    assert_eq!(context.finish(None, None), (76, false));
}
#[test]
fn fragments_accumulate_and_provider_usage_replaces_the_estimate() {
    let mut live = StreamingContext::new(100);
    for _ in 0..8 {
        live.observe("x");
    }
    assert_eq!(live.observe(""), 102);
    assert_eq!(live.finish(Some(80), Some(10)), (90, false));
    assert_eq!(live.finish(None, None), (102, true));
}
#[test]
fn wire_estimate_ignores_bookkeeping_and_does_not_double_count_native_replay() {
    use serde_json::json;
    let plain = json!({"role":"assistant","content":"x".repeat(400)});
    let annotated = json!({"role":"assistant","content":"x".repeat(400),
        "dial_evidence":{"artifact":"artifact://a","observation":"y".repeat(4000)}});
    assert_eq!(wire_message_bytes(&annotated), wire_message_bytes(&plain));
    let native = json!({"role":"assistant","content":"x".repeat(400),
        "dial_provider_content":[{"type":"text","text":"x".repeat(400)}]});
    assert!(wire_message_bytes(&native) < 2 * wire_message_bytes(&plain));
    let image = json!({"role":"user","content":"look","dial_images":[{"path":"a.png"}]});
    assert!(estimate_parts(&[image], &[]) >= IMAGE_ESTIMATE_TOKENS as u64);
}
#[test]
fn calibration_allows_context_to_shrink_after_projection() {
    let mut meter = ContextMeter::default();
    meter.complete(1000, 1000, Some(800), 0);
    assert_eq!(meter.predict(1200), 1000);
    assert_eq!(meter.predict(500), 300);
    assert_eq!(ContextMeter::default().predict(500), 500);
}
