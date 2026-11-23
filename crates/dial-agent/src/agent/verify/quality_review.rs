//! `verification.quality_review`: one opt-in self-review of finished work before it is delivered.
//!
//! When the setting is on and the current instruction changed the workspace, the first completed
//! answer is held back once and the same agent, with its tools, is asked to review what it built:
//! visual results through a fresh render or screenshot, code through its diff and checks. The
//! review spends a per-instruction allowance stored in the session control state, so hook or
//! verification continuations cannot repeat it. Completion gates still apply to the reviewed answer.
use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::run::guidance::Priority;
use crate::agent::{Agent, Ui};

/// Changed paths named in the request; the agent finds the rest from its own checkpoints.
const MAX_LISTED_PATHS: usize = 12;

const CONTROL_KEY: &str = "quality_review";

/// The review request for `paths`, the files this instruction changed (possibly empty when only
/// uncheckpointed shell effects are known).
pub(in crate::agent) fn request(paths: &[String]) -> String {
    let mut text = String::from(
        "[Dial quality review] Quality review is on (`verification.quality_review`). Before \
         finishing, review the quality of the work from this request once:\n\
         - Visual: if the change affects anything that renders (UI, pages, styles, images, \
         documents, charts, terminal output), run or open it, capture a fresh screenshot or \
         render and inspect it. Source text and a passing build do not show appearance. If you \
         cannot render it, say so.\n\
         - Code: reread your changes for correctness, edge cases, error handling, naming, \
         duplication and the project's conventions; run the relevant tests, linters or type \
         checks.\n\
         Fix material problems you find, then finish with a short report: what you reviewed, \
         what you changed as a result, and anything left unverified. Do not expand scope or \
         restyle working code. This review is requested once per instruction.",
    );
    if !paths.is_empty() {
        text.push_str("\nChanged files: ");
        let shown: Vec<&str> = paths
            .iter()
            .take(MAX_LISTED_PATHS)
            .map(String::as_str)
            .collect();
        text.push_str(&shown.join(", "));
        if paths.len() > MAX_LISTED_PATHS {
            text.push_str(&format!(" (+{} more)", paths.len() - MAX_LISTED_PATHS));
        }
    }
    text
}

impl Agent<'_> {
    /// The quality-review continuation for a completed answer, or `None` when the setting is off,
    /// nothing changed, review cannot run here, or this instruction already had its review.
    pub(in crate::agent) fn quality_review(&mut self, ui: &mut dyn Ui) -> Result<Option<Value>> {
        if !self.config.verification.quality_review
            || self.hermetic
            || self.plan_mode
            || self.stop_boundary().is_some()
        {
            return Ok(None);
        }
        let paths = self
            .step_checks
            .turn
            .zip(self.checkpoints.as_ref())
            .map(|(turn, checkpoints)| checkpoints.changed_files(turn))
            .unwrap_or_default();
        if paths.is_empty() && !self.step_checks.has_uncheckpointed_operations() {
            return Ok(None);
        }
        let mut control = self.session.control_state();
        let instruction = control["current_instruction_entry_id"].clone();
        if control
            .get(CONTROL_KEY)
            .is_some_and(|review| review["instruction"] == instruction)
        {
            return Ok(None);
        }
        control[CONTROL_KEY] = json!({"instruction": instruction});
        self.session.set_control_state(control)?;
        ui.notice("Quality review is on: asking the agent to check its work before finishing.");
        let body = request(&paths);
        Ok(
            crate::agent::verify::coach_intervention::CoachIntervention::advisory(
                crate::agent::verify::coach_intervention::InterventionKind::QualityReview,
                Priority::Review,
                &body,
                body.clone(),
            )
            .map(crate::agent::verify::coach_intervention::CoachIntervention::into_message),
        )
    }
}

#[cfg(test)]
mod tests;
