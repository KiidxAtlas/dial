use super::*;

#[test]
fn backoff_grows_and_caps_with_jitter() {
    for attempt in 0..40 {
        let delay = backoff(attempt, BACKOFF_MAX);
        assert!(delay <= BACKOFF_MAX.mul_f64(1.25));
        assert!(delay >= Duration::from_millis(750));
    }
    assert!(backoff(10, HOST_RECLAIM_BACKOFF_MAX) <= HOST_RECLAIM_BACKOFF_MAX.mul_f64(1.25));
}

#[test]
fn queue_retires_peers_and_drops_their_work() {
    let mut queue = Queue::default();
    queue.items.push_back(Pending::One {
        target: 2,
        text: "a".into(),
        bytes: 1,
    });
    queue.items.push_back(Pending::One {
        target: 0,
        text: "b".into(),
        bytes: 1,
    });
    queue.items.push_back(Pending::Batch {
        target: 2,
        charge: SnapshotCharge { id: 1, bytes: 0 },
        frames: Box::new(std::iter::empty()),
    });
    queue.bytes = 2;
    queue.retire(2);
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.bytes, 1);
    assert!(queue.retired_set.contains(&2));
    for peer in 100..(100 + MAX_RETIRED_PEERS as u32 + 10) {
        queue.retire(peer);
    }
    assert!(queue.retired_set.len() <= MAX_RETIRED_PEERS);
    queue.items.push_back(Pending::One {
        target: 5,
        text: "c".into(),
        bytes: 1,
    });
    queue.bytes += 1;
    queue.reset_for_recreated_room();
    assert!(queue.retired_set.is_empty());
    assert_eq!(
        queue.items.len(),
        1,
        "broadcast work survives a room recreation"
    );
}

#[test]
fn snapshot_streams_coalesce_per_peer_and_share_versions() {
    let stream = |id: usize, bytes: usize| Pending::Batch {
        target: 4,
        charge: SnapshotCharge { id, bytes },
        frames: Box::new(std::iter::empty()),
    };
    let mut queue = Queue::default();
    assert!(queue.push(stream(1, 100)));
    // The same peer asking again supersedes its unfinished snapshot instead of stacking one.
    assert!(queue.push(stream(1, 100)));
    assert_eq!(queue.items.len(), 1);
    // A second stream over the same log version is charged its bytes once.
    let other_peer = Pending::Batch {
        target: 5,
        charge: SnapshotCharge { id: 1, bytes: 100 },
        frames: Box::new(std::iter::empty()),
    };
    assert!(queue.push(other_peer));
    assert_eq!(queue.snapshot_bytes(), 100);
    assert_eq!(
        queue.bytes, 0,
        "snapshot streams are not frame-budget bytes"
    );
    // Distinct versions accumulate and are refused once the retained budget is exhausted.
    assert!(!queue.push(Pending::Batch {
        target: 6,
        charge: SnapshotCharge {
            id: 2,
            bytes: MAX_RETAINED_SNAPSHOT_BYTES,
        },
        frames: Box::new(std::iter::empty()),
    }));
    assert!(
        !queue.overloaded,
        "a refused snapshot is not a frame overload"
    );
}

#[test]
fn frames_are_stamped_and_sequence_checked() {
    let stamped = stamp_frame(Role::Guest, "epoch", 7, r#"{"t":"prompt","text":"hi"}"#).unwrap();
    let value: Value = serde_json::from_str(&stamped).unwrap();
    assert_eq!(value["role"], "guest");
    assert_eq!(value["epoch"], "epoch");
    assert_eq!(value["seq"], 7);
    assert_eq!(value["t"], "prompt");
    let empty: Value =
        serde_json::from_str(&stamp_frame(Role::Host, "e", 1, "{}").unwrap()).unwrap();
    assert_eq!(empty["seq"], 1);
    assert!(stamp_frame(Role::Host, "e", 1, "[]").is_none());

    let mut windows = SenderWindows::default();
    // A host accepts guest frames once; the identical ciphertext replayed later is rejected.
    assert!(accept_sequenced(&mut windows, Role::Host, &value));
    assert!(!accept_sequenced(&mut windows, Role::Host, &value));
    // A frame reflected back from the other role is refused even when it is fresh.
    let reflected = serde_json::json!({"role": "host", "epoch": "h", "seq": 1, "t": "prompt"});
    assert!(!accept_sequenced(&mut windows, Role::Host, &reflected));
    assert!(accept_sequenced(&mut windows, Role::Guest, &reflected));
}

#[test]
fn close_codes_map_to_reasons() {
    assert_eq!(
        close_reason(4009),
        Some("a host is already connected for this room")
    );
    assert_eq!(close_reason(1006), None);
    assert_eq!(closed_reason(Some(4001), ""), "room closed");
    assert_eq!(closed_reason(Some(1011), ""), "connection lost (code 1011)");
}
