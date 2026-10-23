//! Launch-scoped overrides carried from CLI flags onto a running agent.
//!
//! Everything here is per-process: nothing is written to control state, so a
//! later launch without the flag behaves as before.

use std::collections::BTreeSet;
use std::time::Instant;

use anyhow::Result;

use crate::agent::Agent;

/// Name of the turn-completion tool. A `--tools` allowlist never removes it,
/// because a turn cannot end cleanly without it.
const FINISH: &str = "finish";

/// Overrides supplied on the command line for this process only.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// `--tools`: hard allowlist of tool names. `None` leaves tools unrestricted.
    pub tools: Option<BTreeSet<String>>,
    /// Candidate provider names to validate against the actual startup registry.
    pub deferred_tools: BTreeSet<String>,
    /// `--skills` / `--no-skills`: visible skill names. `None` leaves skills unrestricted;
    /// `Some(empty)` withholds the catalog and the skill tools.
    pub skills: Option<BTreeSet<String>>,
    /// `--max-time`: wall-clock instant after which the next step stops the turn.
    pub deadline: Option<Instant>,
    /// `--system-prompt`: replaces the discovered SYSTEM.md guidance.
    pub system_prompt: Option<String>,
    /// `--append-system-prompt`: appended after the guidance.
    pub append_system_prompt: Option<String>,
    /// `--system-prompt-template`: file whose template replaces the bundled system prompt body.
    pub system_prompt_template: Option<String>,
    /// `--advisor`: evidence-based review and one repair pass with the `coach` role. Never persisted.
    pub advisor: bool,
    /// `/advisor completion`: keep the advisor to the pre-completion review (no per-round notes).
    pub advisor_completion_only: bool,
    /// `--external-thinking`: private scratchpad reasoning. Never persisted.
    pub external_thinking: bool,
}

impl Agent<'_> {
    /// Stores the per-process `--advisor` and `--external-thinking` overrides. Never persisted.
    pub fn set_launch_modes(&mut self, advisor: bool, external_thinking: bool) {
        self.modes.launch.advisor = advisor;
        self.modes.launch.external_thinking = external_thinking;
    }
}

impl Launch {
    /// Whether a tool may be offered to the model under these overrides.
    pub fn allows_tool(&self, name: &str) -> bool {
        if matches!(name, "skill" | "skill_manage") && !self.any_skill_visible() {
            return false;
        }
        match &self.tools {
            None => true,
            Some(allowed) => name == FINISH || allowed.contains(name),
        }
    }

    /// Whether the named skill may be listed or loaded.
    pub fn skill_visible(&self, name: &str) -> bool {
        self.skills
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    /// Whether any skill is visible; false means the skill tools are withheld.
    pub fn any_skill_visible(&self) -> bool {
        self.skills
            .as_ref()
            .is_none_or(|allowed| !allowed.is_empty())
    }

    /// Whether the `--max-time` deadline has passed.
    pub fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

impl Agent<'_> {
    /// Apply launch overrides and rebuild the system prompt so they take effect
    /// before the next model request. Explicit prompt text replaces the discovered
    /// override; an absent value keeps whatever discovery found.
    pub fn set_launch(&mut self, launch: Launch) -> Result<()> {
        let unknown: Vec<&str> = launch
            .deferred_tools
            .iter()
            .filter(|name| !self.ext.contains(name) && !self.config.tools.contains_key(*name))
            .map(String::as_str)
            .collect();
        anyhow::ensure!(
            unknown.is_empty(),
            "unknown tool(s) for --tools after provider registration: {}",
            unknown.join(", ")
        );
        if let Some(text) = &launch.system_prompt {
            self.parts.overrides.0 = Some(text.clone());
        }
        if let Some(text) = &launch.append_system_prompt {
            self.parts.overrides.1 = Some(text.clone());
        }
        if !launch.any_skill_visible() {
            self.parts.skills.clear();
        }
        if let Some(path) = &launch.system_prompt_template {
            self.set_system_prompt_template(std::path::Path::new(path))?;
        }
        self.modes.launch = launch;
        self.rebuild_system();
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod agent_tests;
