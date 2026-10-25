//! Built-in subagent types: their names, tool subsets and limits (static data).

pub struct AgentSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// Role-specific instructions appended to the common subagent preamble.
    pub prompt: &'static str,
    /// Built-in tools offered (when the harness enables them); extension tools are added by [`toolset`].
    pub tools: &'static [&'static str],
    /// Agent types this definition may recursively launch; an empty list disables spawning.
    pub spawns: &'static [&'static str],
    pub max_steps: u32,
    /// May change files. Such agents run alone, and launching one needs approval.
    pub mutates: bool,
}

pub const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "scout",
        description: "read-only investigation: find and read code, run read-only commands, report facts",
        prompt: "Answer the assigned question using supplied evidence first; inspect only what is missing or stale. Stop when the question is answered. Return concise facts with file paths and line numbers, evidence gaps and any implementation handoff. You cannot modify files.",
        tools: &["read", "glob", "grep", "bash", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: false,
    },
    AgentSpec {
        name: "reviewer",
        description: "read-only code review of a change or area, with findings ranked by severity",
        prompt: "Review the requested change or code area. For change reviews, report concrete defects introduced by the supplied change; for area audits, report concrete existing defects within the requested scope. Do not invent a diff, issues or verification. Cite target-relative paths and inclusive line ranges, explain the trigger and consequence, rank findings by severity, and state findings, no findings, or inconclusive with any evidence gaps. If the caller supplies an output schema, follow that schema exactly; otherwise return a concise prose review. Read-only contextual inspection is permitted; no checks, commands, modifications, commits or pushes.",
        tools: &["read", "glob", "grep", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: false,
    },
    AgentSpec {
        name: "planner",
        description: "read-only: study the code and return an implementation plan (files, steps, risks, verification)",
        prompt: "Plan the assigned outcome using supplied evidence and relevant code. Stop research when the implementation path is known. Return owned files, shared interfaces, ordered changes, risks and checks with an owner; cite evidence instead of copying it. Do not implement or modify files.",
        tools: &["read", "glob", "grep", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: false,
    },
    AgentSpec {
        name: "worker",
        description: "carries out a change: can write and edit files and run commands (each change asks for approval)",
        prompt: "Implement the assigned slice using existing conventions and current evidence. Batch coherent edits; preserve others' work and approval boundaries. Honor the assigned tester/check owner: hand test coverage needs and stable interfaces to the parent for its tester instead of duplicating test authoring or runs. If explicitly assigned testing because delegation is unavailable, use focused checks after relevant edits settle. Diagnose failures before repair; coordinate reruns with the test owner. Report changed behavior, files, observed results and unresolved prerequisites briefly.",
        tools: &["read", "glob", "grep", "bash", "edit", "write", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: true,
    },
    AgentSpec {
        name: "test-runner",
        description: "owns test authoring, focused execution and failure diagnosis; may fix source only when explicitly authorized",
        prompt: "Own the assigned test creation, updates, execution and failure diagnosis using the project's framework and supplied evidence. A test-authoring assignment authorizes scoped test edits, not production changes. Reuse existing coverage; add only tests of changed behavior and important failure boundaries, not prose or implementation details. Author against agreed interfaces while the parent continues independent work; wait for the readiness boundary before running checks on inputs still being edited. Honor check ownership; start with the smallest relevant tests and broaden only for affected cross-cutting behavior, unresolved risk or required project gates. Diagnose actual failures and report exact commands, source state, observed results, errors, file:line references and actionable fixes. Fix production code only within an explicitly authorized, non-overlapping scope; otherwise send diagnostics to the implementation owner. Rerun after repairs or changed inputs, not unchanged success. Report unverified gaps; never weaken valid checks or present test doubles as live integration proof.",
        tools: &["read", "glob", "grep", "bash", "edit", "write", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: true,
    },
];

pub fn builtin(name: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|spec| spec.name == name)
}

/// Whether `name` is an agent type and so unavailable as a `[subagents]` preset name.
pub fn is_reserved(name: &str) -> bool {
    builtin(name).is_some()
}
