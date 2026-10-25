use super::*;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Codebase) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("routing.rs"), "pub fn route() {}\n").unwrap();
    fs::write(project.path().join("consumer.rs"), "use crate::routing;\n").unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("state")).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    cache
        .refresh(
            &Workspace::new(project.path()).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
    (home, project, cache)
}

#[test]
fn blocked_work_neither_blocks_poll_or_task_drop_nor_admits_a_queue() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (cancel_tx, cancel_rx) = mpsc::sync_channel(1);
    let worker = Worker::start(move |_, cancelled| {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        cancel_tx.send(cancelled()).unwrap();
        Ok(Hints::default())
    })
    .unwrap();
    let task = worker.submit(vec!["routing.rs".into()]).unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let pending = matches!(task.try_take(), Err(mpsc::TryRecvError::Empty));
    let rejected = worker.submit(vec!["consumer.rs".into()]).is_none();
    drop(task);
    // Task drop returned while the work is still deliberately blocked.
    release_tx.send(()).unwrap();
    let cancelled = cancel_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(worker);
    assert!(pending && rejected && cancelled);
}

#[test]
fn cache_shutdown_cancels_and_joins_its_single_owned_worker() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let worker = Worker::start(move |_, cancelled| {
        started_tx.send(()).unwrap();
        while !cancelled() {
            thread::yield_now();
        }
        stopped_tx.send(()).unwrap();
        Ok(Hints::default())
    })
    .unwrap();
    let task = worker.submit(vec!["routing.rs".into()]).unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(worker);
    stopped_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        task.try_take(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn admission_skips_busy_cache_locks_without_waiting() {
    let (_home, _project, cache) = fixture();
    let gate = cache.conn.lock();
    assert!(cache.try_impact_hints(vec!["routing.rs".into()]).is_none());
    drop(gate);
    let worker_gate = cache.enrichment.lock();
    assert!(cache.try_impact_hints(vec!["routing.rs".into()]).is_none());
    drop(worker_gate);
    assert!(cache.try_impact_hints(vec!["routing.rs".into()]).is_some());
}

#[test]
fn independent_worker_does_not_take_mandatory_gate_lock_and_checks_source_bytes() {
    let (_home, project, cache) = fixture();
    let task = cache.try_impact_hints(vec!["routing.rs".into()]).unwrap();
    // Once admitted, a worker uses its own connection rather than this gate connection.
    let gate = cache.conn.lock();
    let hints = task.result.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(hints.text.contains("consumer.rs"));
    assert_eq!(hints.sources.len(), 1);
    assert_eq!(hints.sources[0].0, "consumer.rs");
    drop(gate);
    fs::write(
        project.path().join("consumer.rs"),
        "// no longer a routing consumer\n",
    )
    .unwrap();
    let hints = cache
        .related_source_hints(&["routing.rs".into()], 3000, &|| false)
        .unwrap();
    assert!(hints.text.is_empty());
    assert!(hints.sources.is_empty());
}

#[test]
fn cancelled_source_query_never_returns_partial_hints() {
    let (_home, _project, cache) = fixture();
    assert!(
        cache
            .related_source_hints(&["routing.rs".into()], 3000, &|| true)
            .is_err()
    );
}
