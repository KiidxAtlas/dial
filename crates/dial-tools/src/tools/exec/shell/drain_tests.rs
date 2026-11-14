use super::*;

fn drain_with_expired_grace(keep_writer: bool) -> (bool, Option<String>, String) {
    let (mut reader, mut writer) = io::pipe().unwrap();
    writer.write_all(b"ready\n").unwrap();
    let _writer = keep_writer.then_some(writer);
    crate::platform::io::set_nonblocking(&reader).unwrap();
    let mut capture = Capture::new();
    let mut spool = None;
    let mut artifact_error = None;
    let mut capture_error = None;
    let mut eof = false;
    let waiter = crate::platform::wake::Waiter::new().unwrap();
    drain_after_exit(
        &mut reader,
        &mut capture,
        &mut spool,
        &mut artifact_error,
        &mut capture_error,
        &mut eof,
        &waiter,
        &|_| {},
        Duration::ZERO,
    )
    .unwrap();
    assert!(artifact_error.is_none());
    (eof, capture_error, capture.finish())
}

#[test]
fn expired_cleanup_grace_still_observes_ready_output_and_eof() {
    // A descheduled waiter may resume after the deadline with EOF already available.
    let (eof, error, text) = drain_with_expired_grace(false);
    assert!(
        eof,
        "ready EOF must be observed before reporting capture failure: {error:?}"
    );
    assert_eq!(error, None);
    assert_eq!(text, "ready\n");
}

#[test]
fn expired_cleanup_grace_does_not_hide_a_writer_that_still_holds_the_pipe() {
    let (eof, error, text) = drain_with_expired_grace(true);
    assert!(!eof);
    assert!(error.unwrap().contains("pipe did not reach EOF"));
    assert_eq!(text, "ready\n");
}
