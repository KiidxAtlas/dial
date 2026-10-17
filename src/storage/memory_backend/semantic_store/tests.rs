use super::*;

fn store() -> Store {
    Store::new(Connection::open_in_memory().unwrap()).unwrap()
}

fn created(saved: Saved) -> Fact {
    match saved {
        Saved::Created(fact) => fact,
        Saved::Existing(_) => panic!("expected a new fact"),
    }
}

#[test]
fn identical_content_is_deduplicated() {
    let mut store = store();
    let first = created(store.retain("ship on fridays", "test", 1).unwrap());
    match store.retain("ship on fridays", "test", 2).unwrap() {
        Saved::Existing(fact) => assert_eq!(fact.id, first.id),
        Saved::Created(_) => panic!("duplicate must not create a second fact"),
    }
}

#[test]
fn forget_requires_current_revision_and_hides_the_fact() {
    let mut store = store();
    let fact = created(store.retain("temporary plan", "test", 1).unwrap());
    assert!(store.forget(fact.id, fact.revision + 5, 2).is_err());
    store.forget(fact.id, fact.revision, 2).unwrap();
    assert!(store.fact(fact.id).unwrap().is_none());
    assert!(store.recent(10).unwrap().is_empty());
    assert!(store.lexical("temporary", 10).unwrap().is_empty());
}

#[test]
fn update_bumps_revision_and_keeps_history() {
    let mut store = store();
    let fact = created(store.retain("rate limit is 10 rps", "test", 1).unwrap());
    assert!(
        store
            .update(fact.id, fact.revision + 1, "x", "test", "fix", 2)
            .is_err(),
        "a stale revision must not overwrite a newer fact"
    );
    let updated = store
        .update(
            fact.id,
            fact.revision,
            "rate limit is 20 rps",
            "test",
            "fix",
            2,
        )
        .unwrap();
    assert_eq!(updated.revision, fact.revision + 1);
    assert_eq!(updated.content, "rate limit is 20 rps");
    let history = store.history(fact.id).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].revision, 2);
    assert_eq!(history[0].content.as_deref(), Some("rate limit is 20 rps"));
    assert_eq!(history[1].revision, 1);
    assert_eq!(history[1].content.as_deref(), Some("rate limit is 10 rps"));
}

#[test]
fn clear_all_forgets_every_live_fact() {
    let mut store = store();
    store.retain("one", "test", 1).unwrap();
    store.retain("two", "test", 2).unwrap();
    assert_eq!(store.clear_all(3).unwrap(), 2);
    assert!(store.recent(10).unwrap().is_empty());
}

#[test]
fn clear_all_drops_queued_turns() {
    let mut store = store();
    store.retain("one", "test", 1).unwrap();
    assert!(store.enqueue_turn("s", "prompt", "answer", 2).unwrap());
    store.clear_all(3).unwrap();
    assert!(store.next_turns(10).unwrap().is_empty());
    assert!(
        !store.enqueue_turn("s", "prompt", "answer", 4).unwrap(),
        "a cleared turn is not queued again"
    );
}

#[test]
fn two_writers_on_one_database_file_lose_no_facts() {
    let path = std::env::temp_dir().join(format!(
        "dial-semantic-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let open = || {
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        Store::new(conn).unwrap()
    };
    let mut first = open();
    let mut second = open();
    let a = std::thread::spawn(move || {
        for index in 0..40 {
            first
                .retain(&format!("writer a fact {index}"), "test", index)
                .unwrap();
        }
    });
    let b = std::thread::spawn(move || {
        for index in 0..40 {
            second
                .retain(&format!("writer b fact {index}"), "test", index)
                .unwrap();
        }
    });
    a.join().unwrap();
    b.join().unwrap();
    let check = open();
    let (live, _, _) = check.counts(None).unwrap();
    drop(check);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    assert_eq!(live, 80);
}

#[test]
fn racing_updates_with_one_revision_have_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite3");
    let open = || {
        let conn = Connection::open(&path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        Store::new(conn).unwrap()
    };
    let seed = created(open().retain("rate limit is 10 rps", "test", 1).unwrap());
    let (id, revision) = (seed.id, seed.revision);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let contenders: Vec<_> = ["rate limit is 20 rps", "rate limit is 30 rps"]
        .into_iter()
        .map(|content| {
            let barrier = barrier.clone();
            let mut store = open();
            std::thread::spawn(move || {
                barrier.wait();
                store.update(id, revision, content, "test", "fix", 2)
            })
        })
        .collect();
    let results: Vec<_> = contenders.into_iter().map(|t| t.join().unwrap()).collect();
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one update may use the same revision"
    );
    assert_eq!(winners[0].revision, seed.revision + 1);
    let check = open();
    let current = check.fact(seed.id).unwrap().unwrap();
    assert_eq!(current.revision, seed.revision + 1);
    assert_eq!(current.content, winners[0].content);
    assert_eq!(check.history(seed.id).unwrap().len(), 2);
    drop(check);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}
