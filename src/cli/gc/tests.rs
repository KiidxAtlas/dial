use super::*;
use std::time::{Duration, SystemTime};

const NOW: i64 = 1_800_000_000_000;

fn layout(root: &Path) -> Layout {
    let cwd = root.join("project");
    let user = root.join("user");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&user).unwrap();
    Layout::with_user_dir(&cwd, user).unwrap()
}

fn args() -> GcArgs {
    GcArgs {
        apply: false,
        json: false,
        blobs: false,
        archive: false,
        wal: false,
        stale: false,
        cold_archive_after_days: None,
        retain_newest_global: None,
        retain_newest_per_cwd: None,
        stale_retain_newest: None,
        stale_retain_days: None,
    }
}

fn set_age(path: &Path, age: Duration) {
    let when = SystemTime::now() - age;
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

fn write_session(layout: &Layout, file: &str, cwd: &str, age: Duration) -> PathBuf {
    let dir = layout.sessions_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(file);
    let header = serde_json::json!({"type": "session", "id": "x", "cwd": cwd});
    fs::write(&path, format!("{header}\n{{\"type\":\"message\"}}\n")).unwrap();
    set_age(&path, age);
    path
}

#[test]
fn default_selection_is_blobs_archive_wal_without_stale() {
    let s = select(&args());
    assert_eq!(
        s,
        Selection {
            stale: false,
            blobs: true,
            archive: true,
            wal: true
        }
    );
}

#[test]
fn any_sweep_flag_selects_only_flagged_sweeps() {
    let mut a = args();
    a.stale = true;
    assert_eq!(
        select(&a),
        Selection {
            stale: true,
            blobs: false,
            archive: false,
            wal: false
        }
    );
    let mut b = args();
    b.wal = true;
    assert_eq!(
        select(&b),
        Selection {
            stale: false,
            blobs: false,
            archive: false,
            wal: true
        }
    );
}

#[test]
fn default_settings_match_omp() {
    let s = applied_settings(&args());
    assert_eq!(s.cold_archive_after_days, 30);
    assert_eq!(s.retain_newest_global, 20);
    assert_eq!(s.retain_newest_per_cwd, 10);
    assert_eq!(s.stale_retain_newest, 20);
    assert_eq!(s.stale_retain_days, 30);
}

#[test]
fn expired_beyond_keep_protects_newest_and_respects_cutoff() {
    let items = vec![("old", 10), ("new", 100), ("mid", 50)];
    let (kept, expired) = expired_beyond_keep(items, 1, 60);
    assert_eq!(kept, 1);
    assert_eq!(expired, vec!["mid", "old"]);
}

#[test]
fn artifact_hash_requires_lowercase_64_hex() {
    let good = "a".repeat(64);
    assert!(is_artifact_hash(&good));
    assert!(!is_artifact_hash(&"A".repeat(64)));
    assert!(!is_artifact_hash("abc"));
}

#[test]
fn dry_run_archive_leaves_sessions_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let path = write_session(
        &l,
        "20200101T000000000Z_old1.jsonl",
        "/p",
        Duration::from_secs(90 * 86_400),
    );
    let mut a = args();
    a.archive = true;
    a.cold_archive_after_days = Some(30);
    a.retain_newest_global = Some(0);
    a.retain_newest_per_cwd = Some(0);
    let report = execute(&l, &a, NOW).unwrap();
    let archive = report.archive.unwrap();
    assert_eq!(archive.scanned, 1);
    assert_eq!(archive.would_archive, 1);
    assert_eq!(archive.archived, 0);
    assert!(path.exists());
    assert!(!l.user_dir.join(retention::DB_FILE).exists());
}

#[test]
fn apply_archive_moves_session_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let path = write_session(
        &l,
        "20200101T000000000Z_old2.jsonl",
        "/proj",
        Duration::from_secs(90 * 86_400),
    );
    let mut a = args();
    a.apply = true;
    a.archive = true;
    a.retain_newest_global = Some(0);
    a.retain_newest_per_cwd = Some(0);
    let report = execute(&l, &a, retention::now_ms()).unwrap();
    let archive = report.archive.unwrap();
    assert_eq!(archive.archived, 1, "errors: {:?}", archive.errors);
    assert!(!path.exists());
    let dest = l
        .sessions_dir()
        .join(ARCHIVE_DIR)
        .join(path.file_name().unwrap());
    assert!(dest.exists());
    let store = Store::open(&l.user_dir).unwrap();
    let record = store.archived("old2").unwrap().expect("record stored");
    assert_eq!(record.cwd, "/proj");
    assert_eq!(record.archived_path, dest.display().to_string());
    assert!(store.last_run_settings().unwrap().is_some());
}

