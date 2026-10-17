use super::*;

#[test]
fn flags_are_false_outside_a_tool_call() {
    assert!(!steering_requested());
    assert!(!wait_interrupted());
    assert!(current().is_none());
}

#[test]
fn scope_installs_and_restores_signals() {
    let outer = CallSignals::new();
    let inner = CallSignals::new();
    inner.request_steering();
    scope(&outer, || {
        assert!(!steering_requested());
        scope(&inner, || assert!(steering_requested()));
        assert!(!steering_requested());
        outer.interrupt_waits();
        assert!(wait_interrupted());
    });
    assert!(current().is_none());
}

#[test]
fn captured_signals_follow_the_owner_across_threads() {
    let signals = CallSignals::new();
    scope(&signals, || {
        let captured = current().expect("installed");
        let helper = std::thread::spawn(move || scope(&captured, wait_interrupted));
        signals.interrupt_waits();
        // The helper may observe either state depending on timing, but never panics; after the
        // owner raised the flag every later poll through the shared handle sees it.
        let _ = helper.join().unwrap();
        assert!(current().unwrap().wait_interrupted());
    });
}

#[test]
fn restore_runs_even_when_the_call_panics() {
    let signals = CallSignals::new();
    let result = std::panic::catch_unwind(|| scope(&signals, || panic!("tool panicked")));
    assert!(result.is_err());
    assert!(current().is_none());
}
