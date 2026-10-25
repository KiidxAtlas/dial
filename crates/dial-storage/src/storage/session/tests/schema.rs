use super::*;

fn write_journal(e: &Env, name: &str, header: Value) -> PathBuf {
    let dir = e.layout.sessions_dir();
    crate::platform::atomic::private_dir(&dir).unwrap();
    let path = dir.join(name);
    let body = format!(
        "{header}\n{}\n{}\n",
        journal_message("m1", None, "hello"),
        journal_message("m2", Some("m1"), "again"),
    );
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn new_sessions_declare_the_current_schema_version() {
    let e = env();
    let s = new_session(&e);
    let header = fs::read_to_string(s.path().unwrap()).unwrap();
    let header: Value = serde_json::from_str(header.lines().next().unwrap()).unwrap();
    assert_eq!(header["schema_version"], migrate::CURRENT_SCHEMA_VERSION);
    assert!(header["version"].is_string(), "release version is kept");
    assert_eq!(
        s.share_records()[0]["schema_version"],
        migrate::CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn legacy_journals_without_a_schema_version_load_and_are_not_rewritten() {
    let e = env();
    let path = write_journal(&e, "20260101T000000000Z_abc123.jsonl", {
        json!({"type":"session","id":"abc123","started":"2026-01-01T00:00:00.000Z","model":"m","version":"0.1.0"})
    });
    let before = fs::read(&path).unwrap();
    let s = Session::read(&path).unwrap();
    assert_eq!(contents(&s.messages()), ["hello", "again"]);
    assert_eq!(fs::read(&path).unwrap(), before);
    let mut s = Session::open(&path).unwrap();
    s.append(user("third")).unwrap();
    let after = fs::read_to_string(&path).unwrap();
    assert!(
        after.starts_with(
            std::str::from_utf8(&before)
                .unwrap()
                .lines()
                .next()
                .unwrap()
        )
    );
    assert!(!after.lines().next().unwrap().contains("schema_version"));
}

#[test]
fn journals_from_a_newer_schema_are_refused_for_read_and_write() {
    let e = env();
    let path = write_journal(&e, "20260101T000000000Z_abc124.jsonl", {
        json!({"type":"session","id":"abc124","started":"2026-01-01T00:00:00.000Z","model":"m",
               "schema_version": migrate::CURRENT_SCHEMA_VERSION + 1})
    });
    for result in [Session::read(&path), Session::open(&path)] {
        let error = format!("{:#}", result.err().expect("future journal is refused"));
        assert!(
            error.contains("newer") && error.contains("update Dial"),
            "{error}"
        );
    }
    // A refused open leaves the writer lock free for a build that understands it.
    assert!(!Session::open_elsewhere(&path));
}

#[test]
fn unknown_record_types_and_labels_from_a_labeled_journal_load() {
    let e = env();
    let path = write_journal(&e, "20260101T000000000Z_abc125.jsonl", {
        json!({"type":"session","id":"abc125","started":"2026-01-01T00:00:00.000Z","model":"m",
               "schema_version": migrate::CURRENT_SCHEMA_VERSION})
    });
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(
        &json!({"type":"label","id":"l1","parent":"m2","ts":"t","target":"m1","label":"start"})
            .to_string(),
    );
    text.push('\n');
    text.push_str(&json!({"type":"future_thing","id":"x1","parent":"l1","ts":"t"}).to_string());
    text.push('\n');
    fs::write(&path, text).unwrap();
    let mut s = Session::read(&path).unwrap();
    assert_eq!(s.label_of("m1"), Some("start"));
    assert!(s.take_recovery_diagnostics().is_empty());
    assert_eq!(contents(&s.messages()), ["hello", "again"]);
}
