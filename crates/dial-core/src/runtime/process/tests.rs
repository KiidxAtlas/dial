use super::*;

fn sh(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(script);
    command
}

#[test]
fn captures_streams_with_caps_and_stdin() {
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    let done = run(
        sh("cat; echo err >&2; exit 3"),
        Some(b"hello world".to_vec()),
        Duration::from_secs(10),
        5,
        &cancel,
        &mut |stream, chunk| seen.push((stream, chunk.to_vec())),
    )
    .unwrap();
    assert_eq!(done.stdout, b"hello");
    assert_eq!(done.stdout_total, 11);
    assert!(!done.outcome().output_complete);
    assert_eq!(done.stderr_text(100), "err");
    assert_eq!(done.code(), Some(3));
    assert!(!done.success());
    assert!(seen.iter().any(|(s, _)| *s == Stream::Stderr));
    assert!(
        done.failure("x", Duration::from_secs(1))
            .contains("err (exit code 3)")
    );
}

#[test]
fn deadline_kills_the_whole_group() {
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let done = run(
        sh("sleep 30 & sleep 30"),
        None,
        Duration::from_millis(200),
        10,
        &cancel,
        &mut |_, _| {},
    )
    .unwrap();
    assert!(done.timed_out);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_daemon_holding_the_pipe_does_not_hang_the_caller() {
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    // The background job keeps stdout open after the shell exits, like `ssh -f`.
    let done = run(
        sh("(sleep 5) & echo ready"),
        None,
        Duration::from_secs(10),
        100,
        &cancel,
        &mut |_, _| {},
    )
    .unwrap();
    assert!(done.success());
    assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "ready");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn cancellation_before_start_runs_nothing() {
    let cancel = AtomicBool::new(true);
    let done = run(
        sh("exit 0"),
        None,
        Duration::from_secs(1),
        1,
        &cancel,
        &mut |_, _| {},
    )
    .unwrap();
    assert!(done.cancelled && done.status.is_none());
}

#[test]
fn capture_and_retention_are_independent() {
    use crate::runtime::execution::Completeness;
    let done = run(
        sh("printf 123456789"),
        None,
        Duration::from_secs(2),
        3,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .unwrap();
    let facts = done.execution(false);
    let output = facts.output.unwrap();
    assert_eq!(output.capture, Completeness::Complete);
    assert_eq!(output.retention, Completeness::Partial);
    assert_eq!(output.original_bytes, Some(9));
    assert_eq!(output.retained_bytes, Some(3));
    assert_eq!(output.omitted_bytes, Some(6));
    assert!(!facts.process.unwrap().output_complete);
}

#[test]
fn cancellation_drains_buffered_output_before_claiming_eof() {
    let cancel = AtomicBool::new(false);
    let done = run(
        sh("printf first; sleep 30"),
        None,
        Duration::from_secs(2),
        1024,
        &cancel,
        &mut |_, _| {
            cancel.store(true, Ordering::Release);
        },
    )
    .unwrap();
    assert!(done.cancelled);
    assert_eq!(done.stdout, b"first");
    assert!(!done.output_incomplete);
}

#[test]
fn unread_pipe_never_claims_complete_output() {
    let mut policy = Policy::bounded(Duration::from_secs(2), 1024);
    policy.retain_descendants = true;
    let done = run_with_policy(
        sh("(sleep 0.7) & printf ready"),
        None,
        policy,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .unwrap();
    assert!(done.output_incomplete);
    assert!(!done.outcome().output_complete);
    assert_eq!(
        done.output_integrity().capture,
        crate::runtime::execution::Completeness::Partial
    );
}
