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
