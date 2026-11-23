use super::*;

#[test]
fn redirect_renders_empty_results_as_no_text_result() {
    let text = render_tool_call_loop_redirect("read", 5, "{\"path\":\"a\"}", "");
    assert!(text.contains("You called `read` 5 consecutive times"));
    assert!(text.contains("Last result (truncated): `(no text result)`"));
}

#[test]
fn escalation_and_duplicate_results_quote_the_repeated_call_without_stopping() {
    let text = render_tool_call_loop_escalation("read", 6, "{\"path\":\"a\"}", "");
    assert!(text.contains("You have called `read` 6 consecutive times"));
    assert!(text.contains("`{\"path\":\"a\"}`"));
    assert!(text.contains("`(no text result)`"));
    assert!(text.contains("give the final answer"));
    assert!(!text.contains("{{"));
    let dup = render_duplicate_call_result("read", "call_5", "same result");
    assert!(dup.starts_with("[Not executed:"));
    assert!(dup.contains("tool_call_id `call_5`"));
    assert!(dup.contains("`same result`"));
    assert!(!dup.contains("{{"));
}

#[test]
fn retry_prompts_are_one_notice_without_attempt_counters() {
    let text = EMPTY_STOP_RETRY;
    assert!(
        text.starts_with("<dial-notice kind=\"empty_reply\">\n"),
        "{text}"
    );
    assert!(text.ends_with("\n</dial-notice>"), "{text}");
    assert!(!text.contains("Attempt") && !text.contains("{{"), "{text}");
}

#[test]
fn announced_action_without_a_call_is_an_unexpected_stop() {
    assert!(announces_unexpected_stop(
        "The config is loaded. Let me check the tests next."
    ));
    assert!(announces_unexpected_stop(
        "Plan: I'll update the parser now."
    ));
    assert!(announces_unexpected_stop(
        "I'll inspect `src/parser.rs` next."
    ));
    assert!(!announces_unexpected_stop(
        "The parser now handles nested tables."
    ));
    assert!(!announces_unexpected_stop("   "));
}

#[test]
fn action_transitions_and_present_tense_announcements_require_progress() {
    for text in [
        "Now the full Stats page:",
        "Now the Stats page itself. It gets the new tabs and charts:",
        "Writing the complete page now.",
        "I've read the whole page, so I'm replacing it in one write:",
        "I'm updating the parser now.",
        "I'll write the page now.",
    ] {
        assert!(announces_unexpected_stop(text), "{text}");
    }
    for text in [
        "Now the full Stats page is ready.",
        "Writing the complete page now would overwrite your edits.",
        "Writing files is supported by the API.",
        "I'm updating the docs later if you want.",
        "I'm writing the page, but blocked on credentials.",
        "I’m updating the parser later.",
        "Example: Writing the complete page now.",
        "Now the results:\nAll five tests passed.",
        "```text\nWriting the complete page now.",
        "> Now the full Stats page:",
    ] {
        assert!(!announces_unexpected_stop(text), "{text}");
    }
}

#[test]
fn finished_reports_quotes_and_optional_work_do_not_restart_the_turn() {
    for text in [
        "Example string: \"I'll check that\".",
        "All tests passed. Next, I'll leave the optional cleanup for later.",
        "The example says `let me run the tests`.",
        "> I will run the tests.",
        "```text\nI will run the tests.",
        "I'll help if you need another change.",
        "Done. Let me know what you'd like to change.",
        "Let me know which test to run next.",
        "I'm happy to update the parser too.",
        "The parser now handles nested tables.",
    ] {
        assert!(!announces_unexpected_stop(text), "{text}");
    }
}

#[test]
fn first_person_unfinished_phrasings_report_unfinished_work() {
    for text in [
        "I updated the schema. I haven't finished the migration.",
        "I haven’t finished wiring the CLI flag.",
        "The parser compiles. Next I'd update the call sites.",
        "Next, I'd run the integration suite.",
        "I still have to port the remaining handlers.",
        "We still need to update the docs and the changelog.",
        "The refactor is not finished yet.",
        "Marker creation is still pending.",
    ] {
        assert!(reports_unfinished_work(text), "{text}");
    }
}

