use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;
use std::fs;

fn fixture() -> (tempfile::TempDir, Workspace) {
    let (dir, ws) = testkit::workspace();
    let conn = Connection::open(dir.path().join("sample.db")).unwrap();
    conn.execute_batch("CREATE TABLE parent(id INTEGER PRIMARY KEY, label TEXT NOT NULL); CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id), payload BLOB); CREATE INDEX child_parent ON child(parent_id); INSERT INTO parent VALUES(9007199254740993,'one'); INSERT INTO child VALUES(1,9007199254740993,x'0001'); INSERT INTO child VALUES(2,9007199254740993,x'02');").unwrap();
    drop(conn);
    (dir, ws)
}
fn call(ws: &Workspace, action: &str, sql: Option<&str>) -> Result<ToolOutput> {
    let mut args = json!({"action":action,"path":"sample.db"});
    if let Some(sql) = sql {
        args["sql"] = json!(sql);
    }
    run(ws, &args)
}
#[test]
fn schema_query_plan_and_precision() {
    let (_dir, ws) = fixture();
    let schema = call(&ws, "schema", None).unwrap().data.unwrap();
    let text = schema.to_string();
    assert!(
        text.contains("parent_id")
            && text.contains("child_parent")
            && text.contains("foreign_keys")
    );
    let data = call(&ws, "query", Some("SELECT id, label FROM parent"))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(
        data["observation"]["rows"][0][0]["integer"],
        "9007199254740993"
    );
    let blob = call(&ws, "query", Some("SELECT payload FROM child"))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(blob["observation"]["rows"][0][0]["blob_bytes"], 2);
    let plan = call(&ws, "plan", Some("SELECT id FROM child WHERE parent_id=1"))
        .unwrap()
        .data
        .unwrap();
    assert!(plan.to_string().contains("SEARCH"));
    assert_eq!(plan["verification_credit"], false);
}
#[test]
fn rejects_mutation_attachments_pragmas_extensions_virtual_functions_and_tails() {
    let (dir, ws) = fixture();
    let before = fs::read(dir.path().join("sample.db")).unwrap();
    for sql in [
        "DELETE FROM parent",
        "WITH x AS (SELECT 1) DELETE FROM parent",
        "ATTACH ':memory:' AS other",
        "PRAGMA user_version=1",
        "SELECT load_extension('evil')",
        "SELECT readfile('secret')",
        "SELECT * FROM pragma_database_list",
        "SELECT * FROM json_each('[1]')",
        "SELECT 1; SELECT 2",
        "SELECT 1;",
        "SELECT randomblob(999999999)",
    ] {
        assert!(call(&ws, "query", Some(sql)).is_err(), "accepted {sql}");
        assert!(call(&ws, "plan", Some(sql)).is_err(), "plan accepted {sql}");
    }
    assert_eq!(before, fs::read(dir.path().join("sample.db")).unwrap());
}
#[test]
fn bounds_rows_cells_work_and_cancellation() {
    let (_dir, ws) = fixture();
    let out = run(
        &ws,
        &json!({"action":"query","path":"sample.db","sql":"SELECT * FROM child","limit":1}),
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(out["observation"]["truncated"], true);
    assert_eq!(out["observation"]["returned_rows"], 1);
    let expensive = "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<1000000000) SELECT sum(n) FROM x";
    let start = Instant::now();
    assert!(call(&ws, "query", Some(expensive)).is_err());
    assert!(start.elapsed() < Duration::from_secs(10));
    let cancel = AtomicBool::new(true);
    assert!(run_with_cancel(&ws, &json!({"action":"schema","path":"sample.db"}), &cancel).is_err());
    assert!(
        run(
            &ws,
            &json!({"action":"query","path":"sample.db","sql":"SELECT 1","limit":201})
        )
        .is_err()
    );
    let c = cell(ValueRef::Text(&[b'a'; MAX_CELL + 1]));
    assert_eq!(c["truncated"], true);
    assert_eq!(c["text"].as_str().unwrap().len(), MAX_CELL);
}
#[test]
fn refuses_companions_wal_and_virtual_tables() {
    let (dir, ws) = fixture();
    for suffix in ["-wal", "-shm", "-journal"] {
        fs::write(dir.path().join(format!("sample.db{suffix}")), []).unwrap();
        assert!(call(&ws, "schema", None).is_err());
        fs::remove_file(dir.path().join(format!("sample.db{suffix}"))).unwrap();
    }
    let conn = Connection::open(dir.path().join("sample.db")).unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE ft USING fts5(content)")
        .unwrap();
    drop(conn);
    assert!(call(&ws, "schema", None).is_err());
    let mut bytes = fs::read(dir.path().join("sample.db")).unwrap();
    bytes[18] = 2;
    fs::write(dir.path().join("sample.db"), bytes).unwrap();
    assert!(call(&ws, "query", Some("SELECT 1")).is_err());
}
#[test]
fn source_confinement_and_policy_are_preserved() {
    let (dir, mut ws) = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::copy(
        dir.path().join("sample.db"),
        outside.path().join("outside.db"),
    )
    .unwrap();
    assert!(
        run(
            &ws,
            &json!({"action":"schema","path":outside.path().join("outside.db")})
        )
        .is_err()
    );
    ws.policy = Some(crate::tools::exec::sandbox::Policy::Dev {
        deny_read: vec![dir.path().join("sample.db")],
        extra_write: vec![],
        allow_network: false,
    });
    assert!(call(&ws, "schema", None).is_err());
}
#[test]
fn retains_only_observation_with_original_identity() {
    let (dir, ws) = fixture();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let out = call(&ws, "query", Some("SELECT id FROM parent")).unwrap();
    let data = out.data.unwrap();
    let store = ws.artifacts().unwrap();
    let record = store
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    let snapshot = record.evidence.context.snapshot.as_ref().unwrap();
    assert_eq!(
        snapshot.sha256,
        source::snapshot(&fs::read(dir.path().join("sample.db")).unwrap())
            .unwrap()
            .sha256
    );
    assert!(snapshot.artifact.is_none());
    let retained = store
        .evidence_bytes(&record, MAX_OUTPUT + MAX_SQL + 8192)
        .unwrap();
    let observation: Value = serde_json::from_slice(&retained).unwrap();
    assert_eq!(observation["type"], "sqlite_observation");
    assert!(!retained.starts_with(b"SQLite format 3"));
}

#[test]
fn text_cell_omission_propagates_to_observation_and_evidence_coverage() {
    let (dir, ws) = fixture();
    let conn = Connection::open(dir.path().join("sample.db")).unwrap();
    conn.execute("UPDATE parent SET label = ?1", ["x".repeat(MAX_CELL + 1)])
        .unwrap();
    drop(conn);
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let data = call(&ws, "query", Some("SELECT label FROM parent"))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(data["observation"]["truncated_cells"], 1);
    assert_eq!(data["observation"]["truncated"], true);
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert!(record.evidence.context.coverage.unwrap().truncated);
}

#[test]
fn admitted_cross_join_is_interrupted_by_work_budget_and_live_cancellation() {
    let (_dir, ws) = fixture();
    let tables = (0..26)
        .map(|i| format!("child AS c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT count(*) FROM {tables}");
    let error = call(&ws, "query", Some(&sql)).unwrap_err();
    assert!(
        format!("{error:#}").contains("interrupted"),
        "expected actual work-budget interruption: {error:#}"
    );
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(5));
            cancel.store(true, Ordering::Relaxed);
        });
        assert!(
            run_with_cancel(
                &ws,
                &json!({"action":"query","path":"sample.db","sql":sql}),
                &cancel
            )
            .is_err()
        );
    });
    assert!(cancel.load(Ordering::Relaxed));
}

