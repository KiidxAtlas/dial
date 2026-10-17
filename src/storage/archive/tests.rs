use super::*;
use std::io::Write;

fn history() -> (tempfile::TempDir, PromptHistory) {
    let dir = tempfile::tempdir().unwrap();
    let history = PromptHistory::open(dir.path()).unwrap();
    (dir, history)
}

#[test]
fn normalization_folds_line_endings_and_padding() {
    assert_eq!(
        normalize_prompt("  fix it   \r\nplease  \r\n"),
        "fix it\nplease"
    );
    assert_eq!(normalize_prompt("a\rb"), "a\nb");
    assert_eq!(normalize_prompt(" \n\t "), "");
}

#[test]
fn history_dedupes_resubmissions_and_keeps_latest_provenance() {
    let (_dir, history) = history();
    assert!(
        history
            .add("fix the parser", Some("/p/one"), Some("s1"))
            .unwrap()
    );
    assert!(
        history
            .add("run tests", Some("/p/one"), Some("s1"))
            .unwrap()
    );
    // A terminal copy with trailing padding is the same prompt.
    assert!(
        history
            .add("fix the parser   \r\n", Some("/p/two"), Some("s2"))
            .unwrap()
    );
    assert!(!history.add("   ", None, None).unwrap());
    let recent = history.recent(10, HistoryFilter::default()).unwrap();
    assert_eq!(recent.len(), 2, "duplicates collapse into one row");
    assert_eq!(
        recent[0].prompt, "fix the parser",
        "resubmission moves to the front"
    );
    assert_eq!(recent[0].use_count, 2);
    assert_eq!(recent[0].cwd.as_deref(), Some("/p/two"));
    assert_eq!(recent[0].session_id.as_deref(), Some("s2"));
    assert_eq!(recent[1].prompt, "run tests");
    let scoped = history
        .recent(
            10,
            HistoryFilter {
                cwd: Some("/p/one"),
                session_id: None,
            },
        )
        .unwrap();
    assert_eq!(
        scoped.iter().map(|e| e.prompt.as_str()).collect::<Vec<_>>(),
        ["run tests"]
    );
}

#[test]
fn search_matches_prefixes_and_infixes_without_fts_injection() {
    let (_dir, history) = history();
    history.add("git commit the fix", None, None).unwrap();
    history.add("deploy to staging", None, None).unwrap();
    history.add("50% off_sale", None, None).unwrap();
    let prompts = |q: &str| {
        history
            .search(q, 10, HistoryFilter::default())
            .unwrap()
            .into_iter()
            .map(|e| e.prompt)
            .collect::<Vec<_>>()
    };
    assert_eq!(prompts("com"), ["git commit the fix"], "prefix match");
    assert_eq!(
        prompts("mit"),
        ["git commit the fix"],
        "infix match via substring fallback"
    );
    assert_eq!(
        prompts("git fix"),
        ["git commit the fix"],
        "every token must match"
    );
    assert!(
        prompts("NEAR( \"x").is_empty(),
        "FTS operators in queries are plain tokens"
    );
    assert_eq!(prompts("off_sale"), ["50% off_sale"]);
    assert!(
        prompts("%").is_empty(),
        "punctuation-only queries match nothing"
    );
}

