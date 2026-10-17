use super::*;
use crate::runtime::Phase;
use std::{
    panic::AssertUnwindSafe,
    sync::{Mutex, atomic::AtomicUsize},
    thread,
    time::{Duration, Instant},
};

#[test]
fn a_freed_slot_takes_the_next_job_before_a_slow_sibling_ends() {
    let third_started = AtomicBool::new(false);
    let slow_saw_refill = AtomicBool::new(false);
    rolling(
        vec![0usize, 1, 2],
        2,
        |job| match job {
            0 => {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !third_started.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                slow_saw_refill.store(third_started.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            1 => {}
            _ => third_started.store(true, Ordering::SeqCst),
        },
        || {},
    );
    // Chunked scheduling would start job 2 only after the slow job 0 ended.
    assert!(slow_saw_refill.load(Ordering::SeqCst));
}

#[test]
fn rolling_never_exceeds_the_limit_and_runs_each_item_once() {
    let active = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let ran = Mutex::new(Vec::new());
    rolling(
        (0..20usize).collect(),
        3,
        |item| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(2));
            active.fetch_sub(1, Ordering::SeqCst);
            ran.lock().unwrap().push(item);
        },
        || {},
    );
    assert!(peak.load(Ordering::SeqCst) <= 3);
    let mut ran = ran.into_inner().unwrap();
    ran.sort_unstable();
    assert_eq!(ran, (0..20).collect::<Vec<_>>());
}

#[test]
fn pump_runs_after_workers_start_and_empty_work_is_a_no_op() {
    let pumped = AtomicBool::new(false);
    rolling(
        Vec::<usize>::new(),
        4,
        |_| panic!("no items"),
        || pumped.store(true, Ordering::SeqCst),
    );
    assert!(pumped.load(Ordering::SeqCst));
}

#[test]
fn a_panicking_job_releases_the_pump_and_queued_jobs_still_run() {
    for limit in [1, 3] {
        let (tx, rx) = mpsc::channel::<usize>();
        let items: Vec<(usize, mpsc::Sender<usize>)> = (0..4).map(|i| (i, tx.clone())).collect();
        drop(tx);
        let mut seen = Vec::new();
        let mut disconnected = false;
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            rolling(
                items,
                limit,
                |(i, tx)| {
                    if i == 0 {
                        panic!("job 0 failed");
                    }
                    let _ = tx.send(i);
                },
                || loop {
                    match rx.recv_timeout(Duration::from_secs(5)) {
                        Ok(value) => seen.push(value),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                    }
                },
            );
        }));
        assert!(result.is_err(), "the job's panic must propagate");
        assert!(disconnected, "pump must end by disconnect, not by timeout");
        seen.sort_unstable();
        assert_eq!(seen, vec![1, 2, 3]);
    }
}

#[test]
fn a_report_delivers_once_and_a_dropped_report_fails_its_job() {
    let (tx, rx) = mpsc::channel();
    Report::new(3, tx.clone()).finish(Outcome::interrupted(0));
    drop(Report::new(4, tx.clone()));
    drop(tx);
    match rx.recv() {
        Ok(Message::Done(3, outcome)) => {
            assert!(matches!(outcome.status.phase, Phase::Interrupted))
        }
        _ => panic!("finished report must send its own outcome"),
    }
    match rx.recv() {
        Ok(Message::Done(4, outcome)) => {
            assert!(matches!(outcome.status.phase, Phase::Failed))
        }
        _ => panic!("dropped report must send a failure"),
    }
    assert!(rx.recv().is_err(), "each report sends exactly one message");
}
