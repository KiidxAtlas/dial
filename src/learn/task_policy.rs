//! Small deterministic task classification. Profiles still require measured acceptance;
//! classification never grants permissions or changes success criteria.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Debugging,
    Editing,
    Investigation,
    #[default]
    General,
}
impl Kind {
    pub fn classify(prompt: &str) -> Self {
        let lower = prompt.to_lowercase();
        let words: std::collections::HashSet<_> =
            lower.split(|c: char| !c.is_alphanumeric()).collect();
        if ["fix", "bug", "crash", "regression", "failing", "debug"]
            .iter()
            .any(|w| words.contains(w))
        {
            Self::Debugging
        } else if [
            "implement",
            "change",
            "edit",
            "rename",
            "add",
            "remove",
            "refactor",
        ]
        .iter()
        .any(|w| words.contains(w))
        {
            Self::Editing
        } else if ["explain", "investigate", "locate", "find", "why", "trace"]
            .iter()
            .any(|w| words.contains(w))
        {
            Self::Investigation
        } else {
            Self::General
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Debugging => "debugging",
            Self::Editing => "editing",
            Self::Investigation => "investigation",
            Self::General => "general",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Policy {
    pub kind: Kind,
    pub inspection_limit: usize,
    pub initial_reasoning_secs: u64,
}
impl Policy {
    pub fn for_prompt(prompt: &str, configured_secs: u64) -> Self {
        let kind = Kind::classify(prompt);
        let complex = prompt.len() > 1200
            || ["across", "migration", "architecture", "multiple"]
                .iter()
                .any(|w| prompt.to_lowercase().contains(w));
        let inspection_limit = if complex {
            10
        } else {
            match kind {
                Kind::Debugging | Kind::Investigation => 6,
                _ => 4,
            }
        };
        let initial_reasoning_secs = configured_secs.min(if complex {
            60
        } else if kind == Kind::General {
            20
        } else {
            40
        });
        Self {
            kind,
            inspection_limit,
            initial_reasoning_secs,
        }
    }
}
/// Fixed recovery guidance derived from actual failed operations, never executable error text.
pub fn recovery_hint(tool: &str, output: &str) -> &'static str {
    let error = output.to_lowercase();
    if error.contains("not found") || error.contains("no such file") {
        "Recovery checkpoint: verify the path or symbol with one scoped lookup; use the observed location before retrying. An empty search is evidence, not a reason to repeat it unchanged."
    } else if error.contains("syntax") || error.contains("parse") || error.contains("invalid json")
    {
        "Recovery checkpoint: inspect the exact syntax diagnostic and smallest affected region, correct its structure, then rerun the same check."
    } else if matches!(tool, "edit" | "ast_edit") {
        "Recovery checkpoint: refresh the exact edit region once, rebuild the edit against current content, and verify the resulting diff. Do not retry an unchanged failed patch."
    } else {
        "Recovery checkpoint: identify what the failure disproves, choose one check that distinguishes the remaining causes, and change the attempted approach before retrying. Preserve the original acceptance criteria."
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_policy_preserves_operator_limits_and_allows_complex_investigation() {
        assert_eq!(Policy::for_prompt("rename a", 60).inspection_limit, 4);
        assert_eq!(
            Policy::for_prompt("debug migration across multiple modules", 60).inspection_limit,
            10
        );
        assert_eq!(Policy::for_prompt("hello", 5).initial_reasoning_secs, 5);
        assert_eq!(Kind::classify("prefixing values"), Kind::General);
    }
}
