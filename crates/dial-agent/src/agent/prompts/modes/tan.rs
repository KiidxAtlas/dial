//! Background-fork notices; dispatch and isolation remain in the mode runtime.

/// Fork-scoped context notice injected before the clone's request.
pub(crate) const CONTEXT_SWITCH: &str = r#"<dial-notice kind="fork">
The conversation above belongs to the parent session, which is still working on its own task. This fork handles only the user's request that follows.

- Work only on that request. Earlier tasks, todo lists, plans and checklists are the parent's, so leave them for the parent to continue.
- The parent edits this working directory at the same time, so files may change between reads, look mid-refactor or fail to compile. Keep that work intact and leave unrelated failures alone. If your request depends on conflicting live changes, report the dependency instead of guessing.
- When the request is done, stop and report; further work would collide with the parent's.
</dial-notice>"#;

/// Parent-side notice recorded when a clone is dispatched.
pub(crate) const DISPATCH: &str = r#"<dial-notice kind="background_task_dispatched" job="{{jobId}}">
A separate background agent is handling the tangential user task below, so this session does not need to do it. Continue the current task, watching for shared-workspace conflicts and any later user instruction. The background task ({{jobId}}) does not send its report into this conversation; its status and transcript may be available separately.

Dispatched work, for awareness only:
{{work}}
</dial-notice>"#;
