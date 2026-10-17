use super::*;

#[test]
fn computer_user_focus_changes_permanently_disarm_background_restoration() {
    let previous = ProcessSerialNumber { high: 0, low: 7 };
    let target = ProcessSerialNumber { high: 0, low: 8 };
    let third = ProcessSerialNumber { high: 0, low: 9 };
    let lease = || BackgroundFocusLease {
        previous,
        target,
        key: 42,
        activity: [0; 5],
        disarmed: false,
    };
    let mut guard = lease();
    assert_eq!(guard.observe(target, None, [0; 5]), FocusDecision::Restore);
    assert_eq!(guard.observe(third, None, [0; 5]), FocusDecision::Disarm);
    assert_eq!(guard.observe(target, None, [0; 5]), FocusDecision::Disarm);

    let mut guard = lease();
    assert_eq!(
        guard.observe(previous, Some(43), [0; 5]),
        FocusDecision::Disarm
    );
    assert_eq!(guard.observe(target, None, [0; 5]), FocusDecision::Disarm);

    let mut guard = lease();
    assert_eq!(guard.observe(target, None, [1; 5]), FocusDecision::Disarm);
}

#[test]
fn computer_typing_in_the_original_window_does_not_claim_a_user_focus_switch() {
    let previous = ProcessSerialNumber { high: 0, low: 7 };
    let target = ProcessSerialNumber { high: 0, low: 8 };
    let mut guard = BackgroundFocusLease {
        previous,
        target,
        key: 42,
        activity: [0; 5],
        disarmed: false,
    };
    assert_eq!(
        guard.observe(previous, Some(42), [1; 5]),
        FocusDecision::Observe
    );
    assert_eq!(guard.observe(target, None, [1; 5]), FocusDecision::Restore);
}

#[test]
fn computer_only_the_exact_key_window_of_the_front_process_skips_activation() {
    let target = ProcessSerialNumber { high: 0, low: 7 };
    let other = ProcessSerialNumber { high: 0, low: 8 };
    assert!(preserves_exact_existing_focus(
        Some(target),
        target,
        Some(42),
        42
    ));
    assert!(!preserves_exact_existing_focus(
        Some(other),
        target,
        Some(42),
        42
    ));
    assert!(!preserves_exact_existing_focus(
        Some(target),
        target,
        Some(41),
        42
    ));
    assert!(!preserves_exact_existing_focus(
        Some(target),
        target,
        None,
        42
    ));
    assert!(!preserves_exact_existing_focus(None, target, Some(42), 42));
}
