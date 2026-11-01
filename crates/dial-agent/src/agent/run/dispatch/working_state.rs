//! Persist obligation transitions only after any required scope decision comes from a human.
use crate::agent::{
    Agent, Ui,
    workflow::{questions, working_state},
};
use crate::runtime::tool_output::ToolOutput;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

impl Agent<'_> {
    pub(super) fn working_state_tool(&mut self, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        match self.update_working_state(args, ui) {
            Ok(working) => ToolOutput::ok(working.to_string()),
            Err(error) => ToolOutput::err(format!("{error:#}"))
                .with_status(crate::tools::ToolStatus::Blocked)
                .with_effects(crate::tools::surface::outcome::Effects::None),
        }
    }

    fn update_working_state(&mut self, args: &Value, ui: &mut dyn Ui) -> anyhow::Result<Value> {
        let before = self.session.control_state();
        let source = &before["user_intent"]["latest"];
        let source_entry = if matches!(source["kind"].as_str(), Some("user_turn" | "user_steering"))
        {
            source["entry_id"].as_str()
        } else {
            None
        };
        let mut proposed =
            working_state::prepare(&before["working_state"], args, source_entry, &[])?;
        if proposed.needs_confirmation() {
            let summary = proposed.summary();
            let request = questions::prepare(
                &json!({"questions":[{
                    "id":"obligation-scope", "header":"Confirm task scope change",
                    "question":format!("The agent proposes changing these recorded task obligations:\n{summary}\nConfirm only if this reflects the scope you want. This does not authorize tools or verify work."),
                    "options":[{"label":"Keep existing scope"},{"label":"Confirm scope change"}],
                    "recommended":0
                }]}),
                self.config,
                self.plan_mode,
                self.session.id(),
            ).map_err(anyhow::Error::msg)?;
            let outcome = ui.ask_questions(&request, self.interrupt.as_ref());
            anyhow::ensure!(
                !self.interrupt.load(Ordering::SeqCst),
                "scope confirmation interrupted; working state unchanged"
            );
            let questions::QuestionOutcome::Answered(mut answers) = outcome else {
                anyhow::bail!(
                    "scope change needs an explicit human confirmation; working state unchanged. Keep the obligations or report the missing decision."
                );
            };
            questions::validate_answers(&request, &mut answers).map_err(anyhow::Error::msg)?;
            let confirmed = answers.first().is_some_and(|answer| {
                !answer.timed_out
                    && answer.selected_options.as_slice() == ["Confirm scope change"]
                    && answer.custom_input_images.is_empty()
                    && answer.note_images.is_empty()
                    && answer
                        .custom_input
                        .as_deref()
                        .is_none_or(|input| input.trim().is_empty())
                    && answer
                        .note
                        .as_deref()
                        .is_none_or(|note| note.trim().is_empty())
            });
            anyhow::ensure!(
                confirmed,
                "scope change was not explicitly confirmed; working state unchanged"
            );
            let confirmations: Vec<_> = proposed
                .pending
                .iter()
                .map(|pending| working_state::Confirmation {
                    id: pending.id.clone(),
                    action: pending.action,
                    question_id: request.question_id.clone(),
                })
                .collect();
            proposed = working_state::prepare(
                &before["working_state"],
                args,
                source_entry,
                &confirmations,
            )?;
            anyhow::ensure!(
                !proposed.needs_confirmation(),
                "scope changes remain unconfirmed; working state unchanged"
            );
        }
        let working = proposed.next;
        if args["action"] == "view" {
            return Ok(working);
        }
        // Questions can pump incoming input. Never overwrite concurrent state or bind a scope
        // decision to a different request after the user has steered the agent.
        let mut current = self.session.control_state();
        anyhow::ensure!(
            current["working_state"] == before["working_state"]
                && current["user_intent"]["latest"] == before["user_intent"]["latest"],
            "task context changed during working-state update; inspect the current state before retrying"
        );
        current["working_state"] = working.clone();
        self.session.set_control_state(current)?;
        Ok(working)
    }
}
