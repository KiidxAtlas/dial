//! The reply recovery contract shared by parent and child conversations.
//! Productive work is unlimited; attempts that produce no usable answer or tool are bounded.
use super::stream_guards;

const MAX_UNUSABLE_REPLIES: u32 = 8;

#[derive(Default)]
pub(super) struct Recovery {
    unusable: u32,
    empty: u32,
    promised: u32,
    stalled: u32,
}

pub(super) enum StopAction {
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
        } else if stream_guards::announces_unexpected_stop(text)
            || stream_guards::reports_unfinished_work(text)
        {
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
            StopAction::Incomplete(format!(
                "Stopped with incomplete work: {reason} after bounded recovery. Completed effects and the conversation are kept; no completion was recorded."
            ))
        } else {
            StopAction::Continue {
                reason,
                message: stream_guards::render_retry_prompt(prompt, attempt, cap),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coherent_runtime_pending_work_shares_recovery_and_finished_reports_do_not_restart() {
        let mut recovery = Recovery::default();
        for _ in 0..3 {
            assert!(matches!(
                recovery.stopped("The requested work is pending.", false),
                StopAction::Continue { .. }
            ));
        }
        assert!(matches!(
            recovery.stopped("Still pending.", false),
            StopAction::Incomplete(_)
        ));
        for text in [
            "`The work is pending.` is a message in the log.",
            "Fixed the parser. Optional: run a deployment later.",
            "Blocked: the host is unavailable.",
        ] {
            assert!(matches!(recovery.stopped(text, false), StopAction::Accept));
        }
    }

    #[test]
    fn malformed_empty_and_promised_replies_share_a_progress_bound() {
        let mut recovery = Recovery::default();
        for _ in 0..3 {
            assert!(recovery.form(true).is_ok());
            assert!(matches!(
                recovery.stopped("", false),
                StopAction::Continue { .. }
            ));
        }
        assert!(recovery.form(true).is_ok());
        assert!(matches!(
            recovery.stopped("I will run the tests next.", false),
            StopAction::Incomplete(_)
        ));
    }

    #[test]
    fn productive_tools_rearm_recovery_without_limiting_work() {
        let mut recovery = Recovery::default();
        for _ in 0..100 {
            assert!(recovery.form(true).is_ok());
            assert!(matches!(recovery.stopped("", true), StopAction::Accept));
        }
    }

    #[test]
    fn promises_cannot_become_completed_after_the_retry_cap() {
        let mut recovery = Recovery::default();
        for _ in 0..3 {
            assert!(matches!(
                recovery.stopped("Let me inspect the parser.", false),
                StopAction::Continue { .. }
            ));
        }
        assert!(matches!(
            recovery.stopped("Let me inspect the parser.", false),
            StopAction::Incomplete(_)
        ));
    }
}