#[test]
fn materializing_sort_hits_worker_heap_cap_without_constraining_parent_sqlite() {
    let (dir, ws) = fixture();
    let conn = Connection::open(dir.path().join("sample.db")).unwrap();
    conn.execute_batch("CREATE TABLE wide(value TEXT NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO wide VALUES (?1)", ["x".repeat(256 * 1024)])
        .unwrap();
    let parent_limit: i64 = conn
        .query_row("PRAGMA hard_heap_limit", [], |r| r.get(0))
        .unwrap();
    // Only 256 KiB of source data, but the sorter would retain 256 MiB of repeated values.
    // No SQL LIMIT: the tool's output-row limit must not be mistaken for an execution-memory cap.
    let tables = (0..10)
        .map(|i| format!("child AS c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let key = (0..10)
        .map(|i| format!("c{i}.id"))
        .collect::<Vec<_>>()
        .join("+");
    let sql = format!("SELECT wide.value FROM wide,{tables} ORDER BY {key}");
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let error = call(&ws, "query", Some(&sql)).unwrap_err();
    let message = format!("{error:#}").to_ascii_lowercase();
    assert!(
        message.contains("memory"),
        "expected allocator ceiling, not work/time rejection: {message}"
    );
    let after: i64 = conn
        .query_row("PRAGMA hard_heap_limit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        parent_limit, after,
        "worker must not change the agent's SQLite allocator ceiling"
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM child", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let data = call(&ws, "query", Some("SELECT id FROM parent"))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(data["limits"]["sqlite_heap_bytes"], SQLITE_HEAP_BYTES);
    assert_eq!(data["observation"]["returned_rows"], 1);
}
