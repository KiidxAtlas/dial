use super::*;

fn idle_core(capacity: usize) -> (Arc<SessionCore>, flume::Receiver<QueuedRequest>) {
    let core = SessionCore::new(DisplaySelector::Active);
    let (tx, rx) = flume::bounded(capacity);
    core.lifecycle.lock().tx = Some(tx);
    (core, rx)
}

#[test]
fn saturated_admission_refuses_immediately_and_drop_never_waits_for_capacity() {
    let (core, rx) = idle_core(1);
    let (reply, _) = flume::bounded(1);
    core.lifecycle
        .lock()
        .tx
        .as_ref()
        .unwrap()
        .try_send(QueuedRequest {
            request: Request::Capabilities { reply },
            token: core.cancellation.token(),
        })
        .unwrap_or_else(|_| panic!("initial request fits"));
    let result = core.call(core.cancellation.token(), |reply| Request::Capabilities {
        reply,
    });
    assert_eq!(
        result.err().expect("request must fail").code,
        error::ErrorCode::InputBusy
    );
    assert_eq!(core.in_flight.load(Ordering::Acquire), 0);
    let (done, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        drop(core);
        done.send(()).unwrap();
    });
    wait.recv_timeout(Duration::from_secs(1))
        .expect("Drop must not wait on a full queue");
    thread.join().unwrap();
    drop(rx);
}

#[test]
fn uncooperative_full_queue_reports_a_bounded_close_timeout() {
    let (core, rx) = idle_core(1);
    let (reply, _) = flume::bounded(1);
    core.lifecycle
        .lock()
        .tx
        .as_ref()
        .unwrap()
        .try_send(QueuedRequest {
            request: Request::Capabilities { reply },
            token: core.cancellation.token(),
        })
        .unwrap_or_else(|_| panic!("initial request fits"));
    let other = core.clone();
    let (finished, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = finished.send(other.close());
    });
    let result = wait
        .recv_timeout(CLOSE_TIMEOUT * 3)
        .expect("close must not block forever on queue admission");
    assert_eq!(result.unwrap_err().code, error::ErrorCode::Timeout);
    assert!(core.lifecycle.lock().closed);
    thread.join().unwrap();
    drop(rx);
}

#[test]
fn cancellation_releases_a_waiter_even_when_the_native_worker_does_not_reply() {
    let (core, rx) = idle_core(1);
    let other = core.clone();
    let thread = std::thread::spawn(move || {
        other.call(other.cancellation.token(), |reply| Request::Capabilities {
            reply,
        })
    });
    let queued = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("admitted request");
    core.cancellation.cancel_with(CancelReason::SessionClosed);
    let result = thread.join().unwrap();
    assert_eq!(
        result.err().expect("request must fail").code,
        error::ErrorCode::Cancelled
    );
    assert_eq!(core.in_flight.load(Ordering::Acquire), 0);
    drop(queued);
}

#[test]
fn close_rejects_later_admission_and_acknowledges_without_holding_lifecycle_lock() {
    let (core, rx) = idle_core(1);
    let weak = Arc::downgrade(&core);
    let worker = std::thread::spawn(move || {
        let request = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("close request");
        let core = weak.upgrade().unwrap();
        assert!(core.lifecycle.lock().closed);
        assert!(request.request.is_close());
        request.request.reply(Ok(Response::Unit));
    });
    core.close().unwrap();
    assert!(
        core.call(core.cancellation.token(), |reply| Request::Capabilities {
            reply
        })
        .is_err()
    );
    worker.join().unwrap();
}
