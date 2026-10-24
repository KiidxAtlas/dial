//! Corrective prompts and guard types for runaway or stalled generations: stream guards,
//! the tool-call loop redirect, and the turn-recovery prompts. Prompt text is Dial's own.

pub(crate) use crate::agent::prompts::recovery::{
    EMPTY_STOP_RETRY, STREAM_STALL_CONTINUE, UNEXPECTED_STOP_RETRY,
};
use crate::agent::prompts::recovery::{
    GEMINI_TOOL_CALL_REMINDER, duplicate_call_result, tool_call_loop_escalation,
    tool_call_loop_redirect,
};
use std::fmt;

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
    // Closing offers ("Let me know what you'd like to change.") hand the turn back to the user;
    // they are not announced actions.
    if [
        "optional",
        "later",
        "next time",
        "if you",
        "when you",
        "leave ",
        "can help",
        "let me know",
        "would you like",
        "want me to",
        "happy to",
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
    tool_call_loop_redirect(
        tool_name,
        count,
        arguments_summary,
        result_or_placeholder(result_summary),
    )
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
    tool_call_loop_escalation(
        tool_name,
        count,
        arguments_summary,
        result_or_placeholder(result_summary),
    )
}

/// Renders the synthetic result of an identical repeat that was not executed.
pub(crate) fn render_duplicate_call_result(
    tool_name: &str,
    earlier_id: &str,
    result_summary: &str,
) -> String {
    duplicate_call_result(tool_name, earlier_id, result_or_placeholder(result_summary))
}

/// Fills `{{retryCount}}` and `{{maxRetries}}` in a recovery prompt.
pub(crate) fn render_retry_prompt(template: &str, retry_count: u32, max_retries: u32) -> String {
    template
        .replace("{{retryCount}}", &retry_count.to_string())
        .replace("{{maxRetries}}", &max_retries.to_string())
}

#[cfg(test)]
mod tests;
