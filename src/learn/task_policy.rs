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
/// Fixed recovery guidance derived from actual failed operations, never executable error text.
pub fn recovery_hint(tool: &str, output: &str) -> &'static str {
    let error = output.to_lowercase();
    if tool == "read"
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
    } else {
        "Recovery checkpoint: identify what the failure disproves, choose one check that distinguishes the remaining causes, and change the attempted approach before retrying. Preserve the original acceptance criteria."
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_edit_range_gets_syntax_repair_not_another_read() {
        let hint = recovery_hint(
            "edit",
            "line 2: range end: expected a line number, got \"\"",
        );
        assert!(hint.contains("PUT N.=N:"));
        assert!(hint.contains("PUT >N:"));
        assert!(hint.contains("not the file"));
        assert!(!hint.contains("refresh the exact edit region"));
        assert!(recovery_hint("edit", "stale tag").contains("refresh the exact edit region"));
        assert!(recovery_hint("read", "no such file").contains("verify the path"));
    }

    #[test]
    fn recovery_guidance_distinguishes_selector_environment_and_permissions() {
        assert!(recovery_hint("read", "Invalid line selector").contains("numeric line selector"));
        assert!(recovery_hint("bash", "No module named pytest").contains("environment/runner"));
        assert!(recovery_hint("bash", "permission denied").contains("do not bypass"));
        assert!(recovery_hint("bash", "timed out").contains("Do not start duplicate"));
        assert!(
            !recovery_hint("bash", "AssertionError: answer incorrect")
                .contains("environment/runner")
        );
    }

    #[test]
    fn lightweight_policy_is_conservative_for_complex_and_debugging_work() {
        assert!(Policy::for_prompt("Read src/router.rs", 60).lightweight);
        assert!(!Policy::for_prompt("debug a crash", 60).lightweight);
        assert!(!Policy::for_prompt("migrate across multiple modules", 60).lightweight);
        assert!(!Policy::for_prompt(&"detail ".repeat(100), 60).lightweight);
    }
    #[test]
    fn bounded_policy_preserves_operator_limits_and_allows_complex_investigation() {
        assert_eq!(Policy::for_prompt("hello", 5).initial_reasoning_secs, 5);
        assert_eq!(Policy::for_prompt("hello", 60).initial_reasoning_secs, 10);
        assert_eq!(
            Policy::for_prompt("review architecture across modules", 60).initial_reasoning_secs,
            60
        );
        assert_eq!(Kind::classify("prefixing values"), Kind::General);
    }
}
