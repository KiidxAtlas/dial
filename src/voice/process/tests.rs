use super::*;
use std::{sync::atomic::Ordering, thread, time::Instant};

#[test]
fn which_finds_shell_and_rejects_missing() {
    assert!(which(Path::new("sh")).is_some());
    assert!(which(Path::new("definitely-not-a-dial-voice-program")).is_none());
    assert!(which(Path::new("")).is_none());
}

#[cfg(unix)]
#[test]
fn run_feeds_stdin_and_reports_failures() {
    let cancel = AtomicBool::new(false);
    let cat = CommandSpec::new("cat");
    let done = run(&cat, Some(b"hello"), &cancel, Duration::from_secs(5)).unwrap();
    assert_eq!(done.stdout, b"hello");
    let fail = CommandSpec::new("sh")
        .arg("-c")
        .arg("echo nope >&2; exit 3");
    let error = run(&fail, None, &cancel, Duration::from_secs(5))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("nope"), "{error}");
}

#[cfg(unix)]
#[test]
fn run_is_cancellable() {
    let cancel = AtomicBool::new(true);
    let sleep = CommandSpec::new("sleep").arg("5");
    let started = Instant::now();
    assert!(run(&sleep, None, &cancel, Duration::ZERO).is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[cfg(unix)]
#[test]
fn cancel_kills_pipeline_descendants() {
    let cancel = AtomicBool::new(false);
    // `sh` dies on cancel, but `sleep` and `cat` keep the stdout pipe open unless the group dies.
    let pipeline = CommandSpec::new("sh").arg("-c").arg("sleep 30 | cat");
    let started = Instant::now();
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(200));
            cancel.store(true, Ordering::SeqCst);
        });
        assert!(run(&pipeline, None, &cancel, Duration::ZERO).is_err());
    });
    assert!(started.elapsed() < Duration::from_secs(10));
}
