use super::*;

fn probe_job(action: &str) -> Job {
    Job::Probe {
        action: action.to_string(),
    }
}

/// Worker entry for `cargo test`: runs only when the parent spawns this binary as a worker.
#[test]
fn worker_entry() {
    if std::env::var_os(WORKER_ENV).is_some() {
        worker_main();
    }
}

#[test]
fn worker_returns_text_errors_and_contains_panics_and_aborts() {
    let cancel = AtomicBool::new(false);
    let deadline = Duration::from_secs(60);
    assert_eq!(
        run("test", &probe_job("text"), &[], deadline, &cancel),
        Ok("alpha\nbeta <tag>".to_string())
    );
    assert_eq!(
        run("test", &probe_job("failed"), &[], deadline, &cancel),
        Err("bad input".to_string())
    );
    let panicked = run("test", &probe_job("panic"), &[], deadline, &cancel).unwrap_err();
    assert!(panicked.contains("failed internally"), "{panicked}");
    let aborted = run("test", &probe_job("abort"), &[], deadline, &cancel).unwrap_err();
    assert!(
        aborted.contains("without a result") || aborted.contains("crashed"),
        "{aborted}"
    );
}

#[test]
fn runaway_worker_is_killed_at_the_deadline() {
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let err = run(
        "test",
        &probe_job("hang"),
        &[],
        Duration::from_millis(500),
        &cancel,
    )
    .unwrap_err();
    assert!(err.contains("exceeded"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn cancellation_stops_a_running_worker() {
    let cancel = AtomicBool::new(false);
    let err = thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Release);
        });
        run(
            "test",
            &probe_job("hang"),
            &[],
            Duration::from_secs(60),
            &cancel,
        )
        .unwrap_err()
    });
    assert!(err.contains("cancelled"), "{err}");
}