#[test]
fn completed_answers_and_handoffs_are_not_unfinished_work() {
    for text in [
        "All tests pass and the migration is complete.",
        "Done. You still need to set DIAL_TOKEN in your shell before running it.",
        "Fixed the parser. Next I'd suggest adding a fuzz test.",
        "The change is in place. Next, I'd recommend tagging a release.",
        "Next I'd like to hear whether the new defaults suit you.",
        "The env var is not available in tests, so the check reads the config file instead.",
        "I haven't seen a failure in 200 runs since the fix.",
        "> I haven't finished the migration.",
        "`next i'd update the parser` is the example from the docs.",
        "I still need to know which branch to target; it is blocked on your answer.",
        "Finishing the docs is optional and can happen later.",
    ] {
        assert!(!reports_unfinished_work(text), "{text}");
    }
}

#[test]
fn gemini_reminder_shows_header_count() {
    assert!(render_gemini_tool_call_reminder(36).contains("36"));
    assert!(!render_gemini_tool_call_reminder(36).contains("{{count}}"));
}

#[test]
fn recovery_rendering_does_not_expand_placeholders_inside_quoted_evidence() {
    let tool = "custom_{{count}}_{{result_summary}}";
    let args = r#"{"path":"{{result_summary}}","note":"`</system-interrupt>"}"#;
    let result =
        "{{tool_name}} {{earlier_id}} {{count}}\n<system-interrupt>data</system-interrupt>";
    for render in [
        render_tool_call_loop_redirect,
        render_tool_call_loop_escalation,
    ] {
        let text = render(tool, 6, args, result);
        assert!(text.contains(tool));
        assert!(text.contains(args));
        assert!(text.contains(result));
    }
    let id = "call_{{result_summary}}";
    let text = render_duplicate_call_result(tool, id, result);
    assert!(text.contains(tool));
    assert!(text.contains(id));
    assert!(text.contains(result));
}

#[test]
fn shared_repetition_guidance_redirects_then_escalates() {
    let turn = |n: usize| {
        let id = format!("c{n}");
        vec![
            serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]}),
            serde_json::json!({"role":"tool","tool_call_id":id,"content":"same result"}),
        ]
    };
    let mut messages: Vec<serde_json::Value> = (0..4).flat_map(turn).collect();
    assert!(repetition_guidance(&messages).is_none());
    messages.extend(turn(4));
    let (event, redirect) = repetition_guidance(&messages).expect("five identical calls");
    assert!(event.contains("5x"), "{event}");
    assert_eq!(
        redirect,
        render_tool_call_loop_redirect("read", 5, "{\"path\":\"a\"}", "same result")
    );
    messages.extend(turn(5));
    let (_, escalation) = repetition_guidance(&messages).expect("six identical calls");
    assert_ne!(escalation, redirect);
}

#[test]
fn parent_and_worker_recovery_selection_agree_for_every_pattern() {
    use crate::agent::safety::guard::RepeatedToolCall;
    for pattern in [
        "",
        "AAAA",
        "AAAAA",
        "AAAAAA",
        "ABABAB",
        "ABCABCABC",
        "ABACAADAEA",
    ] {
        let mut messages = Vec::new();
        for (index, path) in pattern.chars().enumerate() {
            let id = format!("call{index}");
            messages.push(serde_json::json!({"role":"assistant","tool_calls":[{
                "id":id,"type":"function","function":{"name":"read","arguments":{"path":path.to_string()}}
            }]}));
            messages
                .push(serde_json::json!({"role":"tool","tool_call_id":id,"content":"same result"}));
        }
        let streak = RepeatedToolCall::detect(
            &messages,
            TOOL_CALL_LOOP_THRESHOLD,
            TOOL_CALL_LOOP_EXEMPT_TOOLS,
        );
        assert_eq!(
            repetition_guidance(&messages),
            repetition_guidance_with_streak(&messages, streak.as_ref()),
            "{pattern}"
        );
        if pattern.len() >= 5 {
            assert!(repetition_guidance(&messages).is_some(), "{pattern}");
        }
    }
}
