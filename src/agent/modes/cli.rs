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
mod tests {
    use super::*;

    #[test]
    fn tools_allowlist_keeps_finish_and_drops_others() {
        let launch = Launch {
            tools: Some(["read".to_string()].into_iter().collect()),
            ..Launch::default()
        };
        assert!(launch.allows_tool("read"));
        assert!(launch.allows_tool(FINISH));
        assert!(!launch.allows_tool("bash"));
        assert!(!launch.allows_tool("find"));
    }

    #[test]
    fn empty_skill_allowlist_withholds_skill_tools() {
        let none = Launch {
            skills: Some(BTreeSet::new()),
            ..Launch::default()
        };
        assert!(!none.allows_tool("skill"));
        assert!(!none.allows_tool("skill_manage"));
        assert!(none.allows_tool("bash"));
        assert!(!none.skill_visible("review"));
    }

    #[test]
    fn named_skill_allowlist_filters_catalog_and_keeps_tools() {
        let named = Launch {
            skills: Some(["review".to_string()].into_iter().collect()),
            ..Launch::default()
        };
        assert!(named.allows_tool("skill"));
        assert!(named.skill_visible("review"));
        assert!(!named.skill_visible("deploy"));
        assert!(Launch::default().skill_visible("anything"));
    }

    #[test]
    fn deadline_expires_only_after_it_passes() {
        let past = Launch {
            deadline: Some(Instant::now()),
            ..Launch::default()
        };
        assert!(past.expired());
        let future = Launch {
            deadline: Some(Instant::now() + std::time::Duration::from_secs(60)),
            ..Launch::default()
        };
        assert!(!future.expired());
        assert!(!Launch::default().expired());
    }
}

#[cfg(test)]
mod agent_tests {
    use std::path::PathBuf;

    use super::Launch;
    use crate::agent::{Agent, Backend, ProviderKind, Version};
    use crate::config::types::Config;

    /// Build a hermetic agent over an unreachable endpoint. These tests inspect offered
    /// tools and the system prompt only, so no model request is made.
    fn with_agent<T>(f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
        let config = Config::default();
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
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
            Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
        f(&mut agent)
    }

    #[test]
    fn system_prompt_flag_replaces_the_guidance_in_the_system_message() {
        with_agent(|agent| {
            agent
                .set_launch(Launch {
                    system_prompt: Some("LAUNCH_PROMPT_MARKER".into()),
                    ..Launch::default()
                })
                .unwrap();
            let text = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(text.contains("LAUNCH_PROMPT_MARKER"), "{text}");
        });
    }

    #[test]
    fn tools_allowlist_removes_unlisted_tools_from_the_offered_set() {
        with_agent(|agent| {
            agent
                .set_launch(Launch {
                    tools: Some(["read".to_string()].into_iter().collect()),
                    ..Launch::default()
                })
                .unwrap();
            let names = agent.offered();
            assert!(!names.iter().any(|n| n == "bash"), "{names:?}");
            assert!(!names.iter().any(|n| n == "find"), "{names:?}");
            assert!(!names.iter().any(|n| n == "task"), "{names:?}");
        });
    }

    #[test]
    fn no_skills_withholds_the_skill_tools_and_catalog() {
        with_agent(|agent| {
            agent
                .set_launch(Launch {
                    skills: Some(Default::default()),
                    ..Launch::default()
                })
                .unwrap();
            assert!(!agent.offered().iter().any(|n| n == "skill"));
            let text = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(!text.contains("## Skills"), "{text}");
        });
    }
}
