//! `--plan-yolo` / `--plan-yolo-into <MODEL>`: plan mode starts on at launch, the
//! first plan submitted through `exit_plan` is approved without asking the user, and execution
//! hands off to the target model. The handoff persists through the prewalk handoff slot.

use crate::agent::{Agent, Ui};
use crate::llm::model_config::ModelSelection;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The startup arm ran for this session.
    started: bool,
    /// Model that implements the approved plan. `None` once approved or when not requested.
    armed: Option<ModelSelection>,
}

impl Agent<'_> {
    /// `--plan-yolo` / `--plan-yolo-into`: arms once per session.
    /// The spec resolves like `--prewalk-into`: `@main`, `@role`, or a bare model name.
    pub fn plan_yolo_startup(&mut self, spec: &str) -> Result<()> {
        if self.modes.plan_yolo.started {
            return Ok(());
        }
        self.modes.plan_yolo.started = true;
        let target = self.prewalk_resolve(spec)?;
        self.modes.plan_yolo.armed = Some(target);
        self.set_plan_mode(true);
        self.save_modes()
    }

    /// `/plan-yolo [MODEL]`: arm mid-session. Turns plan mode on; the next plan submitted through
    /// `exit_plan` is approved without asking and execution moves to `spec` (default `@small`,
    /// resolved like `--plan-yolo-into`). Returns the target as `provider/model`.
    pub fn plan_yolo_arm(&mut self, spec: &str) -> Result<String> {
        let target = self.prewalk_resolve(spec)?;
        let label = format!("{}/{}", target.provider, target.model);
        self.modes.plan_yolo.started = true;
        self.modes.plan_yolo.armed = Some(target);
        self.set_plan_mode(true);
        self.save_modes()?;
        Ok(label)
    }

    /// `/plan-yolo off`: the next plan asks for approval again. Plan mode itself is left as is.
    pub fn plan_yolo_disarm(&mut self) -> Result<bool> {
        let was = self.modes.plan_yolo.armed.take().is_some();
        self.save_modes()?;
        Ok(was)
    }

    /// The armed handoff target as `provider/model`, if any.
    pub fn plan_yolo_target(&self) -> Option<String> {
        self.modes
            .plan_yolo
            .armed
            .as_ref()
            .map(|target| format!("{}/{}", target.provider, target.model))
    }

    /// Whether the next `exit_plan` is approved automatically.
    pub(crate) fn plan_yolo_auto_approves(&self) -> bool {
        self.plan_mode && self.modes.plan_yolo.armed.is_some()
    }

    /// Approves `plan` without asking the user, saves it, and switches execution to the armed
    /// target model. The arm is consumed, so a later plan mode asks for approval again.
    pub(crate) fn plan_yolo_approve(&mut self, plan: &str, ui: &mut dyn Ui) -> Result<()> {
        if let Some(target) = self.modes.plan_yolo.armed.clone() {
            self.prewalk_switch(target)?;
            self.modes.plan_yolo.armed = None;
            self.accept_plan(plan, ui);
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Plan,
                "Plan-yolo: execution handed to the target model.",
            ));
        } else {
            self.accept_plan(plan, ui);
        }
        self.save_modes()
    }
}

#[cfg(test)]
mod tests;
