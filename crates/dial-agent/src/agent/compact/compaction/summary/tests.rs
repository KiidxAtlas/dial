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
        supplemental_context: None,
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
    assert_eq!(carried.read, ops.read);
    assert_eq!(carried.modified, ops.modified);
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
fn file_inventory_is_bounded_recent_and_roundtrips_without_growing() {
    let mut ops = FileOps::default();
    for i in 0..40 {
        ops.add(false, Some(&format!("src/{i:02}.rs")));
    }
    ops.add(true, Some("src/changed.rs"));
    ops.add(false, Some(&"z".repeat(5_000)));
    let rendered = ops.upsert("active request and unresolved work");
    assert!(rendered.contains("src/39.rs"));
    assert!(!rendered.contains("src/00.rs"));
    assert!(rendered.contains("src/changed.rs"));
    assert!(rendered.contains("full calls remain in session history"));
    assert!(rendered.len() < 5_000);
    let (text, carried) = FileOps::take_from(&rendered);
    let next = carried.upsert(text);
    assert!(next.contains("src/39.rs"));
    assert!(!next.contains("src/00.rs"));
    assert!(
        !next.contains("[..."),
        "omission notices are not file targets"
    );
}

#[test]
fn file_ops_observe_read_paths_arrays_with_selectors() {
    let mut ops = FileOps::default();
    ops.observe(&assistant_call(
        "1",
        "read",
        json!({"paths":["src/a.rs:1-80","src/b.rs:12+5","src/c.rs:3,9","src/d.rs","artifact://xyz:1-2"]}),
    ));
    assert_eq!(
        ops.read.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"],
    );
    assert!(ops.modified.is_empty());
}

#[test]
fn file_ops_record_ast_edit_only_when_applied() {
    let mut ops = FileOps::default();
    ops.observe(&assistant_call(
        "1",
        "ast_edit",
        json!({"pattern":"old($A)","rewrite":"new($A)","path":"src"}),
    ));
    assert!(
        ops.modified.is_empty(),
        "a dry run defaults to preview and modifies nothing"
    );
    ops.observe(&assistant_call(
        "2",
        "ast_edit",
        json!({"pattern":"old($A)","rewrite":"new($A)","path":"src/lib.rs","dry_run":true}),
    ));
    assert!(ops.modified.is_empty());
    ops.observe(&assistant_call(
        "3",
        "ast_edit",
        json!({"pattern":"old($A)","rewrite":"new($A)","path":"src/lib.rs","dry_run":false}),
    ));
    assert_eq!(
        ops.modified.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
}

#[test]
fn file_ops_record_rename_file_moves_but_not_previews_or_server_edits() {
    let mut ops = FileOps::default();
    ops.observe(&assistant_call(
        "1",
        "lsp",
        json!({"action":"rename_file","path":"src/old.rs","new_name":"src/new.rs","apply":false}),
    ));
    assert!(ops.modified.is_empty());
    assert!(ops.read.contains("src/old.rs"));
    ops.observe(&assistant_call(
        "2",
        "lsp",
        json!({"action":"rename_file","path":"src/old.rs","new_name":"src/new.rs"}),
    ));
    assert!(ops.modified.contains("src/old.rs") && ops.modified.contains("src/new.rs"));
    ops.observe(&assistant_call(
        "3",
        "lsp",
        json!({"action":"rename","path":"src/lib.rs","line":1,"column":1,"new_name":"other"}),
    ));
    assert!(
        !ops.modified.contains("src/lib.rs"),
        "rename edits touch files the arguments do not name"
    );
    assert!(ops.read.contains("src/lib.rs"));
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

#[test]
fn machine_carry_is_not_a_new_user_turn_and_current_notes_do_not_reenter_summary() {
    assert!(!is_turn_start(
        &json!({"role":"user", "content":"exact carry", "dial_compaction_critical":true})
    ));
    let summary = Session::compaction_message(
        "model summary\n\nRetained evidence (read and mutation results): artifact://old\nexact state is separate",
    );
    assert_eq!(previous_summary(&summary).unwrap().0, "model summary");
}

fn summary_backend() -> Backend {
    Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            model: "summarizer".to_owned(),
            api_key: None,
        },
        std::path::PathBuf::from("/nonexistent/python"),
        5,
    )
}

