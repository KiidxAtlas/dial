use super::*;

fn reply(text: &str, finish: &str) -> Reply {
    Reply {
        text: text.into(),
        finish_reason: finish.into(),
        ..Default::default()
    }
}

fn facts(reasoning_guard: bool, plan_gate: bool) -> LengthFacts {
    LengthFacts {
        reasoning_guard,
        plan_gate,
        requested_tokens: 1024,
        window: 128_000,
        prompt_tokens: 1000,
    }
}

#[test]
fn only_length_stops_are_truncations() {
    let mut engine = Engine::default();
    let mut reasoning = reasoning::Recovery::default();
    assert!(
        truncation(
            &mut engine,
            &mut reasoning,
            &reply("done", "stop"),
            &facts(true, false)
        )
        .is_none()
    );
}

#[test]
fn an_empty_truncation_uses_the_single_reasoning_recovery_then_stops() {
    let mut engine = Engine::default();
    let mut reasoning = reasoning::Recovery::default();
    let empty = reply("", "length");
    let Some(Truncation::Reasoning { message, planned }) =
        truncation(&mut engine, &mut reasoning, &empty, &facts(true, true))
    else {
        panic!("expected the reasoning gate");
    };
    assert!(planned && message.contains("todo plan"), "{message}");
    assert!(matches!(
        truncation(&mut engine, &mut reasoning, &empty, &facts(true, true)),
        Some(Truncation::ReasoningExhausted(reason)) if reason.contains("after its one recovery attempt")
    ));
}

#[test]
fn without_the_reasoning_guard_an_empty_truncation_raises_the_limit() {
    let mut engine = Engine::default();
    let mut reasoning = reasoning::Recovery::default();
    assert!(matches!(
        truncation(
            &mut engine,
            &mut reasoning,
            &reply("", "length"),
            &facts(false, false)
        ),
        Some(Truncation::Raise(raised)) if raised > 1024
    ));
}

#[test]
fn the_worker_form_has_no_plan_gate() {
    let mut engine = Engine::default();
    let mut reasoning = reasoning::Recovery::default();
    assert!(matches!(
        truncation(&mut engine, &mut reasoning, &reply(" ", "length"), &facts(true, false)),
        Some(Truncation::Reasoning { planned: false, message }) if !message.contains("todo plan")
    ));
}
#[test]
fn form_failures_are_classified_once_with_harvestable_corrections() {
    let names = vec!["read".to_string()];
    let dropped = reply("", "tool_calls");
    assert_eq!(
        FormFailure::of(&dropped, &names),
        Some(FormFailure::DroppedCall)
    );
    let text = reply(r#"{"name": "read", "arguments": {"path": "a"}}"#, "stop");
    assert_eq!(FormFailure::of(&text, &names), Some(FormFailure::TextCall));
    assert_eq!(
        FormFailure::of(&reply("plain answer", "stop"), &names),
        None
    );
    for failure in [FormFailure::DroppedCall, FormFailure::TextCall] {
        assert!(!failure.correction().is_empty());
        assert!(!failure.kind().is_empty());
    }
    assert!(
        FormFailure::DroppedCall
            .correction()
            .starts_with("Your tool call was unreadable")
    );
}
