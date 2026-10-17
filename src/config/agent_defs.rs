//! Validation of `[agents.<name>]` definitions.
use crate::config::subagents::AGENTS;
use crate::config::subagents::builtin;
use crate::config::types::AgentConfig;

use serde_json::Value;

/// Validate one `[agents.<name>]` entry (called from `Config::validate`).
pub fn validate_config_agent(name: &str, agent: &AgentConfig) -> anyhow::Result<()> {
    let base = builtin(&agent.base);
    anyhow::ensure!(
        base.is_some(),
        "custom agent {name:?} must be based on one of: {}",
        AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
    );
    let base = base.expect("checked");
    anyhow::ensure!(
        !agent.prompt.trim().is_empty() && agent.prompt.len() <= 16384,
        "custom agent {name:?} needs a prompt of 1..16384 bytes"
    );
    anyhow::ensure!(
        agent.description.len() <= 200,
        "custom agent {name:?} description is too long"
    );
    for tool in agent.tools.iter().flatten() {
        anyhow::ensure!(
            base.tools.contains(&tool.as_str()),
            "custom agent {name:?}: tool {tool:?} is not available to a {} agent",
            agent.base
        );
    }
    anyhow::ensure!(
        agent.model.len() <= 8,
        "custom agent {name:?}: at most 8 model entries"
    );
    for model in &agent.model {
        anyhow::ensure!(
            !model.trim().is_empty() && model.len() <= 512,
            "custom agent {name:?}: model entries must be 1..512 bytes"
        );
    }
    if let Some(level) = &agent.thinking_level {
        anyhow::ensure!(
            valid_thinking_level(level),
            "custom agent {name:?}: unknown thinking_level {level:?}"
        );
    }
    if let Some(schema) = &agent.output {
        crate::runtime::schema::check_schema(schema).map_err(|why| {
            anyhow::anyhow!("custom agent {name:?}: invalid output schema: {why}")
        })?;
    }
    for spawn in agent.spawns.iter().flatten() {
        anyhow::ensure!(
            spawn == "*" || valid_name(spawn),
            "custom agent {name:?}: invalid spawns entry {spawn:?}"
        );
    }
    Ok(())
}

/// Custom definitions accept the same disabled-thinking spelling as the CLI and
/// retain the existing exact effort values (including none and ultra).
pub(crate) fn valid_thinking_level(level: &str) -> bool {
    level == "off"
        || serde_json::from_value::<crate::llm::profiles::ReasoningEffort>(Value::String(
            level.to_owned(),
        ))
        .is_ok()
}

/// Agent names may use ASCII letters, digits, `_` and `-` (1..64 bytes).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !matches!(name.to_ascii_lowercase().as_str(), "main" | "sub")
}