#[test]
fn recaps_are_journaled_newest_first_and_filtered() {
    let (_dir, history) = history();
    history.record_recap("s1", "/p/a", "first").unwrap();
    history.record_recap("s1", "/p/a", "second").unwrap();
    history.record_recap("s2", "/p/b", "other").unwrap();
    let all = history.recaps(&RecapQuery::default()).unwrap();
    assert_eq!(all.len(), 3);
    let s1 = history
        .recaps(&RecapQuery {
            session_ids: Some(vec!["s1".into()]),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        s1.iter().map(|r| r.recap.as_str()).collect::<Vec<_>>(),
        ["second", "first"]
    );
    let none = history
        .recaps(&RecapQuery {
            session_ids: Some(vec![]),
            ..Default::default()
        })
        .unwrap();
    assert!(none.is_empty());
    let b = history
        .recaps(&RecapQuery {
            cwd: Some("/p/b".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(b.len(), 1);
    assert!(history.record_recap("s1", "/p/a", "  ").is_err());
}

fn write_session(
    dir: &Path,
    stamp: &str,
    id: &str,
    cwd: &str,
    lines: &[serde_json::Value],
) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{stamp}_{id}.jsonl"));
    let mut file = File::create(&path).unwrap();
    let header = serde_json::json!({"type":"session","id":id,"started":"2026-01-02T03:04:05.000Z","cwd":cwd,"model":"m"});
    writeln!(file, "{header}").unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
    path
}

fn message(id: &str, role: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"type":"message","id":id,"parent":null,"ts":"t","message":{"role":role,"content":text}})
}

#[test]
fn archive_lists_projects_sessions_and_details_with_recaps() {
    let home = tempfile::tempdir().unwrap();
    let user = home.path().join("dial");
    let a = user.join("projects/alpha-11111111/sessions");
    let b = user.join("projects/beta-22222222/sessions");
    write_session(
        &a,
        "20260101T000000000Z",
        "aaaa000011112222",
        "/work/alpha",
        &[
            message("1", "user", "build the parser"),
            message("2", "assistant", "done"),
        ],
    );
    write_session(
        &a,
        "20260102T000000000Z",
        "aaaa333344445555",
        "/work/alpha",
        &[],
    );
    let titled = write_session(
        &b,
        "20260103T000000000Z",
        "bbbb000011112222",
        "/work/beta",
        &[
            message("1", "user", "  explain\n the   cache "),
            serde_json::json!({"type":"meta","id":"2","parent":"1","ts":"t","kind":"title","title":"Cache talk"}),
        ],
    );
    let history = PromptHistory::open(&user).unwrap();
    history
        .record_recap(
            "bbbb000011112222",
            "/work/beta",
            "Explaining the cache; next: diagram",
        )
        .unwrap();
    history
        .add(
            "explain the cache",
            Some("/work/beta"),
            Some("bbbb000011112222"),
        )
        .unwrap();
    drop(history);
    let archive = Archive::new(&user);

    let projects = archive.projects(10).unwrap();
    assert_eq!(projects.records.len(), 2);
    let alpha = projects
        .records
        .iter()
        .find(|p| p.path == "/work/alpha")
        .unwrap();
    assert_eq!(
        alpha.sessions, 2,
        "empty sessions still count toward the project"
    );

    let all = archive.sessions(None, 10).unwrap();
    assert_eq!(all.records.len(), 2, "empty sessions are not listed");
    let beta = all
        .records
        .iter()
        .find(|s| s.id == "bbbb000011112222")
        .unwrap();
    assert_eq!(beta.title, "Cache talk");
    assert_eq!(beta.status, SessionStatus::Pending);
    assert!(beta.messages_exact);
    assert_eq!(
        beta.recap.as_deref(),
        Some("Explaining the cache; next: diagram")
    );
    let alpha_session = all
        .records
        .iter()
        .find(|s| s.id == "aaaa000011112222")
        .unwrap();
    assert_eq!(alpha_session.title, "build the parser");
    assert_eq!(alpha_session.status, SessionStatus::Complete);
    assert_eq!(alpha_session.messages, 2);

    let scoped = archive.sessions(Some(Path::new("/work/beta")), 10).unwrap();
    assert_eq!(
        scoped.records.len(),
        1,
        "a vanished project resolves through its session headers"
    );

    let detail = archive.session("bbbb", 10).unwrap();
    assert_eq!(detail.records.session.file.file_name(), titled.file_name());
    assert_eq!(detail.records.session.id, "bbbb000011112222");
    assert_eq!(detail.records.recaps.len(), 1);
    assert_eq!(detail.records.prompts.len(), 1);
    assert!(
        archive.session("aaaa", 10).is_err(),
        "ambiguous prefixes are refused"
    );
    assert!(archive.session("../etc", 10).is_err());
    assert!(
        archive.session("/etc/passwd", 10).is_err(),
        "paths outside the archive are refused"
    );

    let prompts = archive.prompts(Some("cache"), None, 10).unwrap();
    assert_eq!(prompts.records.len(), 1);
    let recaps = archive.recaps(Some(Path::new("/work/beta")), 10).unwrap();
    assert_eq!(recaps.records.len(), 1);
}

#[test]
fn large_sessions_are_scanned_in_bounded_windows() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("projects/big-33333333/sessions");
    let filler = "x".repeat(1024);
    let mut lines = vec![message("1", "user", "first prompt")];
    for i in 0..200 {
        lines.push(message(&format!("a{i}"), "assistant", &filler));
    }
    lines.push(serde_json::json!({"type":"meta","id":"t","parent":null,"ts":"t","kind":"title","title":"Late title"}));
    lines.push(message("z", "tool", "result"));
    let path = write_session(
        &dir,
        "20260101T000000000Z",
        "cccc000011112222",
        "/work/big",
        &lines,
    );
    let session = scan_session(&path).unwrap();
    assert!(!session.messages_exact);
    assert!(
        session.messages < 202,
        "the middle of a large file is never read"
    );
    assert_eq!(
        session.title, "Late title",
        "titles come from the tail window"
    );
    assert_eq!(session.status, SessionStatus::Interrupted);
    assert_eq!(session.project, "/work/big");
}

