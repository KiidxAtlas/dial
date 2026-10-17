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
