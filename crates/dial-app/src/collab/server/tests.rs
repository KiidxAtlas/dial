use super::*;

#[test]
fn connection_limit_includes_pending_handshakes_and_releases_on_worker_drop() {
    let active = Arc::new(AtomicUsize::new(0));
    let mut permits: Vec<_> = (0..MAX_CONNECTIONS)
        .map(|_| ConnectionPermit::acquire(&active).unwrap())
        .collect();
    assert!(ConnectionPermit::acquire(&active).is_none());
    // std::thread::Builder drops an unstarted closure on spawn failure. A permit captured
    // there must be returned just as it is after a completed worker.
    let permit = permits.pop().unwrap();
    let unstarted_worker = move || drop(permit);
    drop(unstarted_worker);
    assert!(ConnectionPermit::acquire(&active).is_some());
    drop(permits);
    assert_eq!(active.load(Ordering::Acquire), 0);
}

#[test]
fn room_limit_refuses_new_host_without_mutating_rooms() {
    let rooms: Rooms = Arc::default();
    for n in 0..MAX_ROOMS {
        rooms
            .lock()
            .unwrap()
            .insert(format!("room-{n:016}"), Room::default());
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker_rooms = rooms.clone();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        peer(stream, worker_rooms, Arc::new(AtomicU32::new(1))).unwrap();
    });
    let (mut socket, _) =
        tungstenite::connect(format!("ws://{address}/r/another-room-id?role=host")).unwrap();
    let message = socket.read().unwrap();
    assert!(matches!(message, Message::Close(Some(frame)) if frame.code == CloseCode::from(4029)));
    drop(socket);
    worker.join().unwrap();
    assert_eq!(rooms.lock().unwrap().len(), MAX_ROOMS);
}

#[test]
fn authenticated_links_admit_writers_without_granting_viewers_or_forged_tokens() {
    use crate::collab::{
        guest::Guest,
        host::{Host, Steer},
        protocol::{format_link, parse_link},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("ws://{}", listener.local_addr().unwrap());
    let rooms: Rooms = Arc::default();
    let relay_rooms = rooms.clone();
    let worker = thread::spawn(move || {
        let next_peer = Arc::new(AtomicU32::new(1));
        let mut peers = Vec::new();
        for incoming in listener.incoming().take(4) {
            let (rooms, next_peer) = (relay_rooms.clone(), next_peer.clone());
            peers.push(thread::spawn(move || {
                peer(incoming.unwrap(), rooms, next_peer)
            }));
        }
        for peer in peers {
            peer.join().unwrap().unwrap();
        }
    });
    let mut host = Host::start(
        &relay,
        true,
        true,
        vec![json!({"role":"user","content":"history"})],
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rooms
        .lock()
        .unwrap()
        .values()
        .all(|room| room.host.is_none())
    {
        assert!(Instant::now() < deadline, "host did not register");
        thread::sleep(POLL);
    }
    let parsed = parse_link(host.link(), "").unwrap();
    let view = format_link(
        &relay,
        &parsed.room_id,
        &parsed.key,
        &parsed.host_public,
        None,
    )
    .unwrap();
    let forged = format_link(
        &relay,
        &parsed.room_id,
        &parsed.key,
        &parsed.host_public,
        Some(&[42; 16]),
    )
    .unwrap();
    let mut writer = Guest::join("", host.link(), "writer").unwrap();
    let mut viewer = Guest::join("", &view, "viewer").unwrap();
    let mut forger = Guest::join("", &forged, "forger").unwrap();
    while host.guest_count() != 3
        || [&writer, &viewer, &forger]
            .iter()
            .any(|guest| guest.replicated_entries() != 1)
    {
        host.pump();
        writer.pump();
        viewer.pump();
        forger.pump();
        assert!(
            Instant::now() < deadline,
            "signed challenge/replication handshake did not complete"
        );
        thread::sleep(POLL);
    }
    assert!(!writer.is_read_only());
    assert!(viewer.is_read_only());
    assert!(forger.is_read_only());
    assert!(viewer.send_prompt("forbidden").is_err());
    assert!(forger.send_prompt("forbidden").is_err());
    writer.send_prompt("allowed").unwrap();
    loop {
        host.pump();
        let steering = host.take_steering();
        if !steering.is_empty() {
            assert_eq!(steering, [Steer::Prompt("allowed".into())]);
            break;
        }
        assert!(Instant::now() < deadline, "writer prompt did not arrive");
        thread::sleep(POLL);
    }
    drop((writer, viewer, forger));
    drop(host);
    worker.join().unwrap();
}