#[test]
fn unobserved_terminal_records_never_inherit_complete() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("projects/term-44444444/sessions");
    let filler = "x".repeat(1024);
    let mut base = vec![message("1", "user", "first prompt")];
    for i in 0..60 {
        base.push(message(&format!("a{i}"), "assistant", &filler));
    }
    // A short completed exchange, then a final record larger than the tail window.
    let mut oversized = base.clone();
    oversized.push(message("done", "assistant", "all done"));
    oversized.push(message("big", "user", &"y".repeat(40 * 1024)));
    let path = write_session(
        &dir,
        "20260101T000000000Z",
        "dddd000011112222",
        "/work/big",
        &oversized,
    );
    let session = scan_session(&path).unwrap();
    assert!(!session.messages_exact);
    assert_eq!(session.status, SessionStatus::Unknown);

    // A completed exchange whose final record was cut off mid-JSON.
    let mut truncated = base.clone();
    truncated.push(message("done", "assistant", "all done"));
    let path = write_session(
        &dir,
        "20260101T000000001Z",
        "eeee000011112222",
        "/work/big",
        &truncated,
    );
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"type\":\"message\",\"message\":{\"role\":\"assistant\"")
        .unwrap();
    assert_eq!(scan_session(&path).unwrap().status, SessionStatus::Unknown);
}

#[test]
fn a_final_record_ending_exactly_at_the_window_boundary_is_observed() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("projects/edge-55555555/sessions");
    let filler = "x".repeat(1024);
    let mut lines = vec![message("1", "user", "first prompt")];
    for i in 0..60 {
        lines.push(message(&format!("a{i}"), "assistant", &filler));
    }
    // The final record is exactly one tail window long, so the window starts on its line.
    let line_len = serde_json::to_string(&message("z", "assistant", ""))
        .unwrap()
        .len()
        + 1;
    let text = "y".repeat(TAIL_BYTES as usize - line_len);
    lines.push(message("z", "assistant", &text));
    let path = write_session(
        &dir,
        "20260101T000000002Z",
        "ffff000011112222",
        "/work/edge",
        &lines,
    );
    let session = scan_session(&path).unwrap();
    assert!(!session.messages_exact);
    assert_eq!(session.status, SessionStatus::Complete);
}

#[test]
fn iso_formats_known_instants() {
    assert_eq!(iso(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso(951_782_401), "2000-02-29T00:00:01Z");
}
