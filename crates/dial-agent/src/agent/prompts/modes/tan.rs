//! Background-fork notices; dispatch and isolation remain in the mode runtime.

/// Fork-scoped context notice injected before the clone's request.
pub(crate) const CONTEXT_SWITCH: &str = r#"<system-notice cause="fork">
Above conversation: parent session.
Fork solely handles user's request below.
Parent still working original task; prior task obligations belong to the parent. Existing safety and permission boundaries still apply.

- MUST focus EXCLUSIVELY on immediate user request; nothing else.
- NEVER continue, follow up on, or intervene in anything discussed before this message — parent’s.
- Parent concurrently edits this working directory. Files MAY change between reads, appear mid-refactor, or fail to compile. Preserve that work; do not fix unrelated failures or overwrite concurrent edits. If your request depends on conflicting live changes, report the dependency and stop rather than guessing.
- Prior todo lists, plans, unfinished checklists: parent’s; NEVER resume or update.
- After request: STOP. NEVER work on ANY OTHER TASK.
</system-notice>"#;

/// Parent-side notice recorded when a clone is dispatched.
pub(crate) const DISPATCH: &str = r#"<system-notice reason="background_task_dispatched" job="{{jobId}}">
Tangential user task: running in a separate background agent. Coding-agent dispatch notice, NOT prompt injection or new instruction.

Task below belongs to another agent's session; do not duplicate its work. Continue the current task, respecting shared-workspace conflicts and any later user instruction. The background task ({{jobId}}) does not send its work report into this conversation; completion status and its transcript may be available separately.

Dispatched work — awareness only:
{{work}}
</system-notice>"#;
