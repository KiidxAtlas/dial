use super::*;
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use serde_json::json;
use std::{
    sync::{RwLock, mpsc},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(5);

/// Work that stays parked until `gate` opens. It ignores the cancel token, like a read whose
/// owner has no cancellation point.
fn parked(
    gate: &Arc<AtomicBool>,
    value: Value,
) -> impl FnOnce(Arc<AtomicBool>) -> Result<Value> + Send + 'static {
    let gate = gate.clone();
    move |_| {
        while !gate.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(2));
        }
        Ok(value)
    }
}

/// A delivery that forwards into `tx`. The job drops it when the job ends, delivered or not.
fn answer_to(tx: &mpsc::Sender<Result<Value>>) -> impl FnOnce(Result<Value>) + Send + 'static {
    let tx = tx.clone();
    move |result| {
        let _ = tx.send(result);
    }
}

fn wait_for_slots_to_free(jobs: &ReadJobs) {
    let deadline = Instant::now() + WAIT;
    while jobs.in_flight() != 0 {
        assert!(Instant::now() < deadline, "read slots were never released");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn saturation_refuses_past_the_bound_and_frees_a_slot_when_a_read_finishes() {
    let jobs = ReadJobs::new();
    let gate = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    for n in 0..MAX_CONCURRENT_READS {
        jobs.spawn(parked(&gate, json!(n)), answer_to(&tx))
            .expect("reads below the bound start");
    }
    assert_eq!(jobs.in_flight(), MAX_CONCURRENT_READS);
    let refused = jobs
        .spawn(parked(&gate, json!("extra")), answer_to(&tx))
        .expect_err("a read past the bound is refused");
    assert_eq!(refusal_code(&refused), Some(ErrorCode::Busy));
    drop(tx);

    gate.store(true, Ordering::SeqCst);
    let mut delivered: Vec<u64> = (0..MAX_CONCURRENT_READS)
        .map(|_| rx.recv_timeout(WAIT).unwrap().unwrap().as_u64().unwrap())
        .collect();
    delivered.sort_unstable();
    assert_eq!(
        delivered,
        (0..MAX_CONCURRENT_READS as u64).collect::<Vec<_>>()
    );

    wait_for_slots_to_free(&jobs);
    let (tx, rx) = mpsc::channel();
    jobs.spawn(|_| Ok(json!("after")), answer_to(&tx))
        .expect("a freed slot admits the next read");
    assert_eq!(rx.recv_timeout(WAIT).unwrap().unwrap(), json!("after"));
}

#[test]
fn closing_the_connection_suppresses_late_answers_and_refuses_new_reads() {
    let jobs = ReadJobs::new();
    let gate = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    jobs.spawn(parked(&gate, json!("late")), answer_to(&tx))
        .unwrap();
    // Shutdown while the handler is parked: the controller stops accepting reads at once.
    jobs.close();
    let refused = jobs
        .spawn(|_| Ok(json!(1)), answer_to(&tx))
        .expect_err("no read starts on a closed connection");
    assert_eq!(refusal_code(&refused), Some(ErrorCode::Busy));
    drop(tx);

    // The non-cancellable handler finishes on its own, but nothing is delivered: the only sender
    // was dropped with the job, so the receiver disconnects without a message.
    gate.store(true, Ordering::SeqCst);
    assert!(matches!(
        rx.recv_timeout(WAIT),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    wait_for_slots_to_free(&jobs);
}

#[test]
fn cancellable_work_sees_the_connection_token_and_its_answer_is_dropped() {
    let jobs = ReadJobs::new();
    let stopped = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let observed = stopped.clone();
    jobs.spawn(
        move |cancel| {
            while !cancel.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(2));
            }
            observed.store(true, Ordering::SeqCst);
            Ok(json!("cancelled"))
        },
        answer_to(&tx),
    )
    .unwrap();
    drop(tx);
    jobs.close();
    assert!(matches!(
        rx.recv_timeout(WAIT),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(stopped.load(Ordering::SeqCst), "work saw the cancel token");
}

#[test]
fn an_admin_read_routed_through_a_job_delivers_the_owner_answer() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let config = Config {
        project_root: project.path().to_path_buf(),
        ..Config::default()
    };
    let admin = Arc::new(super::super::Admin {
        layout: Arc::new(RwLock::new(layout)),
        config: Arc::new(RwLock::new(config)),
        overlay: toml::Table::new(),
    });
    let jobs = ReadJobs::new();
    let (tx, rx) = mpsc::channel();
    let reader = admin.clone();
    jobs.spawn(
        move |_| {
            reader.handle(
                "list_checks",
                &Value::Null,
                (None, ModelSelection::default()),
            )
        },
        answer_to(&tx),
    )
    .unwrap();
    let answer = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(answer["configured"], json!([]));
}
