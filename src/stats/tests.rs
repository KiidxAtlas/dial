use super::*;
use serde_json::json;

fn request(
    ts_ms: i64,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cost: Option<f64>,
    duration_ms: u64,
) -> RequestRecord {
    RequestRecord {
        ts_ms,
        session: "s1".into(),
        project: "proj-0000aaaa".into(),
        model: model.into(),
        input,
        output,
        cache_read,
        reported: true,
        cost_usd: cost,
        duration_ms,
        finish_reason: "stop".into(),
        ..Default::default()
    }
}

#[test]
fn legacy_request_table_gains_wire_columns_and_records_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(dir.path());
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE requests(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, session TEXT NOT NULL,
                  project TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
                  cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, reasoning INTEGER NOT NULL,
                  reported INTEGER NOT NULL, cost REAL, duration_ms INTEGER NOT NULL, finish TEXT NOT NULL, error INTEGER NOT NULL);
                 INSERT INTO requests VALUES(1,1,'old','p','m',10,1,0,0,0,1,NULL,5,'stop',0);",
        )
        .unwrap();
    let mut db = Db::open(&path).unwrap();
    drop(Db::open(&path).unwrap());
    db.record_request(&RequestRecord {
        wire: Some(WireRecord {
            transport: "http".into(),
            invalidation: "none".into(),
            common_prompt_bytes: 900,
            prompt_bytes: 1000,
        }),
        ..request(2, "m", 1000, 10, 0, None, 5)
    })
    .unwrap();
    type WireRow = (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<WireRow> = db
        .conn
        .prepare("SELECT session, transport, invalidation, common_prompt_bytes, prompt_bytes FROM requests ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("old".into(), None, None, None, None),
            (
                "s1".into(),
                Some("http".into()),
                Some("none".into()),
                Some(900),
                Some(1000)
            ),
        ]
    );
}

#[test]
fn tool_statuses_preserve_legacy_rows_and_distinguish_control_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    db.conn.execute("INSERT INTO tool_calls(ts,session,project,tool,duration_ms,error) VALUES(1,'s','p','read',3,1)", []).unwrap();
    for status in [
        crate::tools::ToolStatus::Blocked,
        crate::tools::ToolStatus::Skipped,
        crate::tools::ToolStatus::Timeout,
        crate::tools::ToolStatus::Aborted,
    ] {
        db.record_tool(&ToolRecord {
            status: Some(status),
            ts_ms: 2,
            tool: "bash".into(),
            session: "s".into(),
            error: status.failed(),
            ..Default::default()
        })
        .unwrap();
    }
    let report = db.report(None, 10).unwrap();
    assert_eq!(
        report
            .tool_outcomes
            .iter()
            .map(|r| (r.name.as_str(), r.count))
            .collect::<Vec<_>>(),
        vec![
            ("aborted", 1),
            ("blocked", 1),
            ("error", 1),
            ("skipped", 1),
            ("timeout", 1)
        ]
    );
}

#[test]
fn rollups_sum_exactly_and_derive_rates() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    let base = 1_760_000_000_000_i64; // fixed instant
    db.record_request(&request(base, "a", 1000, 200, 800, Some(0.5), 2000))
        .unwrap();
    db.record_request(&request(base + 60_000, "a", 3000, 600, 1200, None, 4000))
        .unwrap();
    db.record_request(&request(
        base + 2 * DAY_MS,
        "b",
        500,
        100,
        0,
        Some(0.25),
        1000,
    ))
    .unwrap();
    db.record_request(&RequestRecord {
        error: true,
        finish_reason: "error".into(),
        ..request(base + 2 * DAY_MS, "b", 0, 0, 0, None, 500)
    })
    .unwrap();
    db.record_tool(&ToolRecord {
        status: None,
        ts_ms: base,
        session: "s1".into(),
        project: "proj-0000aaaa".into(),
        tool: "read".into(),
        duration_ms: 10,
        error: false,
    })
    .unwrap();
    db.record_tool(&ToolRecord {
        status: None,
        ts_ms: base,
        session: "s1".into(),
        project: "proj-0000aaaa".into(),
        tool: "read".into(),
        duration_ms: 30,
        error: true,
    })
    .unwrap();

    let report = db.report(None, base + 3 * DAY_MS).unwrap();
    let o = &report.overall;
    assert_eq!(
        (o.requests, o.errors, o.input, o.output, o.cache_read),
        (4, 1, 4500, 900, 2000)
    );
    assert_eq!(o.priced_requests, 2);
    assert!((o.cost_usd - 0.75).abs() < 1e-9);
    assert_eq!(o.duration_ms, 7500);
    assert!((o.tokens_per_second.unwrap() - 900.0 / 7.5).abs() < 1e-9);
    assert!((o.cache_rate.unwrap() - 2000.0 * 100.0 / 4500.0).abs() < 1e-9);
    assert!((o.error_rate.unwrap() - 25.0).abs() < 1e-9);
    assert!((o.avg_latency_ms.unwrap() - 1875.0).abs() < 1e-9);

    let a = report.by_model.iter().find(|g| g.key == "a").unwrap();
    assert_eq!(
        (a.totals.requests, a.totals.input, a.totals.priced_requests),
        (2, 4000, 1)
    );
    assert_eq!(report.by_project.len(), 1);
    assert_eq!(report.daily.len(), 2);
    assert_eq!(
        report.daily.iter().map(|d| d.totals.requests).sum::<u64>(),
        4
    );
    assert_eq!(report.daily[0].tool_calls, 2);
    assert_eq!(report.daily[0].day, civil_day(base));

    let read = &report.by_tool[0];
    assert_eq!((read.calls, read.errors, read.duration_ms), (2, 1, 40));
    assert_eq!(read.avg_ms, Some(20.0));
    assert_eq!(read.error_rate, Some(50.0));

    // A range excludes earlier hours entirely.
    let recent = db.report(Some(base + DAY_MS), base + 3 * DAY_MS).unwrap();
    assert_eq!(recent.overall.requests, 2);
    assert!(recent.by_tool.is_empty());
    // Raw rows agree with the rollups.
    let raw: i64 = db
        .conn
        .query_row("SELECT SUM(input) FROM requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw as u64, o.input);
}

