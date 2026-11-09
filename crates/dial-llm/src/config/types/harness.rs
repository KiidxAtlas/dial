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

/// One-way migration of the removed split memory tool names onto the single `memory` tool.
pub fn migrate_memory_tools(harness: &mut Harness) -> bool {
    migrate_memory_tool_names(&mut harness.tools)
}

/// Tool names replaced by `memory` and its `action` argument.
pub(crate) const SPLIT_MEMORY_TOOLS: [&str; 8] = [
    "recall",
    "retain",
    "memory_update",
    "forget",
    "memory_history",
    "memory_import",
    "reflect",
    "learn",
];

/// `artifact_read` merged into the built-in `read` (`artifact://` paths), so a saved selection drops it.
pub(crate) const MERGED_INTO_READ: &str = "artifact_read";

/// Tools merged into `evidence` actions (`evidence_search`, `workbench`, `investigate`, ...).
fn merged_into_evidence(tool: &str) -> bool {
    crate::config::tools::integrations::MERGED_INTEGRATIONS
        .iter()
        .any(|&(retired, successor)| successor == "evidence" && retired == tool)
}

/// `working_state` merged into `todo` (its `state_*` ops), so a saved selection selects `todo`.
pub(crate) const MERGED_INTO_TODO: &str = "working_state";

/// Whether a saved tool name was retired by the memory, artifact, evidence or todo merges.
pub(crate) fn retired_tool_name(tool: &str) -> bool {
    SPLIT_MEMORY_TOOLS.contains(&tool)
        || tool == MERGED_INTO_READ
        || tool == MERGED_INTO_TODO
        || merged_into_evidence(tool)
}

/// Replace every split memory name with one `memory`, every merged evidence name with one
/// `evidence` and `working_state` with `todo`, each at the first one's position, and drop
/// `artifact_read`; other entries, including unrelated duplicates, stay for validation.
pub(crate) fn migrate_memory_tool_names(tools: &mut Vec<String>) -> bool {
    if !tools.iter().any(|tool| retired_tool_name(tool)) {
        return false;
    }
    let mut memory_placed = tools.iter().any(|tool| tool == "memory");
    let mut evidence_placed = tools.iter().any(|tool| tool == "evidence");
    let mut todo_placed = tools.iter().any(|tool| tool == "todo");
    let original = std::mem::take(tools);
    for tool in original {
        if SPLIT_MEMORY_TOOLS.contains(&tool.as_str()) {
            if !memory_placed {
                tools.push("memory".into());
                memory_placed = true;
            }
        } else if merged_into_evidence(&tool) {
            if !evidence_placed {
                tools.push("evidence".into());
                evidence_placed = true;
            }
        } else if tool == MERGED_INTO_TODO {
            if !todo_placed {
                tools.push("todo".into());
                todo_placed = true;
            }
        } else if tool != MERGED_INTO_READ {
            tools.push(tool);
        }
    }
    true
}
