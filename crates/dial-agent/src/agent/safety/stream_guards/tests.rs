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
fn retry_prompts_show_attempt_and_cap() {
    let text = render_retry_prompt(EMPTY_STOP_RETRY, 2, EMPTY_STOP_MAX_RETRIES);
    assert!(text.ends_with("Attempt #2/3\n</system-injection>"));
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
