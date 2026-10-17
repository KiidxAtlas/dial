use super::*;

fn setup() -> (tempfile::TempDir, Layout, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    let home = root.path().join("home");
    fs::create_dir_all(&cwd).unwrap();
    let layout = Layout::with_user_dir(&cwd, home).unwrap();
    let dir = layout.sessions_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2026-01-01T00-00-00_sess-1.jsonl");
    let lines = [
        r#"{"type":"session","id":"sess-1","started":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work","model":"m1","version":1}"#,
        r#"{"type":"message","id":"m1","parent":null,"ts":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello RAWMARKER"}}"#,
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    (root, layout, path)
}

fn args(session: &Path, out: PathBuf) -> AnonymizeArgs {
    AnonymizeArgs {
        session: Some(session.display().to_string()),
        out: Some(out),
    }
}

#[test]
fn writes_anonymized_copy_and_returns_zero() {
    let (root, layout, session) = setup();
    let out = root.path().join("anon");
    let code = run(&layout, args(&session, out.clone())).unwrap();
    assert_eq!(code, 0);
    let body = fs::read_to_string(out.join("2026-01-01T00-00-00_sess-1.jsonl")).unwrap();
    assert!(!body.contains("RAWMARKER"));
    assert!(body.contains("\"id\":\"sess-1\""));
}

#[test]
fn refuses_non_empty_output_dir() {
    let (root, layout, session) = setup();
    let out = root.path().join("anon");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("keep.txt"), "x").unwrap();
    assert_eq!(run(&layout, args(&session, out)).unwrap(), 2);
}

#[test]
fn refuses_output_inside_sessions_dir() {
    let (_root, layout, session) = setup();
    let out = layout.sessions_dir().join("anon");
    assert_eq!(run(&layout, args(&session, out)).unwrap(), 2);
}

#[test]
fn unknown_session_reference_returns_two() {
    let (root, layout, _session) = setup();
    let code = run(
        &layout,
        AnonymizeArgs {
            session: Some("does-not-exist".into()),
            out: Some(root.path().join("anon")),
        },
    )
    .unwrap();
    assert_eq!(code, 2);
}
