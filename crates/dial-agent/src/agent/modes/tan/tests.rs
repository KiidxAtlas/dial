use super::*;

#[test]
fn cancel_removes_job_and_sets_interrupt() {
    let mut jobs = Jobs::default();
    jobs.insert_idle("tan-1");
    let interrupt = jobs.live["tan-1"].interrupt.clone();
    assert!(jobs.cancel("tan-1"));
    assert!(interrupt.load(Ordering::SeqCst));
    assert!(!jobs.live.contains_key("tan-1"));
    assert!(!jobs.cancel("tan-1"));
}

#[test]
fn dropping_the_registry_interrupts_every_live_clone() {
    let mut jobs = Jobs::default();
    jobs.insert_idle("tan-1");
    let interrupt = jobs.live["tan-1"].interrupt.clone();
    drop(jobs);
    assert!(interrupt.load(Ordering::SeqCst));
}

#[test]
fn finished_jobs_are_reported_once_with_their_outcome() {
    let mut jobs = Jobs::default();
    jobs.insert_idle("tan-1");
    jobs.insert_idle("tan-2");
    let failed = &jobs.live["tan-1"];
    *failed.outcome.lock().unwrap() = Some("failed: no model".to_owned());
    failed.finished.store(true, Ordering::SeqCst);
    assert_eq!(
        jobs.take_finished(),
        ["Background tan tan-1 failed: no model"]
    );
    assert!(jobs.take_finished().is_empty());
    assert!(jobs.live.contains_key("tan-2"));
}

#[test]
fn a_clone_that_never_started_leaves_no_session_file() {
    let root = tempfile::tempdir().unwrap();
    let unstarted = root.path().join("unstarted.jsonl");
    let started = root.path().join("started.jsonl");
    fs::write(&unstarted, "").unwrap();
    fs::write(&started, "").unwrap();
    drop(DiscardUnstarted {
        file: &unstarted,
        started: false,
    });
    drop(DiscardUnstarted {
        file: &started,
        started: true,
    });
    assert!(!unstarted.exists());
    assert!(started.exists());
}
