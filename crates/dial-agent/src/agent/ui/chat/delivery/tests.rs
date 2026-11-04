use super::*;
use crate::agent::*;

#[test]
fn oversized_unicode_fields_roundtrip_in_order_without_loss() {
    for kind in 0..6 {
        let text = "a🦀".repeat(FRAGMENT_BYTES);
        let (tx, mut rx) = channel();
        thread::scope(|scope| {
            let expected = text.clone();
            let worker = scope.spawn(move || {
                let event = match kind {
                    0 => ChatEvent::Text(text),
                    1 => ChatEvent::Reasoning(text),
                    2 => ChatEvent::HiddenText(text),
                    3 => ChatEvent::Notice(text),
                    4 => ChatEvent::ToolArguments {
                        index: 7,
                        id: text.clone(),
                        name: text.clone(),
                        delta: text,
                    },
                    _ => ChatEvent::ToolCallComplete {
                        index: 7,
                        id: text.clone(),
                        name: text.clone(),
                        arguments: text,
                    },
                };
                assert!(tx.send(event).is_ok());
                assert!(
                    tx.send(ChatEvent::Finished(Box::new(Err(anyhow::anyhow!(
                        "terminal failure"
                    )))))
                    .is_ok()
                );
            });
            let mut event = loop {
                match rx.recv_timeout(HEARTBEAT) {
                    Ok(event) => break event,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(error) => panic!("lost live event: {error}"),
                }
            };
            for value in fields(&mut event) {
                assert_eq!(*value, expected, "field lost data for kind {kind}");
            }
            let ChatEvent::Finished(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap()
            else {
                panic!("missing terminal error");
            };
            assert_eq!(result.unwrap_err().to_string(), "terminal failure");
            worker.join().unwrap();
        });
    }
}

#[test]
fn full_queue_cancel_discards_backlog_and_unblocks_sender_before_join() {
    let (tx, mut rx) = channel();
    // Deterministically occupy every slot before the provider tries to publish more.
    for _ in 0..QUEUED_FRAGMENTS {
        tx.payload
            .send(Frame::Field(0, "queued".into()))
            .unwrap_or_else(|_| panic!("receiver alive"));
    }
    thread::scope(|scope| {
        let (started, waiting) = mpsc::sync_channel(0);
        let worker = scope.spawn(move || {
            started.send(()).unwrap();
            assert!(
                tx.send(ChatEvent::Text("late".repeat(FRAGMENT_BYTES)))
                    .is_err()
            );
            assert!(
                tx.send(ChatEvent::Finished(Box::new(Err(anyhow::anyhow!(
                    "provider stopped"
                )))))
                .is_ok()
            );
        });
        waiting.recv().unwrap();
        rx.cancel();
        let ChatEvent::Finished(result) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("cancelled backlog must never escape");
        };
        assert_eq!(result.unwrap_err().to_string(), "provider stopped");
        worker.join().unwrap();
    });
}

#[test]
fn oversized_event_only_places_bounded_fragments_in_the_queue() {
    let (tx, mut rx) = channel();
    thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let _ = tx.send(ChatEvent::Text("🦀".repeat(FRAGMENT_BYTES * 10)));
        });
        let payload = rx.payload.as_ref().unwrap();
        let mut bytes = 0;
        for _ in 0..QUEUED_FRAGMENTS {
            match payload.recv_timeout(Duration::from_secs(5)).unwrap() {
                Frame::Field(_, value) => {
                    assert!(value.len() <= FRAGMENT_BYTES);
                    bytes += value.len();
                }
                Frame::Header(_) => {}
                Frame::End => panic!("oversized payload should still be sending"),
            }
        }
        assert!(bytes <= QUEUED_FRAGMENTS * FRAGMENT_BYTES);
        rx.cancel();
        worker.join().unwrap();
    });
}

#[test]
fn reassembly_limit_fails_explicitly_and_releases_the_provider() {
    for (size, accepted) in [(16, true), (17, false)] {
        let (tx, mut rx) = channel_with_limit(16);
        thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let _ = tx.send(ChatEvent::Notice("x".repeat(size)));
            });
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ChatEvent::Notice(text) if accepted => assert_eq!(text.len(), size),
                ChatEvent::Finished(result) if !accepted => {
                    assert!(result.unwrap_err().to_string().contains("exceeds 16 bytes"));
                }
                _ => panic!("unexpected reassembly outcome for {size} bytes"),
            }
            rx.cancel();
            worker.join().unwrap();
        });
    }
}
