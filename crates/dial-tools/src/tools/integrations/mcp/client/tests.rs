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
    fn post(
        &self,
        message: &Value,
        _: Duration,
        operation: &Arc<Operation>,
        sink: &mut dyn FnMut(Value),
    ) -> Result<()> {
        if !operation.dispatch() {
            return Ok(());
        }
        operation.delivered();
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

struct QueuedRemote {
    entered: Sender<()>,
    exited: Arc<AtomicBool>,
    dispatched: Arc<AtomicBool>,
}
impl Remote for QueuedRemote {
    fn post(
        &self,
        _: &Value,
        _: Duration,
        operation: &Arc<Operation>,
        _: &mut dyn FnMut(Value),
    ) -> Result<()> {
        self.entered.send(()).unwrap();
        while !operation.stopped() {
            thread::sleep(Duration::from_millis(1));
        }
        self.dispatched
            .store(operation.dispatch(), Ordering::SeqCst);
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn cancellation_after_queue_admission_prevents_dispatch_and_joins_worker() {
    let (entered, rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let dispatched = Arc::new(AtomicBool::new(false));
    let client = Client::remote(
        "queued",
        Arc::new(QueuedRemote {
            entered,
            exited: exited.clone(),
            dispatched: dispatched.clone(),
        }),
        Arc::new(Shared::new(Vec::new(), None)),
    );
    let cancel = AtomicBool::new(false);
    let error = client
        .request_with(
            "tools/call",
            json!({}),
            Duration::from_secs(2),
            &|| {
                if rx.try_recv().is_ok() {
                    cancel.store(true, Ordering::SeqCst);
                }
                cancel.load(Ordering::SeqCst)
            },
            None,
        )
        .unwrap_err();
    assert!(
        exited.load(Ordering::SeqCst),
        "request worker must settle before returning"
    );
    assert!(!dispatched.load(Ordering::SeqCst));
    let facts = error.downcast_ref::<RequestFailure>().unwrap();
    assert!(!facts.dispatched);
    assert_eq!(facts.settlement, Settlement::Cancelled);
    assert!(matches!(
        error.downcast_ref::<RequestEffects>(),
        Some(RequestEffects(Effects::None))
    ));
}

struct HandshakeRemote {
    initialized: Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    listening: AtomicBool,
    fail: bool,
}
impl Remote for HandshakeRemote {
    fn post(
        &self,
        message: &Value,
        _: Duration,
        operation: &Arc<Operation>,
        sink: &mut dyn FnMut(Value),
    ) -> Result<()> {
        assert!(operation.dispatch());
        operation.delivered();
        match message["method"].as_str().unwrap() {
            "initialize" => sink(
                json!({"id": message["id"], "result": {"protocolVersion": PROTOCOL_VERSIONS[0], "capabilities": {}}}),
            ),
            "notifications/initialized" => {
                self.initialized.send(()).unwrap();
                lock(&self.release)
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                if self.fail {
                    bail!("initialized notification delivery failed");
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn after_initialize(self: Arc<Self>, _: &str, _: Arc<Shared>) {
        self.listening.store(true, Ordering::SeqCst);
    }
}

#[test]
fn initialize_waits_for_initialized_delivery_and_does_not_start_listener_on_failure() {
    for fail in [false, true] {
        let (initialized, entered) = mpsc::channel();
        let (release, rx) = mpsc::channel();
        let remote = Arc::new(HandshakeRemote {
            initialized,
            release: Mutex::new(rx),
            listening: AtomicBool::new(false),
            fail,
        });
        let client = Arc::new(Client::remote(
            "handshake",
            remote.clone(),
            Arc::new(Shared::new(Vec::new(), None)),
        ));
        let worker = thread::spawn(move || client.initialize(Duration::from_secs(2), &|| false));
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!remote.listening.load(Ordering::SeqCst));
        assert!(
            !worker.is_finished(),
            "handshake cannot publish success before delivery"
        );
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap().is_err(), fail);
        assert_eq!(remote.listening.load(Ordering::SeqCst), !fail);
    }
}

struct EndlessProgress;
impl Remote for EndlessProgress {
    fn post(
        &self,
        message: &Value,
        _: Duration,
        operation: &Arc<Operation>,
        sink: &mut dyn FnMut(Value),
    ) -> Result<()> {
        if !operation.dispatch() {
            return Ok(());
        }
        operation.delivered();
        let Some(id) = message.get("id") else {
            return Ok(());
        };
        while !operation.stopped() {
            sink(
                json!({"method": "notifications/progress", "params": {"progressToken": id, "progress": 1}}),
            );
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
    fn replies_on_stream(&self) -> bool {
        true
    }
}

#[test]
fn progress_never_extends_the_absolute_request_lifetime() {
    let client = Client::remote(
        "progress",
        Arc::new(EndlessProgress),
        Arc::new(Shared::new(Vec::new(), None)),
    );
    let began = Instant::now();
    let error = client
        .request_with_limit(
            "tools/call",
            json!({}),
            Duration::from_secs(2),
            &|| false,
            Some(&|_| {}),
            Duration::from_millis(150),
        )
        .unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(
        error.downcast_ref::<RequestFailure>().unwrap().settlement,
        Settlement::TimedOut
    );
}

#[test]
fn admission_is_bounded_before_any_remote_dispatch() {
    let (client, _, _) = progress_client(false);
    let permits: Vec<_> = (0..MAX_IN_FLIGHT)
        .map(|_| client.admit().unwrap())
        .collect();
    let error = client
        .request("tools/call", json!({}), Duration::from_secs(2))
        .unwrap_err();
    let facts = error.downcast_ref::<RequestFailure>().unwrap();
    assert!(!facts.dispatched);
    assert_eq!(facts.failure, IntegrationFailure::Admission);
    assert!(lock(&client.shared.state).pending.is_empty());
    drop(permits);
    assert_eq!(*lock(&client.admitted), 0);
}

#[cfg(unix)]
#[test]
fn cancelling_a_blocked_stdio_write_kills_and_reaps_the_connection() {
    let config = McpServerConfig {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 60".into()],
        ..McpServerConfig::default()
    };
    let client = Client::spawn("blocked", &config, Shared::new(Vec::new(), None)).unwrap();
    let began = Instant::now();
    let error = client
        .request(
            "tools/call",
            json!({"large": "x".repeat(2 * 1024 * 1024)}),
            Duration::from_millis(100),
        )
        .unwrap_err();
    assert!(began.elapsed() < Duration::from_secs(2));
    assert!(
        client.is_closed(),
        "blocked writes cannot be cancelled while preserving the pipe"
    );
    assert!(error.downcast_ref::<RequestFailure>().unwrap().dispatched);
    if let Transport::Stdio { child, .. } = &client.transport {
        assert!(
            lock(child).try_wait().unwrap().is_some(),
            "the child must be reaped before returning"
        );
    }
}

#[cfg(unix)]
#[test]
fn stdio_writer_skips_a_cancelled_queued_entry() {
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let shared = Arc::new(Shared::new(Vec::new(), None));
    let state = shared.clone();
    let (tx, rx) = mpsc::sync_channel(2);
    let cancelled = Operation::new(NOTIFY_TIMEOUT);
    tx.send(Outgoing {
        message: json!({"method": "must-not-run"}),
        operation: cancelled.clone(),
        delivered: None,
    })
    .unwrap();
    cancelled.cancel();
    let (delivered, confirmation) = mpsc::channel();
    tx.send(Outgoing {
        message: json!({"method": "only-this"}),
        operation: Operation::new(NOTIFY_TIMEOUT),
        delivered: Some(delivered),
    })
    .unwrap();
    drop(tx);
    let writer = thread::spawn(move || write_loop(stdin, rx, &state));
    confirmation
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    writer.join().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(
        lines.next().unwrap().unwrap(),
        json!({"method": "only-this"}).to_string()
    );
    assert!(lines.next().is_none());
    child.wait().unwrap();
}

#[cfg(unix)]
#[test]
fn closed_stdout_settles_an_inflight_blocked_stdin_write_before_return() {
    let config = McpServerConfig {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 0.15; exec 1>&-; exec sleep 60".into()],
        ..Default::default()
    };
    let client = Client::spawn("closed-output", &config, Shared::new(Vec::new(), None)).unwrap();
    let error = client
        .request(
            "tools/call",
            json!({"large":"x".repeat(2 * 1024 * 1024)}),
            Duration::from_secs(2),
        )
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RequestFailure>().unwrap().settlement,
        Settlement::TransportLost
    );
    if let Transport::Stdio { child, .. } = &client.transport {
        assert!(
            lock(child).try_wait().unwrap().is_some(),
            "an error return must settle blocked transport work, not merely forget the waiter"
        );
    }
}

#[cfg(unix)]
#[test]
fn progress_callback_panic_settles_blocked_stdio_work() {
    let config = McpServerConfig {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 60".into()],
        ..Default::default()
    };
    let client = Client::spawn("panic-output", &config, Shared::new(Vec::new(), None)).unwrap();
    let shared = client.shared.clone();
    let progress = thread::spawn(move || {
        while lock(&shared.state).pending.is_empty() {
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(100));
        shared.handle(
            &json!({"method":"notifications/progress","params":{"progressToken":1,"progress":1}}),
        );
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.request_with(
            "tools/call",
            json!({"large":"x".repeat(2 * 1024 * 1024)}),
            Duration::from_secs(2),
            &|| false,
            Some(&|_| panic!("consumer failed")),
        )
    }));
    progress.join().unwrap();
    assert!(result.is_err());
    assert!(lock(&client.shared.state).pending.is_empty());
    if let Transport::Stdio { child, .. } = &client.transport {
        assert!(
            lock(child).try_wait().unwrap().is_some(),
            "unwinding must not leave a blocked transport operation alive"
        );
    }
}
