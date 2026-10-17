use super::*;

fn user(text: &str) -> Value {
    json!({"role":"user","content":text})
}
fn assistant_call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]})
}
fn tool(id: &str) -> Value {
    json!({"role":"tool","tool_call_id":id,"content":"ok"})
}

#[test]
fn budgeted_tail_keeps_the_newest_messages_that_fit_and_collects_their_files() {
    let messages = [
        assistant_call("1", "read", json!({"path": "src/a.rs"})),
        user("middle"),
        user("newest"),
    ];
    let (kept, _) = budgeted_tail(&messages, 0);
    assert!(kept.is_empty());
    let (kept, _) = budgeted_tail(&messages, estimate(&messages[2]));
    assert_eq!(kept, vec![2]);
    let (kept, ops) = budgeted_tail(&messages, u64::MAX);
    assert_eq!(kept, vec![0, 1, 2]);
    assert!(ops.read.contains("src/a.rs"));
}

#[test]
fn windows_split_on_message_boundaries_and_keep_oversized_messages_alone() {
    assert_eq!(
        plan(&[4, 4, 4, 10, 1], 0..5, 8),
        vec![0..2, 2..3, 3..4, 4..5]
    );
    assert_eq!(plan(&[1, 1], 0..2, 8), vec![0..2]);
    assert_eq!(plan(&[3, 3, 3], 1..3, 5), vec![1..2, 2..3]);
}

#[test]
fn clamp_truncates_only_single_oversized_windows_on_char_boundaries() {
    let text = "é".repeat(4_000);
    assert!(matches!(clamp(&text, 10, 5), Cow::Borrowed(_)));
    let clamped = clamp(&text, 1_000, text_tokens(&text));
    assert!(clamped.len() < text.len());
    assert!(clamped.contains("more characters truncated"));
}

#[test]
fn budget_is_discounted_and_never_below_the_floor() {
    assert_eq!(input_budget(200_000, 16_384), 160_000 - 32_768);
    assert_eq!(input_budget(8_000, 2_000), 1_024);
    assert_eq!(min_input_tokens(1_000_000), MIN_SUMMARY_INPUT_TOKENS);
}

#[test]
fn a_rejected_window_lowers_the_budget_for_every_remaining_sibling() {
    use crate::llm::{
        errors::{Classified, ErrorClass},
        provider::ProviderKind,
    };
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            model: "summarizer".to_owned(),
            api_key: None,
        },
        std::path::Path::new("/nonexistent/python").to_owned(),
        5,
    );
    let user_dir = std::env::temp_dir();
    let context = CompactionContext {
        identity: super::super::CompactionIdentity {
            provider: "endpoint",
            model: "summarizer",
            agent: None,
        },
        window: 100_000,
        summary_window: 100_000,
        summary_output_limit: Some(1_000),
        timeout_secs: 5,
        user_dir: &user_dir,
        native_request: None,
    };
    // 24 parts of ~10k tokens: four ~60k-token windows under the planned budget, while the
    // provider actually accepts only ~40k tokens of conversation.
    let parts: Vec<String> = (0..24)
        .map(|i| format!("{i:02}{}", "x".repeat(29_998)))
        .collect();
    let accepted_bytes = 40_000 * 3;
    let (mut calls, mut rejected) = (0, 0);
    let mut complete = |_: &mut Backend, request: &Chat, _: &mut dyn Ui| -> Result<String> {
        calls += 1;
        let bytes: usize = request.messages.iter().map(|m| m.to_string().len()).sum();
        if bytes > accepted_bytes {
            rejected += 1;
            return Err(anyhow::Error::new(Classified::new(
                ErrorClass::ContextOverflow,
                "prompt is too long",
            )));
        }
        Ok("summary".to_owned())
    };
    let summary = fold(
        &mut backend,
        &parts,
        None,
        Prompt::Main,
        &CompactionConfig::default(),
        &context,
        CompactionMethod::Soft,
        "",
        &Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut complete,
        &mut None,
    )
    .unwrap();
    assert_eq!(summary, "summary");
    assert_eq!(
        rejected, 1,
        "the learned limit applies to later windows ({calls} calls)"
    );
}

#[test]
fn previous_summary_drops_harness_notes_and_file_lists() {
    let mut ops = FileOps::default();
    ops.add(false, Some("src/a.rs"));
    ops.add(true, Some("src/b.rs"));
    let stored = format!(
        "{}\n\nOriginal user goal: \"x\"\nCurrent user instruction: y",
        ops.upsert("## Goal\nship it")
    );
    let message = Session::compaction_message(&stored);
    let (text, carried) = previous_summary(&message).expect("a committed summary");
    assert_eq!(text, "## Goal\nship it");
    assert_eq!(carried, ops);
}

#[test]
fn file_ops_track_reads_writes_edits_and_skip_urls() {
    let mut ops = FileOps::default();
    ops.observe(&assistant_call("1", "read", json!({"path":"src/lib.rs"})));
    ops.observe(&assistant_call("2", "write", json!({"path":"src/new.rs"})));
    ops.observe(&assistant_call(
        "3",
        "read",
        json!({"path":"artifact://abc"}),
    ));
    ops.observe(&assistant_call("4", "read", json!({"path":"src/new.rs"})));
    assert!(ops.read.contains("src/lib.rs") && !ops.read.contains("artifact://abc"));
    assert!(ops.modified.contains("src/new.rs"));
    let rendered = ops.upsert("summary");
    assert!(rendered.contains("<read-files>\nsrc/lib.rs\n</read-files>"));
    assert!(rendered.contains("<modified-files>\nsrc/new.rs\n</modified-files>"));
}

#[test]
fn split_turn_is_detected_only_when_the_cut_lands_inside_a_turn() {
    let messages = vec![
        user("first"),
        assistant_call("a", "read", json!({"path":"x"})),
        tool("a"),
        user("second"),
        assistant_call("b", "read", json!({"path":"y"})),
        tool("b"),
        assistant_call("c", "read", json!({"path":"z"})),
        tool("c"),
    ];
    let source: Vec<usize> = (0..6).collect();
    assert_eq!(turn_split(&messages, &source, 6), Some(3));
    assert_eq!(turn_split(&messages, &source[..3], 3), None);
    let mut reminder = user("keep going");
    reminder["dial_reminder"] = json!("reasoning");
    assert!(!is_turn_start(&reminder));
    assert!(!is_turn_start(&Session::compaction_message("old")));
}