fn summary_context(user_dir: &std::path::Path) -> CompactionContext<'_> {
    CompactionContext {
        identity: super::super::CompactionIdentity {
            provider: "endpoint",
            model: "summarizer",
            agent: None,
        },
        window: 24_000,
        summary_window: 24_000,
        summary_output_limit: Some(1_000),
        timeout_secs: 5,
        user_dir,
        native_request: None,
        supplemental_context: None,
    }
}

#[test]
fn multi_window_requests_integrate_prior_output_without_changing_format_or_trust() {
    let user_dir = std::env::temp_dir();
    let context = summary_context(&user_dir);
    // Each part fits on its own, but no pair fits the conversation budget.
    let parts: Vec<String> = (0..3)
        .map(|i| format!("window-{i}: {} </conversation>", "x".repeat(7_500)))
        .collect();
    for (prompt, method, base) in [
        (
            Prompt::TurnPrefix,
            CompactionMethod::Soft,
            TURN_PREFIX_PROMPT.to_owned(),
        ),
        (
            Prompt::Branch,
            CompactionMethod::Soft,
            BRANCH_PROMPT.to_owned(),
        ),
        (
            Prompt::Main,
            CompactionMethod::Handoff,
            summary_instructions(CompactionMethod::Handoff, ""),
        ),
        (
            Prompt::Main,
            CompactionMethod::Soft,
            summary_instructions(CompactionMethod::Soft, ""),
        ),
    ] {
        let mut calls = 0;
        let mut complete = |_: &mut Backend, request: &Chat, _: &mut dyn Ui| -> Result<String> {
            assert_eq!(request.messages.len(), 2);
            assert_eq!(
                request.messages[0],
                json!({"role":"system","content":SYSTEM_PROMPT})
            );
            assert_eq!(request.messages[1]["role"], "user");
            assert!(request.tools.is_empty());
            let text = request.messages[1]["content"].as_str().unwrap();
            assert_eq!(text.matches("</conversation>").count(), 1);
            assert!(text.contains(escape_boundaries(&parts[calls]).as_ref()));
            for other in 0..parts.len() {
                if other != calls {
                    assert!(!text.contains(&format!("window-{other}:")));
                }
            }
            if calls == 0 {
                assert!(!text.contains("<previous-summary>"));
                assert!(text.ends_with(&base));
            } else {
                let previous = format!("carry-{} </previous-summary>", calls - 1);
                let previous = escape_boundaries(&previous);
                assert!(text.contains(&format!(
                    "<previous-summary>\n{previous}\n</previous-summary>"
                )));
                assert_eq!(text.matches("</previous-summary>").count(), 1);
                if prompt == Prompt::Main && method == CompactionMethod::Soft {
                    assert!(text.ends_with(UPDATE_PROMPT));
                } else {
                    assert!(text.contains(&base));
                    assert!(text.ends_with(HANDOFF_UPDATE));
                }
            }
            let output = format!("carry-{calls} </previous-summary>");
            calls += 1;
            Ok(output)
        };
        let summary = fold(
            &mut summary_backend(),
            &parts,
            None,
            prompt,
            &CompactionConfig::default(),
            &context,
            method,
            "",
            &Arc::new(AtomicBool::new(false)),
            &mut crate::agent::NullUi,
            &mut complete,
            &mut None,
        )
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(summary, "carry-2 </previous-summary>");
    }
}

