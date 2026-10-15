//! Bounded, turn-local detection of successful observations that add no new evidence.
//! Execute first: a source may have changed externally, and a reread can be legitimate.
use crate::llm::ToolCall;
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

#[derive(Default)]
pub(super) struct InvestigationGuard {
    observations: VecDeque<(u64, String, usize)>,
    unchanged: usize,
}

impl InvestigationGuard {
    pub fn observe_output(
        &mut self,
        call: &ToolCall,
        output: &crate::tools::ToolOutput,
    ) -> Option<String> {
        if !output.execution.changed_paths.is_empty() {
            self.observations.clear();
            self.unchanged = 0;
        }
        self.observe(call, &output.text, output.is_error)
    }
    pub fn observe(&mut self, call: &ToolCall, text: &str, is_error: bool) -> Option<String> {
        // A failed check is not progress and must not erase earlier duplicate evidence.
        if is_error {
            return None;
        }
        match call.name.as_str() {
            // Successful edits allow fresh verification. Commands alone do not prove
            // a change; observations are executed again and compared with their result.
            "write" | "edit" | "ast_edit" => {
                return None;
            }
            "read" | "grep" | "glob" | "find" | "ast_search" | "bash" => {}
            // Polls, artifact recovery and control calls are not duplicate observations.
            _ => return None,
        }
        let key = observation_key(call, text);
        if let Some(advice) = self.repeat_known(key) {
            return Some(advice);
        }
        self.unchanged = 0;
        if self.observations.len() == 128 {
            self.observations.pop_front();
        }
        self.observations.push_back((key, call.id.clone(), 0));
        None
    }

    /// Count a retry of cached evidence without pretending the withheld tool executed.
    pub fn repeat_known(&mut self, key: u64) -> Option<String> {
        let (_, first, repeats) = self
            .observations
            .iter_mut()
            .find(|(seen, _, _)| *seen == key)?;
        *repeats += 1;
        self.unchanged = *repeats;
        Some(format!(
            "[Investigation check from dial] This observation is unchanged from tool call {first}. Reuse that evidence; do not reread it for reassurance. If the answer supports a change and its check, implement and verify now, or deliver the requested finding. If unresolved, choose one narrower check that distinguishes the remaining possibilities. Reread only for changed sources, missing context or a specific verification need."
        ))
    }

    /// Several unchanged observations after corrective feedback are not further investigation.
    /// Intervening reads, commands and failures cannot reset an observation's repeat count.
    pub fn stop_message(&self) -> Option<String> {
        (self.unchanged >= 4).then(|| "Stopped: the same observation was repeated four times without changing despite investigation feedback. The work remains incomplete; reuse the existing evidence and choose a different next action before continuing.".into())
    }
}

/// Shared result identity for duplicate detection and completion progress.
/// Source-tagged rows identify their own path/range; storage handles and range
/// footers are not new evidence. Unstructured observations retain their arguments.
pub(super) fn observation_key(call: &ToolCall, text: &str) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    call.name.hash(&mut hash);
    let anchored_read = call.name == "read"
        && text.lines().next().is_some_and(|header| {
            header
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .and_then(|h| h.rsplit_once('#'))
                .is_some_and(|(path, tag)| {
                    !path.is_empty() && tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_hexdigit())
                })
        });
    if !anchored_read {
        super::guard::normalized_arguments(call)
            .to_string()
            .hash(&mut hash);
    }
    for line in text.lines().filter(|line| {
        !(anchored_read && line.starts_with("[lines ")
            || call.name == "bash"
                && line.starts_with("[complete output: artifact://")
                && line.ends_with(']'))
    }) {
        line.hash(&mut hash);
    }
    hash.finish()
}

/// Turn-local record of each read target's last full result. An identical re-read whose earlier result is still
/// shown in full gets a short pointer instead of the same text again.
#[derive(Default)]
pub(super) struct ReturnedReads {
    seen: std::collections::HashMap<String, (u64, String)>,
}

