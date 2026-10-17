//! `/plan-review`: re-surfaces the last plan submitted through `exit_plan` for approval while plan
//! mode is active. Approval goes through the same path as `exit_plan`.

use crate::agent::{Agent, Decision, Ui, workflow::plan_files};
use anyhow::{Result, bail, ensure};

impl Agent<'_> {
    /// Saves an approved plan, leaves plan mode and queues the plan for the next turn.
    pub(crate) fn accept_plan(&mut self, plan: &str, ui: &mut dyn Ui) {
        let artifact = plan_files::save(&self.config.state_dir, self.session.id(), plan).ok();
        self.set_plan_mode(false);
        self.pending_plan = Some(match artifact {
            Some(uri) => format!("{plan}\n\nSaved plan: {uri}"),
            None => plan.to_owned(),
        });
        ui.event(&crate::agent::ui::ui_event::UiEvent::new(
            crate::agent::ui::ui_event::EventKind::Plan,
            format!("Plan approved:\n{plan}"),
        ));
    }

    /// Asks for approval of the stored plan again. Approval accepts it; denial keeps plan mode.
    pub fn review_plan(&mut self, ui: &mut dyn Ui) -> Result<()> {
        ensure!(self.plan_mode, "Plan mode is not active.");
        let Some(plan) = self.latest_plan.clone() else {
            bail!("No plan to review yet — submit one with exit_plan first.");
        };
        match ui.approve("exit_plan", &plan) {
            Decision::Allow | Decision::AllowAlways | Decision::AllowProject => {
                self.plan_yolo_approve(&plan, ui)?
            }
            Decision::Deny => ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Plan,
                "Plan not approved; remain in plan mode.",
            )),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
