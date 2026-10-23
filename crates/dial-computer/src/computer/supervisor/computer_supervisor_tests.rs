use super::*;

#[test]
fn computer_abort_signal_shares_flag() {
    let flag = Arc::new(AtomicBool::new(false));
    let signal = AbortSignal::from_flag(Arc::clone(&flag));
    assert!(!signal.is_aborted());
    flag.store(true, Ordering::SeqCst);
    assert!(signal.is_aborted());
}

#[test]
fn computer_ids_accept_strings_and_numbers() {
    assert_eq!(id_of(&json!("42")), Some(42));
    assert_eq!(id_of(&json!(7)), Some(7));
    assert_eq!(id_of(&json!("native-1")), None);
}
// These workers speak only the transport protocol; no DesktopSession is constructed.
#[cfg(unix)]
fn fake_worker(source: &str) -> ComputerSupervisor {
    let supervisor = ComputerSupervisor::new();
    let mut command = Command::new("python3");
    command
        .args(["-u", "-c", source])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::platform::process::spawn_group(&mut command).unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    crate::platform::io::set_nonblocking(&stdin).unwrap();
    crate::platform::io::set_nonblocking(&stdout).unwrap();
    let stopped = AbortSignal::new();
    supervisor.inner.state.lock().worker = Some(Worker {
        generation: 1,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        stopped: stopped.clone(),
        pending: HashMap::new(),
        ready: None,
        closed: None,
    });
    spawn_reader(Arc::downgrade(&supervisor.inner), 1, stdout, stopped);
    supervisor
}

#[cfg(unix)]
#[test]
fn computer_stalled_write_is_bounded_and_cancellable() {
    for cancel in [false, true] {
        let supervisor = fake_worker("import time; time.sleep(30)");
        let signal = AbortSignal::new();
        let started = Instant::now();
        std::thread::scope(|scope| {
            if cancel {
                let signal = &signal;
                scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    signal.abort();
                });
            }
            let error = send_guarded(
                &supervisor.inner,
                &json!({"code":"x".repeat(1024 * 1024)}),
                started + Duration::from_millis(200),
                Some(&signal),
            )
            .unwrap_err();
            assert_eq!(error.is_abort, cancel, "{error}");
        });
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(supervisor.inner.state.lock().worker.is_none());
    }
}

#[cfg(unix)]
#[test]
fn computer_oversized_unterminated_frame_discards_worker() {
    let supervisor = fake_worker(
        "import sys,time; sys.stdout.write('x' * (13 * 1024 * 1024)); sys.stdout.flush(); time.sleep(30)",
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while supervisor.inner.state.lock().worker.is_some() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    let state = supervisor.inner.state.lock();
    assert!(state.worker.is_none());
    assert_eq!(state.status, Status::Failed);
}

#[cfg(unix)]
#[test]
fn computer_closing_one_owner_preserves_another_worker() {
    const ECHO: &str = "import sys,json\nfor line in sys.stdin:\n r=json.loads(line)\n if r['type']=='close': print(json.dumps({'type':'closed'}),flush=True);break\n print(json.dumps({'type':'capabilities','id':r['id'],'ok':True,'capabilities':{'fake':True}}),flush=True)";
    let first = fake_worker(ECHO);
    let second = fake_worker(ECHO);
    first.close().unwrap();
    let (id, reply) = register_pending(&second.inner).unwrap();
    send_guarded(
        &second.inner,
        &json!({"type":"capabilities", "id":id}),
        Instant::now() + START_TIMEOUT,
        None,
    )
    .unwrap();
    assert_eq!(
        reply.recv_timeout(Duration::from_secs(2)).unwrap().unwrap()["fake"],
        true
    );
    assert!(second.inner.state.lock().session.is_none());
    second.close().unwrap();
}
#[cfg(unix)]
#[test]
fn computer_partial_frame_read_is_cancellable() {
    let supervisor =
        fake_worker("import sys,time; sys.stdout.write('{'); sys.stdout.flush(); time.sleep(30)");
    let (id, reply) = register_pending(&supervisor.inner).unwrap();
    let signal = AbortSignal::new();
    let started = Instant::now();
    std::thread::scope(|scope| {
        let signal = &signal;
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            signal.abort();
        });
        let error = await_reply(
            &supervisor.inner,
            Some(id),
            reply,
            started + Duration::from_secs(5),
            Some(signal),
            "unexpected timeout",
        )
        .unwrap_err();
        assert!(error.is_abort, "{error}");
    });
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(supervisor.inner.state.lock().worker.is_none());
}

