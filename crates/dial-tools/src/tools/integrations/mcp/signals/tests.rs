use super::*;

#[test]
fn stalled_consumer_retains_bounded_invalidations_terminal_and_latest_health() {
    let (sender, receiver) = channel();
    for _ in 0..10_000 {
        for signal in [
            Signal::ToolsChanged,
            Signal::PromptsChanged,
            Signal::ResourcesChanged,
            Signal::ResourceUpdated,
            Signal::NotificationHealthy,
            Signal::NotificationDegraded,
        ] {
            sender.send(signal);
        }
    }
    sender.send(Signal::NotificationHealthy);
    sender.send(Signal::Closed);
    assert_eq!(
        receiver.pending.lock().len(),
        7,
        "one slot per semantic kind"
    );
    assert!(matches!(receiver.recv().unwrap(), Signal::Closed));
    let mut kinds = Vec::new();
    while let Ok(signal) = receiver.try_recv() {
        kinds.push(signal);
    }
    assert!(matches!(kinds.last(), Some(Signal::NotificationHealthy)));
    assert!(
        kinds
            .iter()
            .any(|s| matches!(s, Signal::NotificationDegraded)),
        "recovery must not erase cache invalidation"
    );
    assert!(kinds.iter().any(|s| matches!(s, Signal::ToolsChanged)));
    drop(sender);
    assert!(
        receiver.recv().is_err(),
        "sender closure wakes a drained receiver"
    );
}
