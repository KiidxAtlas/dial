use super::*;
use std::time::{Duration, Instant};

fn finish(checks: &mut RuntimeUpdates, root: &Path, now: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(message) = checks.poll_at(root, now, |_| unreachable!("unexpected new check")) {
            return message;
        }
        assert!(Instant::now() < deadline, "background check did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn daily_boundary_and_clock_correction_do_not_disable_checks() {
    let last = CHECK_INTERVAL_SECS * 2;
    assert!(!due(last, last));
    assert!(!due(last + CHECK_INTERVAL_SECS - 1, last));
    assert!(due(last + CHECK_INTERVAL_SECS, last));
    assert!(due(last + CHECK_INTERVAL_SECS + 1, last));
    assert!(due(last - 1, last));
}

#[test]
fn completed_attempt_is_persisted_and_throttles_a_new_shell_until_next_day() {
    let root = tempfile::tempdir().unwrap();
    let now = CHECK_INTERVAL_SECS * 2;
    let mut checks = RuntimeUpdates::default();
    assert!(
        checks
            .poll_at(root.path(), now, |_| Ok("available".into()))
            .is_none()
    );
    assert_eq!(finish(&mut checks, root.path(), now), "available");
    assert_eq!(
        std::fs::read_to_string(root.path().join("runtime-update-check")).unwrap(),
        now.to_string()
    );
    drop(checks);

    let mut checks = RuntimeUpdates::default();
    assert!(
        checks
            .poll_at(
                root.path(),
                now + CHECK_INTERVAL_SECS - 1,
                |_| unreachable!("checked too early")
            )
            .is_none()
    );
    assert!(checks.pending.is_none());
    assert!(
        checks
            .poll_at(root.path(), now + CHECK_INTERVAL_SECS, |_| Ok(
                "next day".into()
            ))
            .is_none()
    );
    assert_eq!(
        finish(&mut checks, root.path(), now + CHECK_INTERVAL_SECS),
        "next day"
    );
}

#[test]
fn a_running_query_never_blocks_poll_or_starts_a_second_check() {
    let root = tempfile::tempdir().unwrap();
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let now = CHECK_INTERVAL_SECS;
    let mut checks = RuntimeUpdates::default();
    let before = Instant::now();
    assert!(
        checks
            .poll_at(root.path(), now, move |_| {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok("finished".into())
            })
            .is_none()
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let before = Instant::now();
    assert!(
        checks
            .poll_at(root.path(), now + CHECK_INTERVAL_SECS, |_| unreachable!(
                "duplicate query"
            ))
            .is_none()
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    release.send(()).unwrap();
    assert_eq!(finish(&mut checks, root.path(), now), "finished");
}

#[test]
fn failed_queries_are_reported_once_and_throttled_too() {
    let root = tempfile::tempdir().unwrap();
    let now = CHECK_INTERVAL_SECS;
    let mut checks = RuntimeUpdates::default();
    checks.poll_at(root.path(), now, |_| anyhow::bail!("offline fixture"));
    let message = finish(&mut checks, root.path(), now);
    assert!(message.contains("offline fixture"), "{message}");
    assert!(message.contains("/setup updates"), "{message}");
    drop(checks);
    let mut checks = RuntimeUpdates::default();
    assert!(
        checks
            .poll_at(root.path(), now + 1, |_| unreachable!("retry flood"))
            .is_none()
    );
    assert!(checks.pending.is_none());
}

#[test]
fn corrupt_timestamp_does_not_permanently_suppress_checks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("runtime-update-check"),
        "invalid timestamp",
    )
    .unwrap();
    let mut checks = RuntimeUpdates::default();
    checks.poll_at(root.path(), 10, |_| Ok("checked".into()));
    assert_eq!(finish(&mut checks, root.path(), 10), "checked");
}

#[test]
fn dropping_the_shell_cancels_without_waiting_for_the_worker() {
    let root = tempfile::tempdir().unwrap();
    let (started, ready) = mpsc::channel();
    let (stopped, observed) = mpsc::channel();
    let mut checks = RuntimeUpdates::default();
    checks.poll_at(root.path(), 10, move |cancel| {
        started.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        stopped.send(cancel.load(Ordering::Acquire)).unwrap();
        Ok("stopped".into())
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let before = Instant::now();
    drop(checks);
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(observed.recv_timeout(Duration::from_secs(3)).unwrap());
}
