use super::*;

#[test]
fn provider_backoff_is_not_a_silent_stall() {
    let idle = Duration::from_secs(150);
    // Genuine remote silence past the limit is still bounded.
    assert!(silence_exceeded(120, false, false, false, idle));
    // A retry delay longer than the limit is an explicit wait, not silence.
    assert!(!silence_exceeded(120, false, false, true, idle));
    // Local prefill stays unarmed until generation is observed.
    assert!(!silence_exceeded(120, true, false, false, idle));
    assert!(silence_exceeded(120, true, true, false, idle));
    // Disabled limit, and idle time below the limit.
    assert!(!silence_exceeded(0, false, true, false, idle));
    assert!(!silence_exceeded(
        120,
        false,
        true,
        false,
        Duration::from_secs(119)
    ));
}
