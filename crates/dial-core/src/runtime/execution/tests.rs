use super::*;

#[test]
fn legacy_execution_deserializes_without_new_facts() {
    let facts: Execution =
        serde_json::from_str(r#"{"status":"ok","effects":"known","changed_paths":[]}"#).unwrap();
    assert!(facts.observation.is_none());
    assert!(facts.process.is_none());
    assert!(facts.output.is_none());
    assert!(facts.integration.is_none());
}

#[test]
fn presentation_clipping_preserves_capture_and_retention() {
    let mut facts = OutputIntegrity {
        capture: Completeness::Complete,
        retention: Completeness::Complete,
        presentation: Completeness::Complete,
        retained_bytes: Some(20),
        presented_bytes: Some(20),
        ..Default::default()
    };
    facts.mark_presentation_partial();
    assert_eq!(facts.capture, Completeness::Complete);
    assert_eq!(facts.retention, Completeness::Complete);
    assert_eq!(facts.retained_bytes, Some(20));
    assert_eq!(facts.presentation, Completeness::Partial);
    assert_eq!(facts.presented_bytes, None);
}
