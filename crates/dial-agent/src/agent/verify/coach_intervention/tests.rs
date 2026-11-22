use super::*;

#[test]
fn coach_intervention_filters_content_free_payloads_and_preserves_priority() {
    assert!(
        CoachIntervention::advisory(
            InterventionKind::RoundReview,
            Priority::Blocker,
            "Looks good!",
            "Advisor blocker: Looks good!",
        )
        .is_none()
    );
    assert!(
        CoachIntervention::transient(
            InterventionKind::FailureEscalation,
            "   LGTM.  ",
            Some(100),
            |advice| format!("Advice: {advice}"),
        )
        .is_none()
    );

    let advisory = CoachIntervention::advisory(
        InterventionKind::QualityReview,
        Priority::Review,
        "Check visual output",
        "Review rendered screenshot.",
    )
    .expect("substantive advisory must be retained");
    assert_eq!(advisory.kind, InterventionKind::QualityReview);
    let message = advisory.into_message();
    assert_eq!(message["dial_transient"], true);
    assert_eq!(message["content"], "Review rendered screenshot.");

    let transient = CoachIntervention::transient(
        InterventionKind::FailureEscalation,
        "Run the unit test on parser.rs:42 to verify the escape branch.",
        Some(2400),
        |advice| format!("Wrapped:\n\n{advice}"),
    )
    .expect("substantive escalation must be retained");
    let message = transient.into_message();
    assert_eq!(message["dial_transient"], true);
    assert!(
        message["content"]
            .as_str()
            .unwrap()
            .contains("parser.rs:42")
    );
}
