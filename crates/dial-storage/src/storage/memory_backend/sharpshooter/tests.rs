use super::*;
use crate::storage::memory_backend::test_support::{FakeRunner, authority, scope};

fn layout(dir: &std::path::Path) -> Layout {
    let project = dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    Layout::with_user_dir(&project, dir.join("home")).unwrap()
}

fn extractions(runner: &FakeRunner) -> usize {
    runner
        .systems
        .iter()
        .filter(|system| system.starts_with("Extract durable PROJECT DECISIONS"))
        .count()
}

#[test]
fn envelopes_left_by_earlier_sessions_are_extracted() {
    let dir = tempfile::tempdir().unwrap();
    let mut driver = open(&layout(dir.path()), &SharpshooterConfig::default()).unwrap();
    let earlier = Operation::for_test(authority("session-a"), scope(), None);
    let messages = vec![
        json!({"role":"user","content":"we always keep the parser free of allocation in hot loops"}),
    ];
    driver.observe(&earlier, &messages).unwrap();
    // The session ended before maintenance ran; a later session picks the envelope up.
    let later = Operation::for_test(authority("session-b"), scope(), None);
    let mut runner = FakeRunner::new(r#"{"deltas":[]}"#);
    driver.maintain(&later, &mut runner).unwrap();
    assert_eq!(extractions(&runner), 1);
    driver.maintain(&later, &mut runner).unwrap();
    assert_eq!(
        extractions(&runner),
        1,
        "a processed envelope is not extracted again"
    );
}

#[test]
fn recall_matches_query_words_not_only_the_exact_phrase() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let mut driver = open(&layout, &SharpshooterConfig::default()).unwrap();
    // Seed a decision file through a second connection to the driver's own store.
    let key = format!(
        "{:x}",
        Sha256::digest(layout.project_root.as_os_str().as_encoded_bytes())
    );
    PrivateDirectory::open(&layout, "sharpshooter")
        .unwrap()
        .child(&key)
        .unwrap()
        .database("decisions.sqlite3")
        .unwrap()
        .execute(
            "INSERT INTO files(name,content)VALUES('architecture.md',?1)",
            ["- Keep the parser allocation-free in hot loops\n- Tabs for indentation"],
        )
        .unwrap();
    let op = Operation::for_test(authority("session-a"), scope(), None);
    let mut runner = FakeRunner::new("{}");
    let found = driver
        .search(&op, "parser allocations", 8, &mut runner)
        .unwrap();
    assert_eq!(found["count"], 1, "{found}");
    assert!(
        found["items"][0]["content"]
            .as_str()
            .unwrap()
            .contains("parser")
    );
}

#[test]
fn pending_deltas_remain_due_after_a_partial_consolidation() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let mut driver = open(&layout, &SharpshooterConfig::default()).unwrap();
    let key = format!(
        "{:x}",
        Sha256::digest(layout.project_root.as_os_str().as_encoded_bytes())
    );
    let db = PrivateDirectory::open(&layout, "sharpshooter")
        .unwrap()
        .child(&key)
        .unwrap()
        .database("decisions.sqlite3")
        .unwrap();
    db.execute("INSERT INTO deltas(id,session,branch,revision,payload,created) VALUES('remaining','s','b','r','{}',1)", []).unwrap();
    db.execute("UPDATE state SET last_consolidated=2 WHERE id=1", [])
        .unwrap();
    let op = Operation::for_test(authority("session"), scope(), None);
    assert!(
        driver.maintenance_due(&op).unwrap(),
        "older unconsumed deltas must remain eligible"
    );
    db.execute("UPDATE state SET lease_until=?1 WHERE id=1", [now() + 3600])
        .unwrap();
    assert!(
        !driver.maintenance_due(&op).unwrap(),
        "an active consolidation lease must still exclude work"
    );
    db.execute(
        "UPDATE state SET lease_until=0,last_consolidated=?1 WHERE id=1",
        [now()],
    )
    .unwrap();
    assert!(
        !driver.maintenance_due(&op).unwrap(),
        "the configured interval still applies"
    );
    db.execute("DELETE FROM deltas", []).unwrap();
    db.execute("UPDATE state SET last_consolidated=0 WHERE id=1", [])
        .unwrap();
    assert!(
        !driver.maintenance_due(&op).unwrap(),
        "an empty queue is not due"
    );
}
