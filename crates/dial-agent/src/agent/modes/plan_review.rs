//! `/plan-review`: re-surfaces the last plan submitted through `exit_plan` for approval while plan
//! mode is active. Approval goes through the same path as `exit_plan`.

use crate::agent::{Agent, Decision, Ui, workflow::plan_files};
use anyhow::{Result, bail, ensure};

impl Agent<'_> {
    /// Saves an approved plan (and its optional project autosave), records it for compaction to
    /// carry, leaves plan mode and queues the plan for the next turn.
    pub(crate) fn accept_plan(&mut self, plan: &str, ui: &mut dyn Ui) {
        let saved = plan_files::save(&self.config.state_dir, self.session.id(), plan).ok();
        let autosaved = if self.config.plan.autosave {
            match plan_files::autosave(self.workspace.root(), &self.config.plan.autosave_dir, plan)
            {
                Ok(path) => path,
                Err(error) => {
                    ui.notice(&format!("Plan autosave failed: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        let mut control = self.session.control_state();
        control["approved_plan"] =
            plan_files::approved_record(plan, saved.as_deref(), autosaved.as_deref());
        if let Err(error) = self.session.set_control_state(control) {
            ui.notice(&format!(
                "The approved plan was not recorded for compaction: {error:#}"
            ));
        }
        self.set_plan_mode(false);
        let mut pending = plan.to_owned();
        if let Some(path) = autosaved.as_deref().or(saved.as_deref()) {
            pending.push_str(&format!("\n\nSaved plan: {}", path.display()));
        }
        self.pending_plan = Some(pending);
        let mut event = format!("Plan approved:\n{plan}");
        if let Some(path) = &autosaved {
            event.push_str(&format!("\n\nSaved to {}", path.display()));
        }
        ui.event(&crate::agent::ui::ui_event::UiEvent::new(
            crate::agent::ui::ui_event::EventKind::Plan,
            event,
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
