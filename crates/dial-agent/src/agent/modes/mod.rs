//! Session-scoped agent modes (vibe, skillful). State persists through the session's
//! `control_state` under the `"modes"` key so resumed sessions keep their mode.
pub mod advisor;
pub mod cleanse;
mod cli;
pub mod debug;
pub mod external_thinking;
pub mod goal;
pub(super) mod lifecycle;
pub mod loop_runner;
pub mod omfg;
pub mod plan_review;
pub mod plan_yolo;
pub mod prewalk;
pub mod relaunch;
pub mod switch;
pub mod tan;
pub use cli::Launch;

use super::Agent;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const KEY: &str = "modes";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// `/vibe`: restrict the offered tools to the read-only set.
    pub vibe: bool,
    /// `/skillful`: `Some(false)` hides the skill catalog for this session; `None` keeps the default.
    pub skillful: Option<bool>,
    /// `/goal`, `/guided-goal`: the session's persistent objective and its budget state.
    pub goal: goal::State,
    /// `--prewalk` / `/prewalk`: the todo-gated handoff to a cheaper model. Persisted.
    pub prewalk: prewalk::Coordinator,
    /// `--plan-yolo` / `--plan-yolo-into`: arms plan approval and the execution handoff. Persisted.
    pub plan_yolo: plan_yolo::State,
    /// `/switch`: the session's model selection, reapplied after an agent rebuild or session swap. Persisted.
    pub switch: Option<switch::Saved>,
    /// CLI launch overrides for this process only. Never persisted.
    #[serde(skip)]
    pub launch: Launch,
}

impl State {
    pub fn restore(control: &Value) -> Self {
        serde_json::from_value(control[KEY].clone()).unwrap_or_default()
    }

    /// Merges this state into a copy of the session's control state, preserving other keys.
    pub fn persisted(&self, control: &Value) -> Value {
        let mut next = control.clone();
        next[KEY] = json!(self);
        next
    }

    /// Whether the tool may be offered under vibe mode.
    pub fn vibe_allows(&self, name: &str) -> bool {
        !self.vibe || matches!(name, "read" | "glob" | "grep" | "skill" | "session_search")
    }
}

impl Agent<'_> {
    pub fn set_vibe(&mut self, on: bool) -> Result<()> {
        self.modes.vibe = on;
        self.save_modes()
    }

    /// Takes effect at the next turn's skill catalog refresh.
    pub fn set_skillful(&mut self, value: Option<bool>) -> Result<()> {
        self.modes.skillful = value;
        self.save_modes()
    }

    pub fn modes(&self) -> &State {
        &self.modes
    }

    pub fn workspace(&self) -> &crate::tools::Workspace {
        &self.workspace
    }

    pub fn workspace_mut(&mut self) -> &mut crate::tools::Workspace {
        &mut self.workspace
    }

    /// Applies CLI `--add-dir` roots to the session workspace. Callers invoke this once after construction.
    pub fn add_workspace_roots(&mut self, dirs: &[std::path::PathBuf]) -> Result<()> {
        self.workspace = self.workspace.clone().with_additional_roots(dirs)?;
        Ok(())
    }

    /// Writes the attached roots into the session header so a resumed session restores them, and
    /// lets checkpoints record and restore files under them. Every root change ends here.
    pub fn persist_workspace_roots(&mut self) -> Result<()> {
        let dirs = self.workspace.additional_roots().to_vec();
        if let Some(checkpoints) = &self.checkpoints {
            checkpoints.set_added_roots(&dirs);
        }
        self.session.set_additional_dirs(&dirs)
    }

    /// Re-roots the session workspace at `target` (used by `/move`). Rules are discovered per turn
    /// from the workspace root, so only the system prompt, which embeds the cwd, needs a rebuild.
    /// Checkpoints are rebound to the target project so later edits snapshot the new root.
    pub fn reroot_workspace(&mut self, target: &crate::config::paths::Layout) -> Result<()> {
        self.workspace = self.workspace.reroot(&target.project_root)?;
        self.layout = Some(target.clone());
        if self.checkpoints.is_some() {
            let checkpoints = std::sync::Arc::new(crate::storage::checkpoint::Checkpoints::new(
                target.checkpoints_dir().join(self.session.id()),
                target.project_root.clone(),
            )?);
            self.workspace = self.workspace.clone().with_observer(std::sync::Arc::new(
                super::CheckpointObserver(checkpoints.clone()),
            ));
            self.checkpoints = Some(checkpoints);
        }
        self.persist_workspace_roots()?;
        self.rebuild_system();
        Ok(())
    }

    fn save_modes(&mut self) -> Result<()> {
        let control = self.modes.persisted(&self.session.control_state());
        self.session.set_control_state(control)
    }
}

pub mod btw;
pub mod fresh;
pub mod ratchet;

#[cfg(test)]
mod tests;
