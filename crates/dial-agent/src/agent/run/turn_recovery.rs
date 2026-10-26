//! The reply recovery contract shared by parent and child conversations.
//! Productive work is unlimited; attempts that produce no usable answer or tool are bounded.
use crate::agent::safety::stream_guards;

const MAX_UNUSABLE_REPLIES: u32 = 8;
/// Text before the final line that makes a reply a report rather than a bare announcement.
const SUBSTANTIVE_REPORT_CHARS: usize = 160;

#[derive(Default)]
pub(in crate::agent) struct Recovery {
    unusable: u32,
    empty: u32,
    promised: u32,
    stalled: u32,
    /// A substantive report whose last line read as a promise already earned its continue.
    report_challenged: bool,
}

pub(in crate::agent) enum StopAction {
    Accept,
    Continue {
        reason: &'static str,
        message: String,
    },
    Incomplete(String),
}

/// The phrase matchers only flag a final line. A reply that carries a real report before it is
/// challenged once; repeating an answer after being told to answer directly confirms it.
fn substantive_report(text: &str) -> bool {
    let text = text.trim();
    let body = text.rsplit_once('\n').map_or("", |(body, _)| body);
    body.split_whitespace().map(str::len).sum::<usize>() >= SUBSTANTIVE_REPORT_CHARS
}

impl Recovery {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn form(&mut self, unusable: bool) -> Result<(), String> {
        if !unusable {
            self.unusable = 0;
            return Ok(());
        }
        self.unusable += 1;
        self.stalled += 1;
        if self.unusable >= MAX_UNUSABLE_REPLIES || self.stalled >= MAX_UNUSABLE_REPLIES {
            Err("Stopped: the model returned 8 consecutive unusable tool calls or stalled replies without progress. No partial tool calls were executed; completed work is kept. Choose a tool-capable model or correct its tool-call configuration before continuing.".into())
        } else {
            Ok(())
        }
    }

    pub fn stopped(&mut self, text: &str, has_tools: bool) -> StopAction {
        if has_tools {
            self.reset();
            return StopAction::Accept;
        }
        let (attempt, cap, reason, prompt) = if text.trim().is_empty() {
            self.empty += 1;
            (
                self.empty,
                stream_guards::EMPTY_STOP_MAX_RETRIES,
                "the model stopped without an answer or tool call",
                stream_guards::EMPTY_STOP_RETRY,
            )
        } else if stream_guards::announces_unexpected_stop(text)
            || stream_guards::reports_unfinished_work(text)
        {
            if substantive_report(text) && std::mem::replace(&mut self.report_challenged, true) {
                self.reset();
                return StopAction::Accept;
            }
            self.promised += 1;
            (
                self.promised,
                stream_guards::UNEXPECTED_STOP_MAX_RETRIES,
                "the model reported unfinished work but stopped before doing it",
                stream_guards::UNEXPECTED_STOP_RETRY,
            )
        } else {
            self.reset();
            return StopAction::Accept;
        };
        self.stalled += 1;
        if attempt > cap || self.stalled >= MAX_UNUSABLE_REPLIES {
            let mut stop = format!(
                "Stopped with incomplete work: {reason} after bounded recovery. Completed effects and the conversation are kept; no completion was recorded."
            );
            // The user keeps what the model last said instead of only the harness's reason.
            if !text.trim().is_empty() {
                stop.push_str("\n\nLast model reply (not accepted as a completion):\n");
                stop.push_str(text.trim());
            }
            StopAction::Incomplete(stop)
        } else {
            StopAction::Continue {
                reason,
                message: stream_guards::render_retry_prompt(prompt, attempt, cap),
            }
        }
    }
}

#[cfg(test)]
mod tests;
