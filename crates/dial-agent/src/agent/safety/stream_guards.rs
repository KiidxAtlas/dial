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

/// Retry cap for an empty stop.
pub(crate) const EMPTY_STOP_MAX_RETRIES: u32 = 3;
/// Retry cap for a stalled stream continuation.
pub(crate) const STREAM_STALL_CONTINUE_MAX_RETRIES: u32 = 3;
/// Default tool-call loop threshold.
pub(crate) const TOOL_CALL_LOOP_THRESHOLD: usize = 5;
/// Default tools exempt from the tool-call loop guard.
pub(crate) const TOOL_CALL_LOOP_EXEMPT_TOOLS: &[&str] = &["wait"];

pub(crate) use crate::agent::verify::reply_signals::announces_unexpected_stop;
#[cfg(test)]
pub(crate) use crate::agent::verify::reply_signals::reports_unfinished_work;

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
/// Advice for a model repeating one call or cycling through a few: an event line for the UI and
/// the guidance text to send. A streak past [`TOOL_CALL_LOOP_THRESHOLD`] gets the stronger
/// escalation. Shared by the main loop and child workers so both see the same redirect.
pub(crate) fn repetition_guidance(messages: &[serde_json::Value]) -> Option<(String, String)> {
    let repeat = crate::agent::safety::guard::RepeatedToolCall::detect(
        messages,
        TOOL_CALL_LOOP_THRESHOLD,
        TOOL_CALL_LOOP_EXEMPT_TOOLS,
    );
    repetition_guidance_with_streak(messages, repeat.as_ref())
}

/// The parent retains the detected streak for duplicate-call suppression; workers only need
/// advice. Both use the same priority and rendering without detecting the streak twice.
pub(crate) fn repetition_guidance_with_streak(
    messages: &[serde_json::Value],
    repeat: Option<&crate::agent::safety::guard::RepeatedToolCall>,
) -> Option<(String, String)> {
    if let Some(repeat) = repeat {
        let render = if repeat.count > TOOL_CALL_LOOP_THRESHOLD {
            render_tool_call_loop_escalation
        } else {
            render_tool_call_loop_redirect
        };
        return Some((
            format!(
                "Repeated tool call {}x; redirecting the model",
                repeat.count
            ),
            render(
                &repeat.tool_name,
                repeat.count,
                &repeat.arguments_summary,
                &repeat.result_summary,
            ),
        ));
    }
    if let Some(cycle) =
        crate::agent::safety::guard::CallCycle::detect(messages, TOOL_CALL_LOOP_EXEMPT_TOOLS)
    {
        return Some((
            format!(
                "Tool calls cycling with period {}; redirecting the model",
                cycle.period
            ),
            cycle.guidance(),
        ));
    }
    crate::agent::safety::guard::RecurringCall::detect(messages, TOOL_CALL_LOOP_EXEMPT_TOOLS)
        .map(|recurring| (recurring.event(), recurring.guidance()))
}

pub(crate) fn render_duplicate_call_result(
    tool_name: &str,
    earlier_id: &str,
    result_summary: &str,
) -> String {
    duplicate_call_result(tool_name, earlier_id, result_or_placeholder(result_summary))
}

#[cfg(test)]
mod tests;