#[test]
fn transcripts_sync_incrementally() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("projects/demo-1234abcd/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("20261010T000000000Z_abcd1234.jsonl");
    let lines = [
        json!({"type":"session","id":"abcd1234","started":"2026-10-10T00:00:00.000Z","cwd":"/work/demo","model":"m1","version":"0"}),
        json!({"type":"message","id":"e1","parent":null,"ts":"2026-10-10T00:00:01.000Z","message":{"role":"user","content":"hi"}}),
        json!({"type":"message","id":"e2","parent":"e1","ts":"2026-10-10T00:00:02.000Z","message":{"role":"user","content":"rules","dial_portable_rules":true}}),
        json!({"type":"message","id":"e3","parent":"e2","ts":"2026-10-10T00:01:00.500Z","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"c2","type":"function","function":{"name":"bash","arguments":"{}"}}]}}),
        json!({"type":"message","id":"e4","parent":"e3","ts":"2026-10-10T00:02:00.000Z","message":{"role":"tool","tool_call_id":"c1","content":"x"}}),
    ];
    fs::write(
        &file,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let mut db = Db::open_default(home.path()).unwrap();
    assert_eq!(
        db.sync_sessions(home.path()).unwrap(),
        SyncReport {
            files: 1,
            updated: 1,
            removed: 0
        }
    );
    assert_eq!(
        db.sync_sessions(home.path()).unwrap(),
        SyncReport {
            files: 1,
            updated: 0,
            removed: 0
        }
    );
    let report = db.report(None, now_ms()).unwrap();
    let s = &report.sessions;
    assert_eq!(
        (
            s.sessions,
            s.projects,
            s.user_messages,
            s.assistant_messages,
            s.tool_calls
        ),
        (1, 1, 1, 1, 2)
    );
    assert_eq!(s.active_ms, 120_000);
    assert_eq!(report.transcript_tools.len(), 2);
    assert_eq!(
        report.transcript_models,
        vec![NamedCount {
            name: "m1".into(),
            count: 1
        }]
    );
    fs::remove_file(&file).unwrap();
    assert_eq!(db.sync_sessions(home.path()).unwrap().removed, 1);
    assert_eq!(db.report(None, now_ms()).unwrap().sessions.sessions, 0);
}

fn write_fixture_session(home: &Path) -> PathBuf {
    let sessions = home.join("projects/demo-1234abcd/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("20261010T000000000Z_abcd1234.jsonl");
    let lines = [
        json!({"type":"session","id":"abcd1234","started":"2026-10-10T00:00:00.000Z","cwd":"/work/demo","model":"m1","version":"0"}),
        json!({"type":"message","id":"e1","parent":null,"ts":"2026-10-10T00:00:01.000Z","message":{"role":"user","content":"hi"}}),
        json!({"type":"message","id":"e2","parent":"e1","ts":"2026-10-10T00:01:00.500Z","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]}}),
        json!({"type":"message","id":"e3","parent":"e2","ts":"2026-10-10T00:02:00.000Z","message":{"role":"tool","tool_call_id":"c1","content":"x"}}),
    ];
    fs::write(
        &file,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    file
}

#[test]
fn trace_returns_only_the_requested_session() {
    let home = tempfile::tempdir().unwrap();
    let file = write_fixture_session(home.path());
    let mut db = Db::open_default(home.path()).unwrap();
    let mut mine = request(1_700_000_000_000, "m1", 100, 20, 40, Some(0.5), 300);
    mine.session = "abcd1234".into();
    mine.project = "demo-1234abcd".into();
    let mut other = request(1_700_000_000_000, "m2", 999, 1, 0, None, 1);
    other.session = "other".into();
    db.record_request(&mine).unwrap();
    db.record_request(&other).unwrap();
    db.record_tool(&ToolRecord {
        status: None,
        ts_ms: 1_700_000_000_000,
        session: "abcd1234".into(),
        project: "demo-1234abcd".into(),
        tool: "read".into(),
        duration_ms: 5,
        error: false,
    })
    .unwrap();
    db.sync_sessions(home.path()).unwrap();

    let (project, canonical) = session_file_in_tree(home.path(), &file).expect("in tree");
    let trace = db
        .trace(home.path(), &project, &canonical)
        .unwrap()
        .expect("session row");
    assert_eq!(trace.session, "abcd1234");
    assert_eq!(trace.model, "m1");
    assert_eq!(trace.requests.len(), 1);
    assert_eq!(trace.requests[0].input, 100);
    assert_eq!(trace.requests[0].cost_usd, Some(0.5));
    assert_eq!(trace.totals.requests, 1);
    assert_eq!(trace.totals.priced_requests, 1);
    assert_eq!(trace.totals.cost_usd, 0.5);
    assert_eq!(trace.tools.len(), 1);
    assert_eq!(trace.tools[0].tool, "read");
    assert!(
        trace
            .transcript_tools
            .iter()
            .any(|t| t.name == "read" && t.count == 1)
    );

    let missing = db.trace(
        home.path(),
        "demo-1234abcd",
        &home.path().join("nope.jsonl"),
    );
    assert!(missing.unwrap().is_none());
}

#[test]
fn session_file_must_sit_in_projects_sessions() {
    let home = tempfile::tempdir().unwrap();
    let file = write_fixture_session(home.path());
    assert!(session_file_in_tree(home.path(), &file).is_some());
    let notes = home.path().join("projects/demo-1234abcd/notes.jsonl");
    fs::write(&notes, "{}\n").unwrap();
    assert!(session_file_in_tree(home.path(), &notes).is_none());
    let txt = home.path().join("projects/demo-1234abcd/sessions/x.txt");
    fs::write(&txt, "{}\n").unwrap();
    assert!(session_file_in_tree(home.path(), &txt).is_none());
    assert!(session_file_in_tree(home.path(), &home.path().join("missing.jsonl")).is_none());
}

#[test]
fn trace_url_encodes_session_path_and_serves_its_json() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let home = tempfile::tempdir().unwrap();
    let file = write_fixture_session(home.path());
    let mut db = Db::open_default(home.path()).unwrap();
    let mut mine = request(1_700_000_000_000, "m1", 7, 3, 0, None, 9);
    mine.session = "abcd1234".into();
    mine.project = "demo-1234abcd".into();
    db.record_request(&mine).unwrap();
    drop(db);

    let url = trace_url(home.path(), &file).unwrap();
    let rest = url.strip_prefix("http://127.0.0.1:").expect("loopback url");
    let (port, encoded) = rest.split_once("/#/traces?s=").expect("hash route");
    assert_eq!(encoded, server::percent_encode(&file.to_string_lossy()));
    assert!(!encoded.contains('/'));

    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    write!(
        stream,
        "GET /api/trace?session={encoded} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let body = response.split("\r\n\r\n").nth(1).unwrap();
    let value: Value = serde_json::from_str(body).unwrap();
    assert_eq!(value["session"], "abcd1234");
    assert_eq!(value["requests"][0]["input"], 7);
    assert_eq!(value["totals"]["requests"], 1);

    let outside = home.path().join("outside.jsonl");
    fs::write(&outside, "{}\n").unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    let forbidden = server::percent_encode(&outside.to_string_lossy());
    write!(
        stream,
        "GET /api/trace?session={forbidden} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
}

#[test]
fn iso_and_range_parsing() {
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(parse_iso_ms("1970-01-02T00:00:01.5Z"), Some(DAY_MS + 1500));
    assert_eq!(parse_iso_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
    assert_eq!(civil_day(951_868_800_000), "2000-03-01");
    assert!(parse_iso_ms("not a date").is_none());
    assert_eq!(parse_range("all").unwrap(), None);
    assert_eq!(parse_range("24h").unwrap(), Some(24 * HOUR_MS));
    assert_eq!(parse_range("7d").unwrap(), Some(7 * DAY_MS));
    assert_eq!(parse_range("2w").unwrap(), Some(14 * DAY_MS));
    assert!(parse_range("0d").is_err() && parse_range("3y").is_err());
}