#[test]
fn restore_archived_returns_the_pair_or_keeps_it_together_in_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let sessions = l.sessions_dir();
    let archive = sessions.join(ARCHIVE_DIR);
    fs::create_dir_all(&archive).unwrap();
    let archived_pair = |name: &str| {
        let original = sessions.join(name);
        let dest = archive.join(name);
        fs::write(&dest, "{}\n").unwrap();
        let streams = crate::runtime::stream_journal::path(&dest);
        fs::write(&streams, "stream\n").unwrap();
        (original, dest, streams)
    };
    let (original, dest, streams) = archived_pair("20200101T000000000Z_back.jsonl");
    restore_archived(&dest, &original).unwrap();
    assert!(original.exists() && !dest.exists());
    assert!(crate::runtime::stream_journal::path(&original).exists());
    assert!(!streams.exists());

    // A directory at the original path makes the main rename fail; the stream journal must
    // then stay beside the archived main file rather than return alone.
    let (blocked, dest, streams) = archived_pair("20200101T000000000Z_blocked.jsonl");
    fs::create_dir(&blocked).unwrap();
    let error = restore_archived(&dest, &blocked).unwrap_err();
    assert!(error.starts_with("restoring"), "{error}");
    assert!(dest.exists());
    assert!(
        streams.exists(),
        "the stream journal must stay with its archived session"
    );
    assert!(!crate::runtime::stream_journal::path(&blocked).exists());
}

#[test]
fn recent_and_protected_sessions_are_not_archived() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    write_session(
        &l,
        "20260101T000000000Z_fresh.jsonl",
        "/p",
        Duration::from_secs(10),
    );
    write_session(
        &l,
        "20200101T000000000Z_keep.jsonl",
        "/p",
        Duration::from_secs(90 * 86_400),
    );
    let mut a = args();
    a.archive = true;
    a.retain_newest_global = Some(1);
    a.retain_newest_per_cwd = Some(0);
    let archive = execute(&l, &a, retention::now_ms())
        .unwrap()
        .archive
        .unwrap();
    assert_eq!(archive.skipped_recent, 1);
    assert_eq!(archive.kept_newest_global, 1);
    assert_eq!(archive.would_archive, 0);
}

#[test]
fn blobs_keep_referenced_and_remove_old_unreferenced() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(artifacts.join(LIFECYCLE_DIR)).unwrap();
    let kept = "b".repeat(64);
    let orphan = "c".repeat(64);
    let transitive = "d".repeat(64);
    fs::write(artifacts.join(&kept), "body").unwrap();
    fs::write(artifacts.join(&orphan), "body").unwrap();
    fs::write(artifacts.join(&transitive), "body").unwrap();
    fs::write(
        artifacts.join(LIFECYCLE_DIR).join(format!("{orphan}.json")),
        "{}",
    )
    .unwrap();
    for name in [&kept, &orphan, &transitive] {
        set_age(&artifacts.join(name), Duration::from_secs(3600));
    }
    // Referenced from a session; the referenced blob itself mentions `transitive`.
    let session_dir = l.sessions_dir();
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("20260101T000000000Z_s.jsonl"),
        format!("artifact://{kept}\n"),
    )
    .unwrap();
    fs::write(
        artifacts.join(&kept),
        format!("see artifact://{transitive}"),
    )
    .unwrap();

    let mut a = args();
    a.apply = true;
    a.blobs = true;
    let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
    assert_eq!(blobs.candidates, 1, "errors: {:?}", blobs.errors);
    assert_eq!(blobs.deleted, 1);
    assert!(artifacts.join(&kept).exists());
    assert!(artifacts.join(&transitive).exists());
    assert!(!artifacts.join(&orphan).exists());
    assert!(
        !artifacts
            .join(LIFECYCLE_DIR)
            .join(format!("{orphan}.json"))
            .exists()
    );
}

#[test]
fn live_spool_is_kept_until_its_owner_finishes() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(&artifacts)
        .unwrap()
        .with_limits(&crate::storage::artifact::ArtifactConfig {
            max_bytes: 64,
            head_bytes: 16,
        });
    let mut spool = store.spool().unwrap();
    // Past the head budget: the head stops changing while the tail keeps being written.
    spool.write_all(&[b'x'; 1000]).unwrap();
    let spool_files: Vec<PathBuf> = fs::read_dir(&artifacts)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SPOOL_PREFIX))
        })
        .collect();
    assert_eq!(spool_files.len(), 2, "head and tail: {spool_files:?}");
    for path in &spool_files {
        set_age(path, Duration::from_secs(3600));
    }
    let mut a = args();
    a.apply = true;
    a.blobs = true;
    let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
    assert_eq!(blobs.deleted, 0, "errors: {:?}", blobs.errors);
    for path in &spool_files {
        assert!(path.exists(), "live spool file removed: {}", path.display());
    }
    spool.write_all(b"tail").unwrap();
    let uri = spool.finish().unwrap();
    let id = uri.trim_start_matches("artifact://");
    assert!(artifacts.join(id).exists());
}

#[test]
fn abandoned_unclaimed_spool_is_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).unwrap();
    let head = artifacts.join(format!("{SPOOL_PREFIX}999-0"));
    let tail = head.with_extension("tail");
    fs::write(&head, "head").unwrap();
    fs::write(&tail, "tail").unwrap();
    set_age(&head, Duration::from_secs(3600));
    set_age(&tail, Duration::from_secs(3600));
    let mut a = args();
    a.apply = true;
    a.blobs = true;
    let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
    assert_eq!(blobs.deleted, 2, "errors: {:?}", blobs.errors);
    assert!(!head.exists() && !tail.exists());
}