impl ReturnedReads {
    /// Pointer text to send instead of `text`, or None when the full result must be sent (and is recorded).
    pub fn pointer(
        &mut self,
        call: &ToolCall,
        text: &str,
        is_error: bool,
        still_full: impl Fn(&str) -> bool,
    ) -> Option<String> {
        if call.name != "read" || is_error {
            return None;
        }
        let target = ["path", "offset", "limit"]
            .map(|name| call.arguments[name].to_string())
            .join("\0");
        let key = observation_key(call, text);
        if let Some((previous, id)) = self.seen.get(&target)
            && *previous == key
            && still_full(id)
        {
            return Some(format!(
                "[Unchanged read] This read of the same range returned identical content in tool call {id}, which is still shown above. Reuse that text; reread only after a change or if it is no longer visible."
            ));
        }
        if self.seen.len() >= 256 {
            self.seen.clear();
        }
        self.seen.insert(target, (key, call.id.clone()));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "first-read".into(),
            name: name.into(),
            arguments: args,
            malformed_arguments: false,
        }
    }

    #[test]
    fn unchanged_reads_redirect_but_changed_results_and_mutations_do_not() {
        let mut guard = InvestigationGuard::default();
        let read = call("read", json!({"path":"src/main.rs", "i":"inspect"}));
        assert!(guard.observe(&read, "old", false).is_none());
        let again = call("read", json!({"path":"src/main.rs", "i":"inspect again"}));
        assert!(
            guard
                .observe(&again, "old", false)
                .unwrap()
                .contains("first-read")
        );
        assert!(guard.observe(&again, "new", false).is_none());
        assert!(guard.observe(&again, "new", true).is_none());
        guard.observe_output(
            &call("edit", json!({})),
            &crate::tools::ToolOutput::structured(json!({})).with_changes(["changed.rs".into()]),
        );
        assert!(guard.observe(&again, "new", false).is_none());
    }

    #[test]
    fn repeated_successful_shell_checks_stop_even_when_timeout_and_artifact_change() {
        let mut guard = InvestigationGuard::default();
        let command = "cd project && grep '^version' pyproject.toml && grep '__version__' src/app/__init__.py";
        for attempt in 0..5 {
            let check = call("bash", json!({"command":command, "timeout":30+attempt}));
            let text = format!(
                "version = \"0.3.31\"\n__version__ = \"0.3.31\"\n[exit code: 0]\n[complete output: artifact://{attempt}]\n"
            );
            assert_eq!(guard.observe(&check, &text, false).is_some(), attempt > 0);
            assert_eq!(guard.stop_message().is_some(), attempt == 4);
        }
    }

    #[test]
    fn shell_checks_allow_new_results_different_commands_and_verification_after_edits() {
        let mut guard = InvestigationGuard::default();
        let check = call("bash", json!({"command":"cat version.txt"}));
        let old = "0.3.31\n[exit code: 0]\n";
        assert!(guard.observe(&check, old, false).is_none());
        assert!(guard.observe(&check, old, true).is_none());
        assert!(guard.observe(&check, old, false).is_some());
        assert!(
            guard
                .observe(&check, "0.3.32\n[exit code: 0]\n", false)
                .is_none()
        );
        assert!(
            guard
                .observe(
                    &call("bash", json!({"command":"cat other-version.txt"})),
                    old,
                    false
                )
                .is_none()
        );
        guard.observe_output(
            &call("edit", json!({"path":"version.txt"})),
            &crate::tools::ToolOutput::structured(json!({})).with_changes(["version.txt".into()]),
        );
        assert!(guard.observe(&check, old, false).is_none());
    }

    #[test]
    fn independent_ranges_and_recovery_are_allowed_and_storage_is_bounded() {
        let mut guard = InvestigationGuard::default();
        for line in 0..200 {
            assert!(
                guard
                    .observe(
                        &call("read", json!({"path":"x", "offset":line})),
                        "same",
                        false
                    )
                    .is_none()
            );
        }
        assert_eq!(guard.observations.len(), 128);
        for _ in 0..3 {
            assert!(
                guard
                    .observe(
                        &call("artifact_read", json!({"uri":"artifact:x"})),
                        "same",
                        false
                    )
                    .is_none()
            );
        }
    }

    #[test]
    fn repeated_evidence_stops_but_new_evidence_rearms() {
        let mut guard = InvestigationGuard::default();
        let read = call("read", json!({"path":"a.rs"}));
        guard.observe(&read, "original", false);
        for _ in 0..3 {
            guard.observe(&read, "original", false);
            assert!(guard.stop_message().is_none());
            guard.observe(&call("todo", json!({"op":"view"})), "plan", false);
        }
        guard.observe(&read, "original", false);
        assert!(guard.stop_message().is_some());
        guard.observe(&read, "changed externally", false);
        assert!(guard.stop_message().is_none());
    }

    #[test]
    fn intervening_checks_errors_and_other_reads_do_not_hide_a_loop() {
        let mut guard = InvestigationGuard::default();
        let read = call("read", json!({"path":"src/steering.rs"}));
        guard.observe(&read, "pub(crate) fn local_unsupported_reason", false);
        for attempt in 0..4 {
            guard.observe(
                &call("bash", json!({"command":"cargo check"})),
                "subagents.rs: compile error",
                true,
            );
            guard.observe(
                &call("bash", json!({"command":"git status"})),
                "unchanged",
                false,
            );
            guard.observe(
                &call("read", json!({"path":format!("other-{attempt}.rs")})),
                "other evidence",
                false,
            );
            assert!(
                guard
                    .observe(&read, "pub(crate) fn local_unsupported_reason", false)
                    .is_some()
            );
        }
        assert!(guard.stop_message().is_some());
        assert!(guard.observe(&read, "changed externally", false).is_none());
        assert!(guard.stop_message().is_none());
    }

    #[test]
    fn equivalent_anchored_reads_share_evidence_but_new_ranges_and_snapshots_do_not() {
        let mut guard = InvestigationGuard::default();
        let original = call("read", json!({"path":"a.rs:1-2"}));
        let text = "[a.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n";
        assert!(guard.observe(&original, text, false).is_none());
        for args in [
            json!({"path":"a.rs", "offset":1, "limit":2}),
            json!({"path":"a.rs:1+2"}),
            json!({"path":"./a.rs:1-2"}),
            json!({"path":"a.rs:1-2", "limit":50}),
        ] {
            assert!(guard.observe(&call("read", args), text, false).is_some());
        }
        assert!(guard.stop_message().is_some());
        assert!(
            guard
                .observe(
                    &call("read", json!({"path":"a.rs", "offset":1, "limit":2})),
                    "[a.rs#AB12]\n1:one\n2:two\n",
                    false
                )
                .is_some()
        );
        assert!(
            guard
                .observe(&original, "[a.rs#AB12]\n3:three\n[lines 3 of 3]\n", false)
                .is_none()
        );
        assert!(guard.stop_message().is_none());
        assert!(
            guard
                .observe(
                    &original,
                    "[a.rs#CD34]\n1:changed\n2:two\n[lines 1-2 of 3]\n",
                    false
                )
                .is_none()
        );
        assert!(
            guard
                .observe(
                    &call("read", json!({"path":"b.rs:1-2"})),
                    "[b.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n",
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn no_op_edits_and_todo_polling_do_not_erase_version_check_loops() {
        let mut guard = InvestigationGuard::default();
        let check = call("bash", json!({"command":"grep '^version' pyproject.toml"}));
        for attempt in 0..5 {
            assert_eq!(
                guard.observe(&check, "version = 0.3.31", false).is_some(),
                attempt > 0
            );
            guard.observe(&call("todo", json!({"op":"view"})), "unfinished", false);
            guard.observe(
                &call("edit", json!({})),
                "No changes: the edit leaves the file identical. Nothing was written.",
                false,
            );
            guard.observe(
                &call("write", json!({})),
                "change preview (+0 -0 lines)",
                false,
            );
            assert_eq!(guard.stop_message().is_some(), attempt == 4);
        }
        guard.observe_output(
            &call("edit", json!({})),
            &crate::tools::ToolOutput::structured(json!({})).with_changes(["changed.rs".into()]),
        );
        assert!(guard.stop_message().is_none());
        assert!(guard.observe(&check, "version = 0.3.31", false).is_none());
    }

    #[test]
    fn unchanged_read_gets_pointer_only_while_earlier_result_is_full() {
        let mut returned = ReturnedReads::default();
        let read = call("read", json!({"path":"src/lib.rs"}));
        assert!(
            returned
                .pointer(&read, "1: fn a() {}", false, |_| true)
                .is_none()
        );
        assert!(
            returned
                .pointer(&read, "1: fn a() {}", false, |_| false)
                .is_none()
        );
        assert!(
            returned
                .pointer(&read, "1: fn a() {}", false, |_| true)
                .is_some()
        );
        assert!(
            returned
                .pointer(&read, "1: fn b() {}", false, |_| true)
                .is_none()
        );
    }
}
