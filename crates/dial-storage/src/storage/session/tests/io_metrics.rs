use super::*;

#[test]
fn control_publications_count_attempts_equal_states_and_changes_without_extra_state() {
    let e = env();
    let mut session = new_session(&e);
    let before = session.io_metrics();

    session.set_control_state(json!({"mode": "a"})).unwrap();
    let changed = session.io_metrics().since(&before);
    assert_eq!(changed.control_publication_attempts, 1);
    assert_eq!(changed.equal_control_publications, 0);
    assert_eq!(changed.control_publication_failures, 0);
    assert_eq!(session.control_state()["mode"], "a");

    let mid = session.io_metrics();
    let previous_revision = session.memory_revision().map(str::to_owned);
    session.set_control_state(json!({"mode": "a"})).unwrap();
    let equal = session.io_metrics().since(&mid);
    assert_eq!(equal.control_publication_attempts, 1);
    assert_eq!(equal.equal_control_publications, 1);
    assert_eq!(equal.control_publication_failures, 0);
    assert_eq!(
        equal.equal_control_publication_micros, equal.control_publication_micros,
        "an equal publication uses the same timing sample as its parent operation"
    );
    assert_eq!(session.control_state()["mode"], "a");
    assert_ne!(
        session.memory_revision(),
        previous_revision.as_deref(),
        "equal-state publications must retain their memory-authority event"
    );

    // The changed state still persists and replays after reopen.
    session.set_control_state(json!({"mode": "b"})).unwrap();
    let path = session.path().unwrap().to_owned();
    drop(session);
    assert_eq!(Session::open(&path).unwrap().control_state()["mode"], "b");
}

#[test]
fn failed_control_publication_counts_one_failure_and_no_equal_publication() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("kept")).unwrap();
    let path = session.path().unwrap().to_owned();
    // A read-only snapshot has no writer; publication must fail and count as a failed attempt.
    let mut snapshot = Session::read(&path).unwrap();
    let before = snapshot.io_metrics();

    assert!(snapshot.set_control_state(json!({"mode": "x"})).is_err());
    let delta = snapshot.io_metrics().since(&before);
    assert_eq!(delta.control_publication_attempts, 1);
    assert_eq!(delta.control_publication_failures, 1);
    assert_eq!(delta.equal_control_publications, 0);
}

#[test]
fn flush_runtime_counts_each_call_and_ephemeral_sessions_skip_sync_counters() {
    let e = env();
    let mut session = new_session(&e);
    let before = session.io_metrics();
    session.flush_runtime().unwrap();
    let delta = session.io_metrics().since(&before);
    assert_eq!(delta.runtime_flush_attempts, 1);
    assert_eq!(delta.runtime_flush_failures, 0);
    assert_eq!(delta.runtime_reclaim_sync_attempts, 0);

    let mut ephemeral = Session::ephemeral();
    ephemeral.append(user("memory only")).unwrap();
    ephemeral.set_control_state(json!({"mode": "e"})).unwrap();
    let eph = ephemeral.io_metrics();
    assert_eq!(eph.context_sync_attempts, 0);
    assert_eq!(eph.control_publication_attempts, 1);
}

#[test]
fn context_entry_sync_is_counted_once_per_acknowledged_append() {
    let e = env();
    let mut session = new_session(&e);
    let before = session.io_metrics();
    session
        .append_custom_entry("dial_test", json!({"v": 1}))
        .unwrap();
    let delta = session.io_metrics().since(&before);
    assert_eq!(delta.context_sync_attempts, 1);
    assert_eq!(delta.context_sync_failures, 0);
}

#[test]
fn since_saturates_when_snapshots_are_reversed() {
    let later = SessionIoMetrics {
        control_publication_attempts: 5,
        ..SessionIoMetrics::default()
    };
    let earlier = SessionIoMetrics {
        control_publication_attempts: 9,
        ..SessionIoMetrics::default()
    };
    assert_eq!(later.since(&earlier), SessionIoMetrics::default());
}
