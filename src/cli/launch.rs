//! Builds the per-process agent overrides ([`Launch`]) from the parsed CLI flags.
//!
//! The flag modules validate and resolve values; this module only maps them onto the
//! agent-side carrier so `main.rs` can hand one value to the session.

use std::collections::BTreeSet;

use crate::agent::modes::Launch;
use crate::cli::model_flags::ModelFlags;
use crate::cli::tool_flags::EffectiveSettings;

/// Launch-scoped agent overrides for `--tools`, `--skills`/`--no-skills`,
/// `--system-prompt`, `--append-system-prompt`, and `--max-time`.
pub fn agent_launch(settings: &EffectiveSettings, model: &ModelFlags) -> Launch {
    Launch {
        tools: settings
            .tools
            .as_ref()
            .map(|names| names.iter().cloned().collect::<BTreeSet<_>>()),
        skills: settings
            .skills
            .as_ref()
            .map(|names| names.iter().cloned().collect::<BTreeSet<_>>()),
        deadline: model.deadline(),
        system_prompt: settings.system_prompt.clone(),
        append_system_prompt: settings.append_system_prompt.clone(),
        system_prompt_template: settings.system_prompt_template.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
