//! `dial rules`: read-only inspection of the stream rules a workspace loads, without a model.
//! `list` reports every loaded rule and the files refused while loading; `test` runs one sample
//! through each stream rule exactly as a live inference would and reports which ones fire.
use std::path::Path;

use serde::Serialize;

use super::rules::Catalog;
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::llm::ToolCall;

/// One loaded rule.
#[derive(Debug, Serialize)]
pub struct RuleSummary {
    pub id: String,
    /// Where the rule was loaded from, such as `project:dial:.dial/rules/x.md`.
    pub source: String,
    /// What activates it: `condition`, `astCondition`, `question`, `globs` or `alwaysApply`.
    pub triggers: Vec<String>,
    pub interrupt_mode: String,
    pub scopes: Vec<String>,
    pub globs: Vec<String>,
    pub question: Option<String>,
}

/// Loaded rules plus the files refused while loading them.
#[derive(Debug, Serialize)]
pub struct Listing {
    pub enabled: bool,
    pub rules: Vec<RuleSummary>,
    pub warnings: Vec<String>,
}

/// One output to test the rules against.
#[derive(Debug, Clone)]
pub enum Sample {
    Text(String),
    Thinking(String),
    Tool(ToolCall),
}

/// A stream rule that fired on the sample: `interrupt` aborts the inference, `deferred` lets it
/// proceed and reminds the next request.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ProbeMatch {
    pub id: String,
    pub source: String,
    pub trigger: String,
}

/// The outcome of `dial rules test`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Probe {
    pub matches: Vec<ProbeMatch>,
    /// Sources of judged `question` rules the judge role would be asked about this sample.
    pub judged: Vec<String>,
}

fn catalog(layout: &Layout, workspace: &Path, config: &Config) -> Catalog {
    if config.portable_rules.enabled {
        Catalog::discover(layout, workspace, &config.portable_rules)
    } else {
        Catalog::default()
    }
}

/// `dial rules list`.
pub fn list(layout: &Layout, workspace: &Path, config: &Config) -> Listing {
    let catalog = catalog(layout, workspace, config);
    Listing {
        enabled: config.portable_rules.enabled,
        rules: catalog.inspect(),
        warnings: catalog.warnings,
    }
}

/// `dial rules test` for `agent` (the main agent is `main`).
pub fn test(
    layout: &Layout,
    workspace: &Path,
    config: &Config,
    agent: &str,
    sample: &Sample,
) -> Result<Probe, String> {
    catalog(layout, workspace, config).probe(workspace, agent, sample)
}

#[cfg(test)]
mod tests;