#[test]
fn failed_transitive_read_deletes_nothing_and_reports_incomplete_scan() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).unwrap();
    let unreadable = "a".repeat(64);
    let orphan = "c".repeat(64);
    // A directory where the referenced body should be: reading it fails with a real error,
    // not NotFound, so the scan must fail closed.
    fs::create_dir(artifacts.join(&unreadable)).unwrap();
    fs::write(artifacts.join(&orphan), "body").unwrap();
    set_age(&artifacts.join(&orphan), Duration::from_secs(3600));
    let session_dir = l.sessions_dir();
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("20260101T000000000Z_s.jsonl"),
        format!("artifact://{unreadable}\n"),
    )
    .unwrap();
    let mut a = args();
    a.apply = true;
    a.blobs = true;
    let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
    assert_eq!(blobs.deleted, 0);
    assert!(
        blobs
            .errors
            .iter()
            .any(|e| e.contains("reference scan incomplete") && e.contains(&unreadable)),
        "errors: {:?}",
        blobs.errors
    );
    assert!(artifacts.join(&orphan).exists());
}

#[test]
fn referenced_body_that_is_absent_is_missing_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).unwrap();
    let absent = "9".repeat(64);
    let session_dir = l.sessions_dir();
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("20260101T000000000Z_m.jsonl"),
        format!("artifact://{absent}\n"),
    )
    .unwrap();
    let scan = collect_references(&l).unwrap();
    assert!(scan.refs.contains(&absent));
    assert_eq!(scan.missing, vec![absent]);
}

#[test]
fn blobs_skip_fresh_unreferenced_files_inside_grace() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let artifacts = l.state_dir.join(ARTIFACTS_DIR);
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("e".repeat(64)), "new").unwrap();
    let mut a = args();
    a.blobs = true;
    let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
    assert_eq!(blobs.candidates, 1);
    assert_eq!(blobs.would_delete, 0);
}

#[test]
fn stale_prunes_dangling_records_beyond_keep_window() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let lifecycle = l.state_dir.join(ARTIFACTS_DIR).join(LIFECYCLE_DIR);
    fs::create_dir_all(&lifecycle).unwrap();
    let old = "f".repeat(64);
    let fresh_keep = "1".repeat(64);
    fs::write(lifecycle.join(format!("{old}.json")), "{}").unwrap();
    fs::write(lifecycle.join(format!("{fresh_keep}.json")), "{}").unwrap();
    set_age(
        &lifecycle.join(format!("{old}.json")),
        Duration::from_secs(90 * 86_400),
    );
    let mut a = args();
    a.apply = true;
    a.stale = true;
    a.stale_retain_newest = Some(1);
    let stale = execute(&l, &a, retention::now_ms()).unwrap().stale.unwrap();
    assert_eq!(stale.dangling_records, 2);
    assert_eq!(stale.kept_newest, 1);
    assert_eq!(stale.deleted, 1, "errors: {:?}", stale.errors);
    assert!(!lifecycle.join(format!("{old}.json")).exists());
}

#[test]
fn wal_reports_size_in_dry_run_and_checkpoints_on_apply() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let db = l.user_dir.join("stats.sqlite3");
    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .unwrap();
    drop(conn);
    // Dry-run never checkpoints.
    let mut a = args();
    a.wal = true;
    let dry = execute(&l, &a, retention::now_ms()).unwrap().wal.unwrap();
    assert_eq!(dry.databases.len(), 1);
    assert!(!dry.checkpointed);
    a.apply = true;
    let applied = execute(&l, &a, retention::now_ms()).unwrap().wal.unwrap();
    assert!(applied.errors.is_empty(), "errors: {:?}", applied.errors);
    assert!(applied.databases[0].checkpointed);
    assert_eq!(wal_size(&db), 0);
}

#[test]
fn json_report_uses_camel_case_keys() {
    let dir = tempfile::tempdir().unwrap();
    let l = layout(dir.path());
    let report = execute(&l, &args(), NOW).unwrap();
    let json: Value = serde_json::to_value(&report).unwrap();
    assert!(json.get("stateDir").is_some());
    assert!(json.get("lockPath").is_some());
    assert_eq!(json["apply"], Value::Bool(false));
    assert!(json.get("archive").is_some());
    assert!(json.get("stale").is_none());
    assert!(json["settings"].get("coldArchiveAfterDays").is_some());
}

#[test]
fn exit_code_reflects_errors() {
    let report = GcReport {
        state_dir: String::new(),
        lock_path: String::new(),
        apply: false,
        settings: applied_settings(&args()),
        stale: None,
        blobs: Some(BlobResult {
            errors: vec!["boom".into()],
            ..Default::default()
        }),
        archive: None,
        wal: None,
    };
    assert_eq!(report.errors(), vec!["boom"]);
}
