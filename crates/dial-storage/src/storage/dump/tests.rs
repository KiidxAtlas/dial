use std::io::Read;

use super::*;

fn fixture(layout: &Layout) -> PathBuf {
    let dir = layout.sessions_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2026-01-01T00-00-00_sess-1.jsonl");
    let lines = [
        r#"{"type":"session","id":"sess-1","started":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work","model":"m1","version":1}"#,
        r#"{"type":"message","id":"m1","parent":null,"ts":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"RAWMARKER token ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn read_member(archive: &Path, name: &str) -> Option<String> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut entry = zip.by_name(name).ok()?;
    let mut out = String::new();
    entry.read_to_string(&mut out).unwrap();
    Some(out)
}

fn setup() -> (tempfile::TempDir, Layout, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    let home = root.path().join("home");
    fs::create_dir_all(&cwd).unwrap();
    let layout = Layout::with_user_dir(&cwd, home).unwrap();
    let session = fixture(&layout);
    (root, layout, session)
}

#[test]
fn all_archive_contains_verbatim_session() {
    let (root, layout, session) = setup();
    let out = root.path().join("out");
    let report = dump_session(&layout, &session, DumpKind::All, &out).unwrap();
    assert_eq!(
        report.entries,
        vec!["session.jsonl".to_string(), "llm-request.json".to_string()]
    );
    assert_eq!(report.subagents, 0);
    let body = read_member(&report.archive, "session.jsonl").unwrap();
    assert!(body.contains("RAWMARKER"));
    assert!(report.to_string().contains("treat accordingly"));
}

#[test]
fn anon_archive_has_no_raw_content() {
    let (root, layout, session) = setup();
    let out = root.path().join("out");
    let report = dump_session(&layout, &session, DumpKind::Anon, &out).unwrap();
    let body = read_member(&report.archive, "session.jsonl").unwrap();
    assert!(!body.contains("RAWMARKER"));
    assert!(!body.contains("ghp_aaaa"));
    assert!(body.contains("\"id\":\"sess-1\""));
    assert!(report.redacted >= 1);
    assert!(report.to_string().contains(REVIEW_NOTE));
}

#[test]
fn refuses_out_dir_inside_sessions() {
    let (root, layout, session) = setup();
    let bad = layout.sessions_dir().join("nested");
    let err = dump_session(&layout, &session, DumpKind::All, &bad).unwrap_err();
    assert!(err.to_string().contains("refusing"));
    let _ = root;
}

fn worker_fixture(layout: &Layout, parent: &str, body: &str) {
    let dir = subagent::transcript_dir(layout, parent);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("task-1-1700000000000.jsonl"),
        format!(
            "{{\"type\":\"header\",\"id\":\"task-1\"}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"{body}\"}}}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn all_archive_includes_worker_transcripts_verbatim() {
    let (root, layout, session) = setup();
    worker_fixture(&layout, "sess-1", "WORKER_RAW_MARKER");
    let report = dump_session(&layout, &session, DumpKind::All, &root.path().join("out")).unwrap();
    assert_eq!(report.subagents, 1);
    assert!(
        report
            .entries
            .contains(&"subagents/task-1-1700000000000.jsonl".to_string())
    );
    let body = read_member(&report.archive, "subagents/task-1-1700000000000.jsonl").unwrap();
    assert!(body.contains("WORKER_RAW_MARKER"));
}

#[test]
fn anon_archive_anonymizes_worker_transcripts() {
    let (root, layout, session) = setup();
    worker_fixture(
        &layout,
        "sess-1",
        "WORKER_RAW_MARKER token ghp_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    let report = dump_session(&layout, &session, DumpKind::Anon, &root.path().join("out")).unwrap();
    assert_eq!(report.subagents, 1);
    let body = read_member(&report.archive, "subagents/task-1-1700000000000.jsonl").unwrap();
    assert!(!body.contains("WORKER_RAW_MARKER"));
    assert!(!body.contains("ghp_bbbb"));
}