#[cfg(unix)]
#[test]
fn computer_stale_generation_replies_are_discarded() {
    let supervisor = fake_worker("import time; time.sleep(30)");
    // Generation 7 never admitted a request; its reply must not reach generation 1.
    let error = send_reply(&supervisor.inner, 7, &json!({"type":"native-result"})).unwrap_err();
    assert_eq!(error.message, NOT_RUNNING);
    assert!(supervisor.inner.state.lock().worker.is_some());

    // A reply for the same request ID from an older generation must not settle the new one.
    let (id, reply) = register_pending(&supervisor.inner).unwrap();
    resolve_pending(&supervisor.inner, 2, &json!({"id": id}), Ok(json!("stale")));
    assert!(reply.try_recv().is_err());
    resolve_pending(
        &supervisor.inner,
        1,
        &json!({"id": id}),
        Ok(json!("current")),
    );
    assert_eq!(reply.try_recv().unwrap().unwrap(), json!("current"));
}

fn gate_inner() -> Arc<Inner> {
    Arc::clone(&ComputerSupervisor::new().inner)
}

#[test]
fn computer_sequential_runs_never_inherit_previous_cleanup() {
    let inner = gate_inner();
    let session = DesktopSession::new(None).unwrap();
    for _ in 0..3 {
        let gate = acquire_gate(
            &inner,
            &AbortSignal::new(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let token = session.token_for_test();
        assert!(token.check().is_ok());
        session.retire();
        assert!(token.check().is_err());
        drop(gate);
    }
}

#[test]
fn computer_overlapping_run_waits_for_cleanup_before_snapshotting_native_work() {
    let inner = gate_inner();
    let session = Arc::new(DesktopSession::new(None).unwrap());
    let gate = acquire_gate(
        &inner,
        &AbortSignal::new(),
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    let waiter = {
        let (inner, session) = (Arc::clone(&inner), Arc::clone(&session));
        std::thread::spawn(move || {
            let _gate = acquire_gate(
                &inner,
                &AbortSignal::new(),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
            session.token_for_test().check()
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    session.retire();
    drop(gate);
    assert!(waiter.join().unwrap().is_ok());
}

#[test]
fn computer_rejected_overlapping_run_leaves_active_run_untouched() {
    let inner = gate_inner();
    let session = DesktopSession::new(None).unwrap();
    let _gate = acquire_gate(
        &inner,
        &AbortSignal::new(),
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    let active = session.token_for_test();

    let abort = AbortSignal::new();
    abort.abort();
    let error = acquire_gate(&inner, &abort, Instant::now() + Duration::from_secs(1))
        .err()
        .unwrap();
    assert!(error.is_abort);
    let started = Instant::now();
    let error = acquire_gate(
        &inner,
        &AbortSignal::new(),
        started + Duration::from_millis(60),
    )
    .err()
    .unwrap();
    assert!(error.message.contains("busy"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        active.check().is_ok(),
        "rejected runs must not cancel the session"
    );
}

#[test]
fn computer_gate_wait_observes_close_and_abort_promptly() {
    let supervisor = ComputerSupervisor::new();
    let inner = Arc::clone(&supervisor.inner);
    let _gate = acquire_gate(
        &inner,
        &AbortSignal::new(),
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    let signal = AbortSignal::new();
    let waiter = {
        let (inner, signal) = (Arc::clone(&inner), signal.clone());
        std::thread::spawn(move || {
            acquire_gate(&inner, &signal, Instant::now() + Duration::from_secs(30))
                .err()
                .unwrap()
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    signal.abort();
    assert!(waiter.join().unwrap().is_abort);

    let waiter = {
        let inner = Arc::clone(&inner);
        std::thread::spawn(move || {
            acquire_gate(
                &inner,
                &AbortSignal::new(),
                Instant::now() + Duration::from_secs(30),
            )
            .err()
            .unwrap()
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    supervisor.close().unwrap();
    assert_eq!(waiter.join().unwrap().message, CLOSED);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[cfg(unix)]
#[test]
fn native_lifecycle_boundaries_apply_at_admission_before_next_request() {
    for method in ["retire", "cancel", "releaseControl"] {
        let supervisor = fake_worker("import time; time.sleep(30)");
        let session = Arc::new(DesktopSession::new(None).unwrap());
        supervisor.inner.state.lock().session = Some(session.clone());
        let delayed = session.admit();
        spawn_native(
            &supervisor.inner,
            1,
            &json!({
                "id": "boundary", "method": method, "args": {}
            }),
        );
        // No scheduling/wait loop: the boundary must already have taken effect
        // when the protocol reader is ready to admit the next message.
        assert!(delayed.check_admission().is_err(), "{method}");
        assert!(session.admit().check_admission().is_ok(), "{method}");
    }
}
