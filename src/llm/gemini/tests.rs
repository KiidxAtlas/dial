use super::*;

#[test]
fn accumulator_streams_wrapped_cca_parts_and_retains_signature_and_execution_id() {
    let patience = super::super::Patience::for_stream(30, false);
    let mut accumulator = Accumulator::default();
    let mut visible = String::new();
    let mut on_text: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut |delta| {
        visible.push_str(delta);
        true
    });
    let mut on_event = |_event: &StreamEvent| true;
    accumulator.feed(
        r#"{"responseId":"execution","response":{"candidates":[{"content":{"role":"model","parts":[{"text":"answer","thoughtSignature":"signed-native-value"}]},"finishReason":"STOP"}]}}"#,
        &patience,
        &mut on_text,
        &mut on_event,
    ).unwrap();
    assert_eq!(visible, "answer");
    assert_eq!(accumulator.execution_id(), Some("execution"));
    let endpoint = Endpoint {
        url: "https://example.test",
        auth: None,
        model: "gemini-3.1-pro",
        headers: &[],
    };
    let reply = accumulator.finish(&endpoint, "scope".into()).unwrap();
    assert_eq!(
        reply.provider_content[0]["parts"][0]["thoughtSignature"],
        "signed-native-value"
    );
}
