use super::*;

fn record(dead_since_ms: u64) -> Record {
    Record {
        owner: ProcId { pid: 10, start: 1 },
        driver: ProcId { pid: 20, start: 2 },
        dead_since_ms: Some(dead_since_ms),
    }
}

#[test]
fn live_owner_is_never_reaped() {
    let action = decide(
        &record(0),
        1_000_000,
        Liveness::Alive,
        Liveness::Alive,
        GRACE,
    );
    assert_eq!(action, Action::Keep);
}

#[test]
fn dead_owner_inside_grace_is_kept() {
    let grace_ms = GRACE.as_millis() as u64;
    let action = decide(
        &record(1_000),
        1_000 + grace_ms - 1,
        Liveness::Gone,
        Liveness::Alive,
        GRACE,
    );
    assert_eq!(action, Action::Keep);
}

#[test]
fn dead_owner_after_grace_reaps_live_driver_and_forgets_gone_driver() {
    let grace_ms = GRACE.as_millis() as u64;
    assert_eq!(
        decide(
            &record(1_000),
            1_000 + grace_ms,
            Liveness::Gone,
            Liveness::Alive,
            GRACE
        ),
        Action::Reap
    );
    assert_eq!(
        decide(
            &record(1_000),
            1_000 + grace_ms,
            Liveness::Gone,
            Liveness::Gone,
            GRACE
        ),
        Action::Forget
    );
}

#[test]
fn record_round_trips_and_forget_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ProcId {
        pid: 4242,
        start: 9,
    };
    let driver = ProcId {
        pid: 4343,
        start: 11,
    };
    write(dir.path(), owner, driver).unwrap();
    let path = record_path(dir.path(), 4242, 4343);
    let parsed: Record = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(parsed.owner, owner);
    assert_eq!(parsed.driver, driver);
    forget(dir.path(), 4242, 4343);
    assert!(!path.exists());
}

#[test]
fn sweep_ignores_non_record_files_and_keeps_fresh_records() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
    write(
        dir.path(),
        ProcId { pid: 7, start: 5 },
        ProcId { pid: 8, start: 6 },
    )
    .unwrap();
    // The owner identity matches no real process, so the owner reads as gone, but the record
    // was just touched, so the grace window keeps it.
    let reaped = sweep(dir.path(), |_| panic!("fresh record must not be reaped"));
    assert_eq!(reaped, 0);
    assert!(record_path(dir.path(), 7, 8).exists());
    assert!(dir.path().join("notes.txt").exists());
}

#[test]
fn legacy_record_without_identity_is_removed_and_never_signalled() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("1-2.json");
    std::fs::write(
        &legacy,
        br#"{"owner_pid":1,"owner_start":"x","driver_pid":2,"driver_start":"y","touched_ms":0}"#,
    )
    .unwrap();
    let reaped = sweep(dir.path(), |_| {
        panic!("unverifiable record must not be signalled")
    });
    assert_eq!(reaped, 0);
    assert!(!legacy.exists());
}

#[test]
fn unverifiable_driver_is_never_signalled_even_after_grace() {
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("7-8.json");
    let stale_record = Record {
        owner: ProcId { pid: 7, start: 5 },
        driver: ProcId { pid: 8, start: 0 },
        dead_since_ms: Some(0),
    };
    std::fs::write(&stale, serde_json::to_vec(&stale_record).unwrap()).unwrap();
    let reaped = sweep(dir.path(), |_| {
        panic!("unverifiable driver must not be signalled")
    });
    assert_eq!(reaped, 0);
    assert!(!stale.exists());
}
