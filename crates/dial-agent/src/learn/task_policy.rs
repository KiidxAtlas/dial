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
    pub initial_reasoning_secs: u64,
    pub lightweight: bool,
}
impl Policy {
    pub fn for_prompt(prompt: &str, configured_secs: u64) -> Self {
        let kind = Kind::classify(prompt);
        let complex = prompt.len() > 1200
            || ["across", "migration", "architecture", "multiple"]
                .iter()
                .any(|w| prompt.to_lowercase().contains(w));
        let initial_reasoning_secs = configured_secs.min(if complex {
            60
        } else if kind == Kind::General {
            10
        } else {
            40
        });
        Self {
            kind,
            initial_reasoning_secs,
            lightweight: !complex && prompt.len() <= 600 && kind != Kind::Debugging,
        }
    }
}
/// Tools whose failures come from running project code or commands, where a failure is
/// evidence about behaviour and the hypothesis-revision checkpoint applies.
const EXECUTION_TOOLS: &[&str] = &[
    "bash",
    "bg",
    "eval",
    "notebook",
    "test",
    "ssh",
    "harness_probe",
    "autoresearch",
];

/// Failures that already name their exact correction: argument/schema validation, tool
/// protocol, bookkeeping preconditions and patch grammar. Debugging advice adds nothing there
/// and pads every retry, so they get no checkpoint.
fn mechanical(tool: &str, error: &str) -> bool {
    matches!(tool, "todo" | "codebase_memory" | "memory" | "forget")
        || error.starts_with("invalid tool call")
        || [
            "missing required argument",
            "missing string",
            "unknown or unavailable tool",
            "not a valid json object",
            "needs at least one",
            "the limit is",
            "shorten it",
            "expected_revision",
        ]
        .iter()
        .any(|needle| error.contains(needle))
        || (matches!(tool, "edit" | "ast_edit")
            && [
                "`put",
                "`cut",
                "`+` row",
                "empty patch",
                "section header",
                "duplicate rem",
            ]
            .iter()
            .any(|needle| error.contains(needle)))
}

/// Fixed recovery guidance derived from actual failed operations, never executable error text.
/// `None` when the error is mechanical or no specific guidance applies.
pub fn recovery_hint(tool: &str, output: &str) -> Option<&'static str> {
    let error = output.trim_start().to_lowercase();
    if mechanical(tool, &error) {
        return None;
    }
    let hint = if tool == "read"
        && (error.contains("line selector")
            || error.contains("range end")
            || error.contains("offset/limit"))
    {
        "Recovery checkpoint: repair the numeric line selector, not the filename. Use path:25-50 or separate offset/limit arguments; do not repeat the malformed path or search for it as a literal filename."
    } else if matches!(tool, "bash" | "bg")
        && (error.contains("no module named pytest")
            || error.contains("python requirement")
            || error.contains("command not found"))
    {
        "Recovery checkpoint: this is an environment/runner failure. Inspect the project metadata and use its declared runner or interpreter. Do not rewrite application code to satisfy an unrelated environment; installing dependencies or using the network still needs approval."
    } else if error.contains("permission denied")
        || error.contains("approval denied")
        || error.contains("sandbox")
        || error.contains("outside the workspace")
    {
        "Recovery checkpoint: respect the permission or sandbox boundary. Explain the needed permission or choose an allowed operation; do not bypass it with another tool or path."
    } else if error.contains("timed out") || error.contains("timeout") {
        "Recovery checkpoint: distinguish a slow operation from a stuck process. Inspect retained output/process status; use the managed background tool for long-running services and honor cancellation. Do not start duplicate processes or rerun merely to recover output."
    } else if tool == "edit" && error.contains("expected a line number") {
        "Recovery checkpoint: fix the patch target, not the file. PUT N.=M: needs both endpoints; use PUT N.=N: for one line, PUT >N: to insert after it, or PUT >$: to append. Keep the latest read's header/tag and + body rows. Do not repeat the malformed target or reread unchanged content to fix syntax."
    } else if error.contains("not found") || error.contains("no such file") {
        "Recovery checkpoint: verify the path or symbol with one scoped lookup; use the observed location before retrying. An empty search is evidence, not a reason to repeat it unchanged."
    } else if error.contains("syntax") || error.contains("parse") || error.contains("invalid json")
    {
        "Recovery checkpoint: inspect the exact syntax diagnostic and smallest affected region, correct its structure, then rerun the same check."
    } else if matches!(tool, "edit" | "ast_edit") {
        "Recovery checkpoint: refresh the exact edit region once, rebuild the edit against current content, and verify the resulting diff. Do not retry an unchanged failed patch."
    } else if EXECUTION_TOOLS.contains(&tool) {
        "Recovery checkpoint: identify what the failure disproves, choose one check that distinguishes the remaining causes, and change the attempted approach before retrying. Preserve the original acceptance criteria."
    } else {
        return None;
    };
    Some(hint)
}
#[cfg(test)]
mod tests;
