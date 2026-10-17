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
        prompt: "Investigate with your tools and finish with a concise, factual answer that cites file paths and line numbers. You cannot modify files.",
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
        prompt: "Produce an implementation plan for the task. Read the relevant code first, then answer with the goal, the files to change and what changes in each, ordered steps, risks, and how to verify the result. Do not write the implementation. You cannot modify files.",
        tools: &["read", "glob", "grep", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: false,
    },
    AgentSpec {
        name: "worker",
        description: "carries out a change: can write and edit files and run commands (each change asks for approval)",
        prompt: "Implement the assigned slice end to end using existing project conventions. Reuse current evidence and batch coherent edits. Preserve other agents' work and approval boundaries. Run the assigned or relevant project checks once the slice is ready; report changed files, results and any unresolved prerequisites.",
        tools: &["read", "glob", "grep", "bash", "edit", "write", "skill"],
        spawns: &[],
        max_steps: 0,
        mutates: true,
    },
    AgentSpec {
        name: "test-runner",
        description: "make and run tests under the direction of the main agent",
        prompt: "Your only purpose is to make and run tests under the direction of the main agent. Discover existing tests, write new tests when asked, run the test suite, and report pass/fail results with clear evidence. Use the project's test framework (cargo test for Rust, pytest for Python). Report failures with exact error messages and file:line references. Do not modify source code unless explicitly asked; your job is tests only.",
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
