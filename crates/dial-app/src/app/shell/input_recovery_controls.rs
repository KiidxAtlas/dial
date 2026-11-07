//! Explicit recovery choices; the recovery store remains the only persistence owner.
use super::Shell;
use super::input_recovery::InputReviewChoice;
use crate::app::core::App;

impl Shell {
    pub(super) fn draft_command(&mut self, app: &App, args: &str) {
        self.poll_input_recovery(app);
        if matches!(args, "" | "preview" | "restore" | "replace") {
            let preview = match self.preview_input_recovery() {
                Ok(preview) => preview,
                Err(error) => {
                    self.note("Input recovery", format!("{error:#}"));
                    return;
                }
            };
            let choice = match self.ask(
                "Review saved input",
                &preview.text(),
                &[
                    ("Keep current", 'k'),
                    ("Restore held", 'r'),
                    ("Replace reviewed input", 'p'),
                    ("Discard saved", 'd'),
                ],
            ) {
                Some(1) => InputReviewChoice::RestoreHeld,
                Some(2) => InputReviewChoice::ReplaceReviewed,
                Some(3) => InputReviewChoice::Discard,
                _ => InputReviewChoice::Keep,
            };
            let result = self.apply_input_review(preview, choice);
            match result {
                Ok(text) => self.note("Input recovery", text),
                Err(error) => self.note("Input recovery", format!("{error:#}")),
            }
            return;
        }
        self.input_recovery_command(&format!("/draft {args}"));
    }
}
