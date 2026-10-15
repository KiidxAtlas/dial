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
            ui.event(&crate::agent::ui_event::UiEvent::new(
                crate::agent::ui_event::EventKind::Plan,
                "Plan-yolo: execution handed to the target model.",
            ));
        } else {
            self.accept_plan(plan, ui);
        }
        self.save_modes()
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::{Agent, NullUi};
    use crate::config::types::{Config, Version};
    use crate::llm::provider::{Backend, ProviderKind};
    use std::path::PathBuf;

    fn endpoint() -> Backend {
        Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        )
    }

    #[test]
    fn plan_yolo_approves_the_plan_and_hands_off_to_the_target() {
        let mut config = Config {
            model: "test".into(),
            ..Config::default()
        };
        config.endpoint = Some(crate::config::types::EndpointConfig {
            headers: None,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            account_id: None,
        });
        let workspace = tempfile::tempdir().unwrap();
        let version = Version {
            id: 0,
            parent: None,
            kind: "config".into(),
            harness: config.harness.clone(),
            adapter: None,
            status: "active".into(),
        };
        let mut agent =
            Agent::hermetic(&config, endpoint(), &version, workspace.path(), None, None).unwrap();
        agent.plan_yolo_startup("@main").unwrap();
        // Hermetic agents refuse plan mode through `set_plan_mode`; set the flag directly.
        agent.plan_mode = true;
        assert!(agent.plan_yolo_auto_approves());
        agent.plan_yolo_approve("PLAN", &mut NullUi).unwrap();
        assert!(!agent.plan_mode);
        assert!(
            !agent.plan_yolo_auto_approves(),
            "the arm is consumed by approval"
        );
        assert!(
            agent
                .pending_plan
                .as_deref()
                .is_some_and(|plan| plan.starts_with("PLAN"))
        );
        assert!(
            agent.modes.prewalk.handoff.is_some(),
            "execution handed to the target"
        );
    }

    #[test]
    fn mid_session_arm_hands_off_once_and_disarm_restores_approval() {
        let mut config = Config {
            model: "test".into(),
            ..Config::default()
        };
        config.endpoint = Some(crate::config::types::EndpointConfig {
            headers: None,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            account_id: None,
        });
        let workspace = tempfile::tempdir().unwrap();
        let version = Version {
            id: 0,
            parent: None,
            kind: "config".into(),
            harness: config.harness.clone(),
            adapter: None,
            status: "active".into(),
        };
        let mut agent =
            Agent::hermetic(&config, endpoint(), &version, workspace.path(), None, None).unwrap();
        // Startup already ran for this session; `/plan-yolo` must still arm.
        agent.modes.plan_yolo.started = true;
        let target = agent.plan_yolo_arm("@main").unwrap();
        assert_eq!(agent.plan_yolo_target().as_deref(), Some(target.as_str()));
        agent.plan_mode = true;
        assert!(agent.plan_yolo_auto_approves());
        assert!(agent.plan_yolo_disarm().unwrap());
        assert!(!agent.plan_yolo_auto_approves(), "disarmed plans ask again");
        assert!(agent.plan_yolo_target().is_none());
        assert!(!agent.plan_yolo_disarm().unwrap(), "nothing left to disarm");
    }
}
