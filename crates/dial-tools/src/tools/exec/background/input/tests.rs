use super::*;
use std::sync::Arc;

#[test]
fn queued_input_progress_does_not_require_output_or_exit_notifications() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let input = Input::new(
        Handle::Terminal(crate::platform::io::QueuedWriter::with_notify(
            file.reopen().unwrap(),
            1,
            wake::notify,
        )),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let bytes = b"queued-input";
    assert_eq!(
        input
            .write(
                bytes,
                Some(Instant::now() + Duration::from_secs(2)),
                &cancel,
                &cancel
            )
            .unwrap(),
        bytes.len()
    );
    assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
}

#[test]
fn readiness_already_delivered_is_not_lost_before_wait_registration() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let handle = Handle::Terminal(crate::platform::io::QueuedWriter::with_notify(
        file.reopen().unwrap(),
        1,
        wake::notify,
    ));
    let input = Input::new(handle, Arc::new(AtomicBool::new(false))).unwrap();
    let slot = input.handle.lock();
    let handle = slot.as_ref().unwrap();
    let cancel = AtomicBool::new(false);
    // No queued bytes: there will be no future delivery notification to rescue this wait.
    // An expired deadline makes this deterministic, without depending on wall-clock timing.
    input
        .wait_writable(handle, Some(Instant::now()), &cancel, &cancel, 0)
        .expect("already-ready input must not wait for a future notification");
}

#[test]
fn queued_input_blocked_on_os_pipe_observes_cancel_and_deadline() {
    for cancelled in [false, true] {
        let (_read, write) = io::pipe().unwrap();
        let input = Input::new(Handle::pipe(write), Arc::new(AtomicBool::new(false))).unwrap();
        let cancel = AtomicBool::new(cancelled);
        let interrupt = AtomicBool::new(false);
        let result = input.write(
            &vec![b'x'; 1024 * 1024],
            Some(Instant::now() + Duration::from_millis(30)),
            &cancel,
            &interrupt,
        );
        assert!(result.unwrap_err().contains(if cancelled {
            "cancel"
        } else {
            "deadline exceeded"
        }));
    }
}
