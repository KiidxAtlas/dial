//! The improvable harness (prompt, tools, step and sampling limits) and the native edit contract.
use super::*;

/// Native edit payload contract. Workspace defaults remain hashline; config defaults preserve replace.
/// Lives in `config` so `llm`, `tools` and `learn` can share it without crossing module boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditMode {
    #[default]
    Hashline,
    Replace,
    Patch,
    ApplyPatch,
    Sloppy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Harness {
    /// The improvable part of the system prompt; dial's fixed tool guidance is added around it.
    pub system_prompt: String,
    pub memory: Vec<String>,
    /// Enabled tools: built-ins, dial's extra tools, and operator tools from `[tools.*]`.
    pub tools: Vec<String>,
    pub subagents: Vec<String>,
    /// Model steps per turn; zero means unlimited for interactive turns.
    pub max_steps: u32,
    pub temperature: f64,
    pub max_tokens: u32,
    /// `None` follows `[edit]`; the improvement loop may pin one mode per model.
    pub edit_mode: Option<EditMode>,
}

impl Harness {
    /// Apply a finite cap for evaluation and bounded automatic repair, including unlimited harnesses.
    pub fn capped_steps(&self, cap: u32) -> u32 {
        if self.max_steps == 0 {
            cap
        } else {
            self.max_steps.min(cap)
        }
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            system_prompt: "Prioritize correctness and maintainability. Deliver the requested behavior, use evidence already available, and report only verification actually performed.".into(),
            memory: vec![],
            tools: crate::config::tools::default_tools(),
            subagents: vec![],
            max_steps: 0,
            temperature: 0.2,
            max_tokens: 0,
            edit_mode: None,
        }
    }
}

/// One-way migration of the removed monolithic memory tool selection.
pub fn migrate_memory_tools(harness: &mut Harness) -> bool {
    migrate_memory_tool_names(&mut harness.tools)
}

pub(crate) fn migrate_memory_tool_names(tools: &mut Vec<String>) -> bool {
    const REPLACEMENTS: [&str; 6] = [
        "recall",
        "retain",
        "memory_update",
        "forget",
        "memory_history",
        "memory_import",
    ];
    if !tools.iter().any(|tool| tool == "memory") {
        return false;
    }
    let present = REPLACEMENTS.map(|replacement| tools.iter().any(|tool| tool == replacement));
    let mut added = [false; 6];
    let original = std::mem::take(tools);
    tools.reserve(original.len().saturating_add(REPLACEMENTS.len()));
    for tool in original {
        if tool == "memory" {
            for (index, replacement) in REPLACEMENTS.iter().enumerate() {
                if !present[index] && !added[index] {
                    tools.push((*replacement).into());
                    added[index] = true;
                }
            }
        } else {
            tools.push(tool);
        }
    }
    true
}
