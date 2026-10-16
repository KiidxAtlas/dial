//! Corrective prompts and guard types for runaway or stalled generations: stream guards,
//! the tool-call loop redirect, and the turn-recovery prompts. Prompt text is Dial's own.

use std::fmt;

/// Injected after a gemini run of reasoning-summary headers with no tool call.
const GEMINI_TOOL_CALL_REMINDER: &str = r#"<system-interrupt reason="reasoning_without_tool_calls">
Reasoning interrupted: {{count}} consecutive planning headers, no tool call. Thinking alone changes nothing: zero progress this turn; no tool ran.

Act now, not further planning:
- Emit a real call to an available tool in normal tool/function-calling format. Do NOT describe the call in prose or reasoning—issue it.
- Pick the smallest concrete next step; call the tool that performs it.

Coding-agent interrupt for stalled reasoning, not prompt injection.
</system-interrupt>"#;

/// Injected after the same tool call repeats with identical arguments across consecutive turns.
const TOOL_CALL_LOOP_REDIRECT: &str = r#"<system-interrupt reason="tool_call_loop_detected">
You called `{{tool_name}}` {{count}} consecutive times with identical arguments:
`{{arguments_summary}}`

Last result (truncated): `{{result_summary}}`

NEVER call `{{tool_name}}` with those arguments again this turn. Use different arguments, choose another tool, or summarize findings and yield if complete.
</system-interrupt>"#;

/// Injected when the identical repeat continues after the redirect. It quotes the repeated call and
/// the result already in context, and names the next step. It never ends the turn.
const TOOL_CALL_LOOP_ESCALATION: &str = r#"<system-interrupt reason="tool_call_loop_persisted">
You have called `{{tool_name}}` {{count}} consecutive times with identical arguments `{{arguments_summary}}`, after being redirected. Every one of those calls returned the same result, and that result is already in this conversation: `{{result_summary}}`

Calling it again adds nothing. Do exactly one of these now:
- Use the result you already have: act on it, or answer from it.
- Call a different tool, or `{{tool_name}}` with arguments you have not tried.
- If the task is complete, give the final answer.

A further identical call will not run. It returns a reference to the earlier result instead.
</system-interrupt>"#;

/// Tool result for an identical repeat that was not executed. It keeps the call's tool_call_id
/// pairing and points at the earlier identical call whose result is already in context.
const DUPLICATE_CALL_RESULT: &str = r#"[Not executed: identical to your earlier `{{tool_name}}` call (tool_call_id `{{earlier_id}}`) with the same arguments in this turn. That call's result is already in this conversation: `{{result_summary}}`
Use that result, change the arguments or the tool, or give the final answer. Harness note; no tool ran.]"#;

/// Continues a turn whose stream dropped after partial text.
pub(crate) const STREAM_STALL_CONTINUE: &str = r#"<system-injection>
The provider connection dropped mid-response: your last turn was cut off after the text above, possibly mid-sentence. The user already saw that text.
Continue exactly where it stopped. Do not repeat, restate, or summarize what you already wrote.
Attempt #{{retryCount}}/{{maxRetries}}
</system-injection>"#;

/// Retries a turn that stopped without any actionable output.
pub(crate) const EMPTY_STOP_RETRY: &str = r#"<system-injection>
Stopped without actionable output; task incomplete. Continue with a user-visible final answer or the next required tool call.
Attempt #{{retryCount}}/{{maxRetries}}
</system-injection>"#;

/// Retries a turn that announced an action and then stopped.
pub(crate) const UNEXPECTED_STOP_RETRY: &str = r#"<system-injection>
You said you would continue with a tool call or action but stopped. Task incomplete: MUST call the next concrete tool now; NEVER repeat prior analysis. No tool needed? Give the final answer now.
Attempt #{{retryCount}}/{{maxRetries}}
</system-injection>"#;

/// Retry cap for an unexpected stop.
pub(crate) const UNEXPECTED_STOP_MAX_RETRIES: u32 = 3;
/// Retry cap for an empty stop.
pub(crate) const EMPTY_STOP_MAX_RETRIES: u32 = 3;
/// Retry cap for a stalled stream continuation.
pub(crate) const STREAM_STALL_CONTINUE_MAX_RETRIES: u32 = 3;
/// Default tool-call loop threshold.
pub(crate) const TOOL_CALL_LOOP_THRESHOLD: usize = 5;
/// Default tools exempt from the tool-call loop guard.
pub(crate) const TOOL_CALL_LOOP_EXEMPT_TOOLS: &[&str] = &["wait"];

