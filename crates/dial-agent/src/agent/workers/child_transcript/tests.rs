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

fn archive(messages: &[&str]) -> String {
    let mut text = json!({"type":"header","job":"j"}).to_string() + "\n";
    for message in messages {
        let record = json!({"type":"message","message":{"role":"user","content":message}});
        text += &(record.to_string() + "\n");
    }
    text
}

/// Runs `follow` until it reports no progress; each change is rendered as `reset` or `index:text`.
fn drain(follower: &mut MessageFollower) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        let step = follower
            .follow(&mut |change| match change {
                Change::Reset => seen.push("reset".to_owned()),
                Change::Message(index, message) => {
                    seen.push(format!(
                        "{index}:{}",
                        message["content"].as_str().unwrap_or_default()
                    ));
                }
            })
            .unwrap();
        if step == Follow::Unchanged {
            return seen;
        }
    }
}

#[test]
fn follower_emits_absolute_indices_and_only_new_records() {
    let dir = tempfile::tempdir().unwrap();
    let sink = ChildTranscript::create(&layout(dir.path()), "s", "follow", json!({"job":"follow"}))
        .unwrap();
    sink.record(&json!({"role":"user","content":"a"})).unwrap();
    sink.record(&json!({"role":"user","content":"b"})).unwrap();
    let mut follower = MessageFollower::new(sink.path.clone());
    assert_eq!(drain(&mut follower), ["0:a", "1:b"]);
    let before = follower.bytes_read();
    assert_eq!(
        follower.follow(&mut |_| unreachable!()).unwrap(),
        Follow::Unchanged
    );
    assert!(
        follower.bytes_read() - before < 1024,
        "unchanged file costs only anchor checks"
    );
    sink.record(&json!({"role":"user","content":"c"})).unwrap();
    assert_eq!(drain(&mut follower), ["2:c"]);
    assert_eq!(follower.message_count(), 3);
}

#[test]
fn torn_tail_waits_for_its_newline_even_inside_a_multibyte_character() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.jsonl");
    let next = (json!({"type":"message","message":{"role":"user","content":"é"}}).to_string()
        + "\n")
        .into_bytes();
    // 0xC3 starts the two-byte `é`; stop the first write between its bytes.
    let split = next.iter().position(|byte| *byte == 0xC3).unwrap() + 1;
    let mut bytes = archive(&["a"]).into_bytes();
    bytes.extend_from_slice(&next[..split]);
    fs::write(&path, &bytes).unwrap();
    let mut follower = MessageFollower::new(&path);
    assert_eq!(drain(&mut follower), ["0:a"]);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&next[split..])
        .unwrap();
    assert_eq!(drain(&mut follower), ["1:é"]);
}

#[test]
fn truncation_and_replacement_restart_the_replay() {
    // (case, next archive, replaced by a new inode, expected changes)
    let cases: [(&str, &[&str], bool, &[&str]); 4] = [
        ("truncated", &["x"], false, &["reset", "0:x"]),
        (
            "replaced by another file",
            &["a", "b"],
            true,
            &["reset", "0:a", "1:b"],
        ),
        (
            "rewritten in place at the head",
            &["z", "b"],
            false,
            &["reset", "0:z", "1:b"],
        ),
        (
            "rewritten in place at the tail",
            &["a", "z"],
            false,
            &["reset", "0:a", "1:z"],
        ),
    ];
    for (case, next, by_rename, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        fs::write(&path, archive(&["a", "b"])).unwrap();
        let mut follower = MessageFollower::new(&path);
        assert_eq!(drain(&mut follower), ["0:a", "1:b"], "{case}: first replay");
        if by_rename {
            let replacement = dir.path().join("next.jsonl");
            fs::write(&replacement, archive(next)).unwrap();
            fs::rename(&replacement, &path).unwrap();
        } else {
            fs::write(&path, archive(next)).unwrap();
        }
        assert_eq!(drain(&mut follower), expected, "{case}");
    }
}

#[test]
fn oversized_line_is_skipped_without_buffering_it_even_while_torn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.jsonl");
    let mut bytes = archive(&["a"]).into_bytes();
    bytes.extend(std::iter::repeat_n(b'x', MAX_LINE + 5));
    fs::write(&path, &bytes).unwrap();
    let mut follower = MessageFollower::new(&path);
    assert_eq!(drain(&mut follower), ["0:a"]);
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"\n").unwrap();
    file.write_all(
        (json!({"type":"message","message":{"role":"user","content":"b"}}).to_string() + "\n")
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(drain(&mut follower), ["1:b"]);
}

#[test]
fn oversized_or_malformed_headers_are_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(dir.path());
    let parent = transcript_dir(&layout, "s");
    fs::create_dir_all(&parent).unwrap();
    let pad = "x".repeat(MAX_HEADER);
    fs::write(
        parent.join("huge.jsonl"),
        format!("{{\"type\":\"header\",\"id\":\"huge\",\"pad\":\"{pad}\"}}\n"),
    )
    .unwrap();
    fs::write(
        parent.join("ok.jsonl"),
        "{\"type\":\"header\",\"id\":\"ok\"}\n",
    )
    .unwrap();
    fs::write(
        parent.join("torn.jsonl"),
        "{\"type\":\"header\",\"id\":\"to",
    )
    .unwrap();
    let ids: Vec<String> = scan(&layout, "s")
        .iter()
        .map(|found| found.header["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["ok"]);
}

#[test]
fn status_is_found_when_the_tail_window_starts_inside_a_character() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.jsonl");
    let mut tail = json!({"type":"status","status":"done"}).to_string();
    if tail.len().is_multiple_of(2) {
        tail.push(' ');
    }
    // Two-byte characters with an odd tail length put the 4 KiB window on a continuation byte.
    fs::write(&path, format!("{}\n{tail}\n", "é".repeat(3000))).unwrap();
    assert_eq!(read_status(&path).unwrap()["status"], "done");
}

#[test]
fn deep_link_entries_share_the_message_index_space() {
    let dir = tempfile::tempdir().unwrap();
    let sink =
        ChildTranscript::create(&layout(dir.path()), "s", "links", json!({"job":"links"})).unwrap();
    for text in ["a", "b", "c"] {
        sink.record(&json!({"role":"user","content":text})).unwrap();
    }
    sink.finish(json!({"status":"done"})).unwrap();
    let found = entries(&sink.path).unwrap();
    let texts: Vec<&str> = found
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["a", "b", "c"]);
    assert_eq!(messages(&sink.path).unwrap(), found);
}
