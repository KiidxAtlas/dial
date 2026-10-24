use super::*;

#[test]
fn server_error_text_and_stderr_lose_terminal_escapes() {
    let shared = Shared::new(Vec::new(), None);
    let (tx, rx) = mpsc::channel();
    lock(&shared.state).pending.insert(
        1,
        Waiter {
            reply: tx,
            progress: None,
        },
    );
    shared.handle(&json!({"id": 1, "error": {"code": -1, "message": "bad\u{1b}[2Jinput"}}));
    match rx.recv().unwrap() {
        Event::Reply(Err(Failure::Rpc(text))) => assert_eq!(text, "bad[2Jinput (code -1)"),
        _ => panic!("expected an RPC failure"),
    }
    *lock(&shared.stderr) = b"boom\x1b]0;title\x07 end".to_vec();
    assert_eq!(shared.stderr_tail(), "boom]0;title end");
}

#[test]
fn progress_flood_retains_only_latest_and_never_queues_ahead_of_terminal_reply() {
    let shared = Shared::new(Vec::new(), None);
    let (tx, rx) = mpsc::channel();
    let latest = Arc::new(Mutex::new(None));
    lock(&shared.state).pending.insert(
        1,
        Waiter {
            reply: tx,
            progress: Some(latest.clone()),
        },
    );
    for progress in 0..10_000 {
        shared.handle(&json!({"method": "notifications/progress", "params": {
            "progressToken": 1, "progress": progress, "message": "x".repeat(1000)
        }}));
    }
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let (_, text) = lock(&latest).take().expect("one coalesced progress update");
    assert!(text.starts_with("9999 "));
    assert_eq!(text.chars().count(), 300);
    assert!(lock(&latest).is_none());
    shared.handle(&json!({"id": 1, "result": {"done": true}}));
    assert!(
        matches!(rx.recv_timeout(POLL), Ok(Event::Reply(Ok(value))) if value == json!({"done": true}))
    );
    assert!(lock(&shared.state).pending.is_empty());
}

/// Drives the real request wait loop without network or child processes. The callback may be
/// deliberately slow while the transport finishes flooding and sends its terminal response.
struct ProgressRemote {
    terminal: bool,
    callback_started: Mutex<mpsc::Receiver<()>>,
    cancellation: Sender<()>,
}

impl Remote for ProgressRemote {
    fn post(&self, message: &Value, _: Duration, sink: &mut dyn FnMut(Value)) -> Result<()> {
        if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            let _ = self.cancellation.send(());
            return Ok(());
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return Ok(());
        };
        assert_eq!(
            message.pointer("/params/_meta/progressToken"),
            Some(&json!(id))
        );
        sink(json!({"method": "notifications/progress", "params": {
            "progressToken": id, "progress": 1, "total": 10, "message": "working"
        }}));
        lock(&self.callback_started)
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        for progress in 2..10_000 {
            sink(json!({"method": "notifications/progress", "params": {
                "progressToken": id, "progress": progress
            }}));
        }
        if self.terminal {
            sink(json!({"id": id, "result": {"done": true}}));
        }
        Ok(())
    }

    fn replies_on_stream(&self) -> bool {
        true
    }
}

fn progress_client(terminal: bool) -> (Client, Sender<()>, mpsc::Receiver<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let remote = Arc::new(ProgressRemote {
        terminal,
        callback_started: Mutex::new(started_rx),
        cancellation: cancel_tx,
    });
    (
        Client::remote("test", remote, Arc::new(Shared::new(Vec::new(), None))),
        started_tx,
        cancel_rx,
    )
}

#[test]
fn ordinary_progress_and_terminal_reply_complete_with_a_slow_callback() {
    let (client, started, _) = progress_client(true);
    let seen = Mutex::new(Vec::new());
    let result = client.request_with(
        "tools/call",
        json!({}),
        Duration::from_secs(2),
        &|| false,
        Some(&|text| {
            lock(&seen).push(text.to_owned());
            let _ = started.send(());
            thread::sleep(Duration::from_millis(100));
        }),
    );
    assert_eq!(result.unwrap(), json!({"done": true}));
    assert_eq!(lock(&seen)[0], "1/10 working");
    assert!(lock(&client.shared.state).pending.is_empty());
}

#[test]
fn progress_flood_does_not_starve_cancellation_or_change_effects() {
    let (client, started, cancelled) = progress_client(false);
    let cancel = AtomicBool::new(false);
    let error = client
        .request_with(
            "tools/call",
            json!({}),
            Duration::from_secs(2),
            &|| cancel.load(Ordering::Relaxed),
            Some(&|_| {
                let _ = started.send(());
                cancel.store(true, Ordering::Relaxed);
            }),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    assert!(matches!(
        error.downcast_ref::<RequestEffects>(),
        Some(RequestEffects(Effects::Unknown))
    ));
    cancelled.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(lock(&client.shared.state).pending.is_empty());
}

#[test]
fn progress_flood_eventually_times_out_and_removes_waiter() {
    let (client, started, cancelled) = progress_client(false);
    let error = client
        .request_with(
            "tools/call",
            json!({}),
            Duration::from_millis(150),
            &|| false,
            Some(&|_| {
                let _ = started.send(());
            }),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("did not answer"));
    assert!(matches!(
        error.downcast_ref::<RequestEffects>(),
        Some(RequestEffects(Effects::Unknown))
    ));
    cancelled.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(lock(&client.shared.state).pending.is_empty());
}

#[test]
fn capability_exposure_requires_a_capability_object() {
    let (client, _, _) = progress_client(false);
    for malformed in [
        Value::Null,
        json!(false),
        json!(true),
        json!("tools"),
        json!([]),
    ] {
        *lock(&client.capabilities) = json!({"tools": malformed});
        assert!(!client.has_capability("tools"));
    }
    *lock(&client.capabilities) = json!({"tools": {"listChanged": false}});
    assert!(client.has_capability("tools"));
    assert!(!client.has_capability("prompts"));
}
