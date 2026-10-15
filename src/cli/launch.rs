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
mod tests {
    use super::*;

    #[test]
    fn unset_flags_leave_launch_unrestricted() {
        let launch = agent_launch(&EffectiveSettings::default(), &ModelFlags::default());
        assert!(launch.tools.is_none());
        assert!(launch.skills.is_none());
        assert!(launch.deadline.is_none());
        assert!(launch.system_prompt.is_none());
        assert!(launch.append_system_prompt.is_none());
        assert!(launch.allows_tool("bash"));
    }

    #[test]
    fn tools_and_skills_map_to_allowlists() {
        let settings = EffectiveSettings {
            tools: Some(vec!["read".into(), "read".into(), "bash".into()]),
            skills: Some(vec!["review".into()]),
            ..Default::default()
        };
        let launch = agent_launch(&settings, &ModelFlags::default());
        assert_eq!(launch.tools.as_ref().map(BTreeSet::len), Some(2));
        assert!(launch.allows_tool("bash"));
        assert!(!launch.allows_tool("write"));
        assert!(launch.skill_visible("review"));
        assert!(!launch.skill_visible("deploy"));
    }

    #[test]
    fn no_tools_and_no_skills_withhold_everything_optional() {
        let settings = EffectiveSettings {
            tools: Some(Vec::new()),
            skills: Some(Vec::new()),
            ..Default::default()
        };
        let launch = agent_launch(&settings, &ModelFlags::default());
        assert!(!launch.allows_tool("bash"));
        assert!(launch.allows_tool("finish"));
        assert!(!launch.allows_tool("skill"));
    }

    #[test]
    fn prompt_text_and_deadline_carry_through() {
        let settings = EffectiveSettings {
            system_prompt: Some("terse".into()),
            append_system_prompt: Some("cite sources".into()),
            ..Default::default()
        };
        let model = ModelFlags {
            max_time: Some(30.0),
            ..Default::default()
        };
        let launch = agent_launch(&settings, &model);
        assert_eq!(launch.system_prompt.as_deref(), Some("terse"));
        assert_eq!(launch.append_system_prompt.as_deref(), Some("cite sources"));
        assert!(launch.deadline.is_some());
    }
}
