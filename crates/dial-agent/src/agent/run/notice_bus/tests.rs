use super::*;

#[test]
fn discarded_reply_restores_notes_and_retracts_advisory_emission() {
    let mut bus = NoticeBus::default();
    bus.push_advisory(Priority::Loop, "Avoid repeating the same failed approach.");
    bus.push_protocol("Protocol continuation prompt.");

    let mut first = vec![json!({"role":"user","content":"task"})];
    let dropped = bus.flush_to_request(&mut first, 1, []);
    assert!(dropped.is_empty());
    assert!(
        first[1]["content"]
            .as_str()
            .unwrap()
            .contains("Avoid repeating")
    );

    // Reply arrived, then was discarded (0 assistant messages reached durable history).
    bus.reply_received(1);
    assert!(bus.pending().is_empty());
    assert!(bus.settle_delivered(|_| 0).is_none());
    assert_eq!(bus.pending().len(), 2);

    // Retry must still include the advisory note instead of suppressing it as a repeat.
    let mut retry = vec![json!({"role":"user","content":"task"})];
    let dropped = bus.flush_to_request(&mut retry, 1, []);
    assert!(dropped.is_empty());
    assert!(
        retry[1]["content"]
            .as_str()
            .unwrap()
            .contains("Avoid repeating")
    );

    // Once durable history records the assistant message, notes are retired and confirmed.
    bus.reply_received(1);
    let confirmed = bus.settle_delivered(|_| 1).unwrap();
    assert_eq!(confirmed.len(), 2);
    assert!(bus.pending().is_empty());
}

#[test]
fn steering_reset_keeps_safety_boundaries_and_clears_lower_advice() {
    let mut bus = NoticeBus::default();
    bus.push_advisory(Priority::Failure, "Old failure advice.");
    bus.push_advisory(Priority::Safety, "Keep this safety boundary.");
    bus.reset(true, true);
    assert_eq!(bus.pending().len(), 1);
    assert!(
        bus.pending()[0]["content"]
            .as_str()
            .unwrap()
            .contains("safety boundary")
    );
}