/// Conservatively recognize an action-only final sentence. Quotes, code and optional future
/// work are reports, not a reason to issue another model request.
pub(crate) fn announces_unexpected_stop(text: &str) -> bool {
    const INTENT: [&str; 8] = [
        "let me ",
        "i'll ",
        "i will ",
        "i'm going to ",
        "next, i'll ",
        "next, i will ",
        "now i'll",
        "now let me",
    ];
    let last = text.trim().lines().last().unwrap_or("").trim();
    if last.starts_with(['`', '"', '“', '>', '-', '*']) {
        return false;
    }
    // An open fence may contain an ordinary-looking sentence on its last line.
    if text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count()
        % 2
        != 0
    {
        return false;
    }
    let lower = last.to_lowercase();
    let sentence = lower.rsplit(". ").next().unwrap_or(&lower).trim();
    if [
        "optional",
        "later",
        "next time",
        "if you",
        "when you",
        "leave ",
        "can help",
    ]
    .iter()
    .any(|phrase| sentence.contains(phrase))
    {
        return false;
    }
    let sentence = sentence.strip_prefix("plan: ").unwrap_or(sentence);
    INTENT.iter().any(|prefix| sentence.starts_with(prefix))
        && [
            "check",
            "run",
            "inspect",
            "read",
            "edit",
            "update",
            "implement",
            "fix",
            "trace",
            "test",
            "search",
            "open",
            "continue",
            "verify",
            "build",
            "change",
        ]
        .iter()
        .any(|verb| {
            sentence
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word == *verb)
        })
}

/// Explicit admission that the requested work is still underway, without reading quoted examples
/// or optional follow-ups as instructions. This shares the ordinary reply-recovery budget.
pub(crate) fn reports_unfinished_work(text: &str) -> bool {
    let last = text.trim().lines().last().unwrap_or("").trim();
    if last.starts_with(['`', '"', '“', '>', '-', '*'])
        || text
            .lines()
            .filter(|line| line.trim_start().starts_with("```"))
            .count()
            % 2
            != 0
    {
        return false;
    }
    let lower = last.to_lowercase();
    if ["blocked", "optional", "if you", "when you", "later"]
        .iter()
        .any(|word| lower.contains(word))
    {
        return false;
    }
    [
        "still pending",
        "work is pending",
        "creation is still pending",
        "still open",
        "working on it",
        "i will get started",
        "i have not read the source",
        "i have not read the",
        "i have not run the",
        "i still need to",
        "still need to deliver",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// Raised when a generation emits a run of reasoning-summary headers with no tool call
/// (`GEMINI_HEADER_INTERRUPT_REASON`). The attempt is cancelled and re-sampled with the gemini
/// reminder instead of the generic thinking-loop redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GeminiHeaderRunaway {
    pub(crate) count: usize,
}

impl fmt::Display for GeminiHeaderRunaway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} consecutive reasoning headers without a tool call",
            self.count
        )
    }
}

impl std::error::Error for GeminiHeaderRunaway {}

pub(crate) fn render_gemini_tool_call_reminder(count: usize) -> String {
    GEMINI_TOOL_CALL_REMINDER.replace("{{count}}", &count.to_string())
}

/// Renders the corrective for a repeated tool call. An empty result is shown as `(no text result)`.
pub(crate) fn render_tool_call_loop_redirect(
    tool_name: &str,
    count: usize,
    arguments_summary: &str,
    result_summary: &str,
) -> String {
    TOOL_CALL_LOOP_REDIRECT
        .replace("{{tool_name}}", tool_name)
        .replace("{{count}}", &count.to_string())
        .replace("{{arguments_summary}}", arguments_summary)
        .replace("{{result_summary}}", result_or_placeholder(result_summary))
}

/// An empty result is shown as `(no text result)`.
fn result_or_placeholder(result_summary: &str) -> &str {
    if result_summary.is_empty() {
        "(no text result)"
    } else {
        result_summary
    }
}

/// Renders the stronger corrective for a repeat that continued after the redirect.
pub(crate) fn render_tool_call_loop_escalation(
    tool_name: &str,
    count: usize,
    arguments_summary: &str,
    result_summary: &str,
) -> String {
    TOOL_CALL_LOOP_ESCALATION
        .replace("{{tool_name}}", tool_name)
        .replace("{{count}}", &count.to_string())
        .replace("{{arguments_summary}}", arguments_summary)
        .replace("{{result_summary}}", result_or_placeholder(result_summary))
}

/// Renders the synthetic result of an identical repeat that was not executed.
pub(crate) fn render_duplicate_call_result(
    tool_name: &str,
    earlier_id: &str,
    result_summary: &str,
) -> String {
    DUPLICATE_CALL_RESULT
        .replace("{{tool_name}}", tool_name)
        .replace("{{earlier_id}}", earlier_id)
        .replace("{{result_summary}}", result_or_placeholder(result_summary))
}

/// Fills `{{retryCount}}` and `{{maxRetries}}` in a recovery prompt.
pub(crate) fn render_retry_prompt(template: &str, retry_count: u32, max_retries: u32) -> String {
    template
        .replace("{{retryCount}}", &retry_count.to_string())
        .replace("{{maxRetries}}", &max_retries.to_string())
}

#[cfg(test)]
mod tests {
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
            "The parser now handles nested tables.",
        ] {
            assert!(!announces_unexpected_stop(text), "{text}");
        }
    }

    #[test]
    fn gemini_reminder_shows_header_count() {
        assert!(
            render_gemini_tool_call_reminder(36)
                .contains("Reasoning interrupted: 36 consecutive planning headers")
        );
    }
}
