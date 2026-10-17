use super::*;

#[test]
fn frame_budget_refuses_overflow_and_marks_the_connection_overloaded() {
    let (outbox, rx) = channel();
    for _ in 0..MAX_QUEUED_FRAMES {
        outbox.send("{}".to_owned()).unwrap();
    }
    assert_eq!(outbox.send("{}".to_owned()), Err(Refused));
    assert!(outbox.is_overloaded());
    assert!(rx.is_overloaded());
    // Once overloaded, nothing more is admitted, even after the writer drains.
    assert!(rx.try_recv().is_ok());
    assert_eq!(outbox.send("{}".to_owned()), Err(Refused));
}

#[test]
fn byte_budget_is_charged_and_released_by_the_writer() {
    let (outbox, rx) = channel();
    let big = "x".repeat(MAX_QUEUED_BYTES / 2 + 1);
    outbox.send(big.clone()).unwrap();
    assert_eq!(outbox.send(big.clone()), Err(Refused));
    assert!(outbox.is_overloaded());

    let (outbox, rx2) = channel();
    outbox.send(big.clone()).unwrap();
    assert_eq!(rx2.recv().unwrap().len(), big.len());
    // The taken frame released its bytes, so a second frame of the same size fits again.
    outbox.send(big).unwrap();
    assert!(!outbox.is_overloaded());
    drop(rx);
}

#[test]
fn a_closed_receiver_refuses_without_marking_overload() {
    let (outbox, rx) = channel();
    drop(rx);
    assert_eq!(outbox.send("{}".to_owned()), Err(Refused));
    assert!(!outbox.is_overloaded());
}