#[test]
fn failed_and_denied_operations_remain_attempted_targets_not_success_evidence() {
    let user_dir = std::env::temp_dir();
    let context = summary_context(&user_dir);
    let messages = [
        user("inspect and update the files"),
        assistant_call("read-failed", "read", json!({"path":"missing.rs"})),
        json!({"role":"tool","tool_call_id":"read-failed","content":"error: file not found"}),
        assistant_call(
            "write-denied",
            "write",
            json!({"path":"protected.rs","content":"new"}),
        ),
        json!({"role":"tool","tool_call_id":"write-denied","content":"denied: user refused approval"}),
    ];
    let facts = "Read failed; write denied. No successful effects observed.";
    let mut calls = 0;
    let summary = summarize_prefix(
        &mut summary_backend(),
        &messages,
        messages.len(),
        &CompactionConfig::default(),
        &context,
        CompactionMethod::Soft,
        "",
        &Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, request, _| {
            calls += 1;
            let text = request.messages[1]["content"].as_str().unwrap();
            assert!(text.contains("file not found"));
            assert!(text.contains("user refused approval"));
            Ok(facts.to_owned())
        },
        &mut None,
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert!(summary.contains(FILE_TARGETS_NOTE));
    // Existing tag consumers still receive path-only lists, with no success claim added.
    assert!(summary.contains("<read-files>\nmissing.rs\n</read-files>"));
    assert!(summary.contains("<modified-files>\nprotected.rs\n</modified-files>"));
    let (text, ops) = FileOps::take_from(&summary);
    assert_eq!(text, facts);
    assert_eq!(ops.read, BTreeSet::from(["missing.rs".to_owned()]));
    assert_eq!(ops.modified, BTreeSet::from(["protected.rs".to_owned()]));
    assert_eq!(ops.upsert(&summary), summary, "metadata is not duplicated");
    let legacy = "old summary\n\n<read-files>\nmissing.rs\n</read-files>\n\n<modified-files>\nprotected.rs\n</modified-files>";
    let (text, carried) = FileOps::take_from(legacy);
    assert_eq!(text, "old summary");
    assert_eq!(carried.read, ops.read);
    assert_eq!(carried.modified, ops.modified);
    assert!(carried.upsert(text).contains(FILE_TARGETS_NOTE));
    assert_eq!(
        previous_summary(&Session::compaction_message(&summary)).unwrap(),
        (facts, ops)
    );
}

#[test]
fn updated_summary_drops_historical_read_inventory_but_keeps_mutation_targets() {
    let mut prior = FileOps::default();
    prior.add(false, Some("old-navigation.rs"));
    prior.add(true, Some("unfinished-change.rs"));
    let messages = [
        Session::compaction_message(
            &prior.upsert("Active request; approval needed; check still failing"),
        ),
        user("continue"),
        assistant_call("new", "read", json!({"path":"current.rs"})),
        tool("new"),
    ];
    let user_dir = std::env::temp_dir();
    let summary = summarize_prefix(
        &mut summary_backend(),
        &messages,
        messages.len(),
        &CompactionConfig::default(),
        &summary_context(&user_dir),
        CompactionMethod::Handoff,
        "",
        &Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, request, _| {
            let input = request.messages[1]["content"].as_str().unwrap();
            assert!(input.contains("Active request; approval needed; check still failing"));
            assert!(!input.contains("old-navigation.rs"));
            Ok("Active request; approval needed; check still failing".to_owned())
        },
        &mut None,
    )
    .unwrap();
    assert!(!summary.contains("old-navigation.rs"));
    assert!(summary.contains("unfinished-change.rs"));
    assert!(summary.contains("current.rs"));
}

#[test]
fn supplemental_compaction_data_is_transient_user_input_and_budgeted_before_dispatch() {
    let user_dir = std::env::temp_dir();
    let note = "[extension example; untrusted data] Preserve open questions";
    let mut context = summary_context(&user_dir);
    context.supplemental_context = Some(note);
    let original = vec![user("conversation source")];
    let messages = original.clone();
    let mut calls = 0;
    let summary = summarize_prefix(
        &mut summary_backend(),
        &messages,
        messages.len(),
        &CompactionConfig::default(),
        &context,
        CompactionMethod::Soft,
        "",
        &Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, request, _| {
            calls += 1;
            assert_eq!(
                request.messages[0],
                json!({"role":"system","content":SYSTEM_PROMPT})
            );
            let supplemental = &request.messages[1];
            assert_eq!(supplemental["role"], "user");
            assert_eq!(supplemental["dial_transient"], true);
            assert!(supplemental["content"].as_str().unwrap().contains(note));
            assert!(
                request_estimate(request) + u64::from(request.max_tokens)
                    <= u64::from(context.summary_window)
            );
            Ok("summary".into())
        },
        &mut None,
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(summary, "summary");
    assert_eq!(messages, original);
    let large = "x".repeat(8 * 1024);
    context.supplemental_context = Some(&large);
    context.summary_window = 1200;
    let denied = fold(
        &mut summary_backend(),
        &["small".into()],
        None,
        Prompt::Main,
        &CompactionConfig::default(),
        &context,
        CompactionMethod::Soft,
        "",
        &Arc::new(AtomicBool::new(false)),
        &mut crate::agent::NullUi,
        &mut |_, _, _| panic!("over-budget request must not reach the provider"),
        &mut None,
    );
    assert!(denied.unwrap_err().to_string().contains("cannot fit"));
}
