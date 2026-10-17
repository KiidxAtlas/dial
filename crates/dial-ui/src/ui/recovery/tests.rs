use super::*;

#[test]
fn step_not_closed_gets_the_plain_summary() {
    let raw = "Step not closed: no tool has run since this step started.";
    assert!(summary(raw).starts_with("The agent stopped because a step was not finished."));
    assert!(!summary(raw).contains("Step not closed"));
}

#[test]
fn other_stops_get_the_generic_summary() {
    assert_eq!(
        summary("Repeated the same call three times"),
        "The agent stopped before finishing. Ask it to continue or describe the next step."
    );
}

#[test]
fn raw_text_is_only_shown_on_request() {
    let raw = "Step not closed: focused checks failed";
    assert_eq!(panel_text(raw, false), summary(raw));
    assert_eq!(panel_text(raw, true), raw);
}
