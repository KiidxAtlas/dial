//! The reply recovery contract shared by parent and child conversations.
//! Productive work is unlimited; attempts that produce no usable answer or tool are bounded.
use crate::agent::safety::stream_guards;

const MAX_UNUSABLE_REPLIES: u32 = 8;

#[derive(Default)]
pub(in crate::agent) struct Recovery {
    unusable: u32,
    empty: u32,
    stalled: u32,
}

pub(in crate::agent) enum StopAction {
    Accept,
    Continue {
        reason: &'static str,
        message: String,
    },
    Incomplete(String),
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
                message: prompt.to_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
