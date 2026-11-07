use super::*;

#[test]
fn onboarding_has_explicit_hosted_and_local_paths_and_escape_is_cancel() {
    assert!(matches!(
        onboarding_destination(Some(1)),
        Some(Which::Providers)
    ));
    assert!(matches!(
        onboarding_destination(Some(2)),
        Some(Which::Models)
    ));
    for cancel in [None, Some(0), Some(99)] {
        assert!(onboarding_destination(cancel).is_none(), "{cancel:?}");
    }
    // Cancelling a panel returns a Cancel flow; it is not an implicit local selection.
    assert!(matches!(
        onboarding_flow(&OverlayOutcome::default()),
        OnboardingFlow::Cancel
    ));
}

#[test]
fn onboarding_verification_is_an_explicit_action_even_after_a_selection_is_saved() {
    let outcome = OverlayOutcome {
        next: Some("command:setup choose verify-model".into()),
        latest: Some(Config::default()),
        ..Default::default()
    };
    assert!(matches!(
        onboarding_flow(&outcome),
        OnboardingFlow::VerifySelectedModel
    ));
}
