#[test]
fn coherent_runtime_child_view_uses_portable_metadata_and_keeps_full_evidence() {
    use crate::runtime::{Phase, presentation::Presentation};
    let lines = super::format_lines(&[
        serde_json::json!({"role":"tool","content":"model pointer","dial_tool_presentation":{"view":Presentation{label:"Inspect".into(),target:"parser".into(),facts:vec!["3 findings".into()],..Default::default()},"phase":Phase::Incomplete},"dial_child_tool_result":{"text":"full evidence\nsecond line"}}),
    ]);
    assert!(
        lines[0].contains("Inspect")
            && lines[0].contains("parser")
            && lines[0].contains("incomplete")
    );
    assert_eq!(&lines[1..], &["   full evidence", "   second line"]);
    assert!(!lines.join("\n").contains("model pointer"));
}
#[test]
fn format_lines_summarizes_blocks_on_one_line() {
    let lines = format_lines(&[
        json!({"role":"user","content":"look\nat this"}),
        json!({"role":"assistant","content":[
            {"type":"text","text":"reading"},
            {"type":"tool_use","name":"read"},
            {"type":"tool_result"}
        ]}),
        json!({"role":"tool"}),
    ]);
    assert_eq!(
        lines,
        vec![
            "user: look at this".to_owned(),
            "assistant: reading [tool_use read] [tool_result]".to_owned(),
            "tool: ".to_owned(),
        ]
    );
}

use super::*;

fn layout(root: &Path) -> Layout {
    Layout {
        project_root: root.join("project"),
        project_dir: root.join("project/.dial"),
        user_dir: root.join("user"),
        state_dir: root.join("state"),
    }
}

#[test]
fn records_round_trip_redacted_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let sink = ChildTranscript::create(
        &layout,
        "sess/1",
        "task-7",
        json!({"job":"task-7","agent":"scout","status":"running"}),
    )
    .unwrap();
    let secret = format!("sk-ant-{}", "a1".repeat(20));
    sink.record(&json!({"role":"user","content":"hello"}))
        .unwrap();
    sink.record(&json!({"role":"assistant","content":"x".repeat(MAX_RECORD + 1)}))
        .unwrap();
    sink.record(&json!({"role":"tool","content":format!("token {secret}")}))
        .unwrap();
    sink.finish(json!({"status":"done","steps":3})).unwrap();

    let found = scan(&layout, "sess/1");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].header["job"], "task-7");
    assert_eq!(found[0].status.as_ref().unwrap()["status"], "done");

    let loaded = messages(&found[0].path).unwrap();
    assert_eq!(loaded.len(), 3, "header and status are not messages");
    assert_eq!(loaded[0]["content"], "hello");
    assert!(
        loaded[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("[message truncated")
    );
    let tool = loaded[2]["content"].as_str().unwrap();
    assert!(!tool.contains(secret.as_str()), "{tool}");
}

#[test]
fn components_are_kept_inside_the_state_dir() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    assert_eq!(
        transcript_dir(&layout, "../../etc"),
        layout.state_dir.join("subagents").join("______etc")
    );
    assert_eq!(safe_component(""), "session");
}

#[test]
fn scan_ignores_unrelated_files() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let parent = transcript_dir(&layout, "s");
    fs::create_dir_all(&parent).unwrap();
    fs::write(parent.join("notes.txt"), "x").unwrap();
    fs::write(parent.join("bad.jsonl"), "not json\n").unwrap();
    assert!(scan(&layout, "s").is_empty());
}

#[test]
fn pending_prose_redacts_split_credentials_only_at_whole_value_boundaries() {
    const CHILD: &str = "DIAL_PENDING_REDACTION_TEST_CHILD";
    const ENV_SECRET: &str = "literal secret spans\nmultiple lines of prose";
    if std::env::var_os(CHILD).is_none() {
        // Secret collection reads the process environment. Use an isolated test
        // process rather than mutating global environment under parallel tests.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("pending_prose_redacts_split_credentials_only_at_whole_value_boundaries")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("DIAL_TEST_AUTH_TOKEN", ENV_SECRET)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sink = ChildTranscript::create(&layout(dir.path()), "s", "redaction", json!({})).unwrap();
    let path = sink.path.clone();
    let initial = fs::read(&path).unwrap();
    let vendor = format!("sk-ant-{}", "a1".repeat(24));
    let pem = "-----BEGIN PRIVATE KEY-----\nSENSITIVEPEMPAYLOAD\nSECONDPEMLINE\n-----END PRIVATE KEY-----";
    let text = format!("Public update. {vendor}\n{pem}\n{ENV_SECRET}\nDone.");
    for chunk in text.as_bytes().chunks(7) {
        sink.assistant_stream(Some(std::str::from_utf8(chunk).unwrap()))
            .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            initial,
            "no provisional fragments ever reach disk"
        );
    }
    let (revision, snapshot) = pending_assistant(&path, 0).unwrap();
    assert!(snapshot.contains("Public update."));
    for secret in [
        &vendor[..],
        "SENSITIVEPEMPAYLOAD",
        "SECONDPEMLINE",
        ENV_SECRET,
    ] {
        assert!(!snapshot.contains(secret), "{snapshot}");
    }
    assert!(
        pending_assistant(&path, revision).is_none(),
        "unchanged revisions avoid copying/redacting"
    );
    sink.finish(json!({"status":"cancelled"})).unwrap();
    let durable = fs::read_to_string(&path).unwrap();
    for secret in [
        &vendor[..],
        "a1a1a1a1",
        "SENSITIVEPEMPAYLOAD",
        "SECONDPEMLINE",
        "literal secret spans",
    ] {
        assert!(!durable.contains(secret), "{durable}");
    }
    assert_eq!(messages(&path).unwrap().len(), 1);
    assert_eq!(pending_assistant(&path, 0).unwrap().1, "");
    drop(sink);
    assert!(
        pending_assistant(&path, 0).is_none(),
        "drop removes registry entries"
    );
}

#[test]
fn pending_prose_is_bounded_discardable_and_recovers_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let sink = ChildTranscript::create(&layout(dir.path()), "s", "bounded", json!({})).unwrap();
    let path = sink.path.clone();
    sink.assistant_stream(Some(&"x".repeat(MAX_RECORD)))
        .unwrap();
    assert_eq!(sink.pending.lock().unwrap().text.len(), MAX_RECORD);
    sink.assistant_stream(Some("overflow")).unwrap();
    for _ in 0..10 {
        sink.assistant_stream(Some(&"y".repeat(MAX_RECORD)))
            .unwrap();
    }
    assert!(sink.pending.lock().unwrap().text.len() < MAX_RECORD);
    assert!(pending_assistant(&path, 0).unwrap().1.contains("truncated"));
    sink.assistant_stream(None).unwrap();
    sink.assistant_stream(Some("Discarded attempt")).unwrap();
    sink.assistant_stream(None).unwrap();
    sink.assistant_stream(Some("Partial response retained on drop"))
        .unwrap();
    drop(sink);
    let loaded = messages(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0]["content"], "Partial response retained on drop");
    assert!(
        !fs::read_to_string(&path)
            .unwrap()
            .contains("Discarded attempt")
    );
    assert!(pending_assistant(&path, 0).is_none());
}
