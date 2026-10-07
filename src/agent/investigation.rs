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
    pub fn observe(&mut self, call: &ToolCall, text: &str, is_error: bool) -> Option<String> {
        // A failed check is not progress and must not erase earlier duplicate evidence.
        if is_error {
            return None;
        }
        match call.name.as_str() {
            // Successful edits allow fresh verification. Commands alone do not prove
            // a change; observations are executed again and compared with their result.
            "write" | "edit" | "ast_edit" => {
                self.observations.clear();
                self.unchanged = 0;
                return None;
            }
            "read" | "grep" | "glob" | "find" | "ast_search" | "bash" => {}
            // Polls, artifact recovery and control calls are not duplicate observations.
            _ => return None,
        }
        let mut args = call.arguments.clone();
        if let Some(object) = args.as_object_mut() {
            object.remove("i");
            if call.name == "bash" {
                object.remove("timeout");
            }
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        call.name.hash(&mut hash);
        // Hashline reads identify the source snapshot and returned line range themselves.
        // Equivalent selectors/offsets must not disguise the same observation. Raw and
        // unstructured outputs still need arguments to distinguish sources and ranges.
        let anchored_read = call.name == "read"
            && text.lines().next().is_some_and(|header| {
                header
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .and_then(|h| h.rsplit_once('#'))
                    .is_some_and(|(path, tag)| {
                        !path.is_empty()
                            && tag.len() == 4
                            && tag.bytes().all(|b| b.is_ascii_hexdigit())
                    })
            });
        if !anchored_read {
            args.to_string().hash(&mut hash);
        }
        if anchored_read {
            // Selector reads add a range footer; offset/limit reads may omit it. The
            // numbered rows already encode coverage, so this metadata is not new evidence.
            for line in text.lines().filter(|line| !line.starts_with("[lines ")) {
                line.hash(&mut hash);
            }
        } else if call.name == "bash" {
            // Retained-output handles describe storage, not command progress. Compare
            // the command's result (including exit status), not a newly allocated URI.
            for line in text.lines().filter(|line| {
                !(line.starts_with("[complete output: artifact://") && line.ends_with(']'))
            }) {
                line.hash(&mut hash);
            }
        } else {
            text.hash(&mut hash);
        }
        let key = hash.finish();
        if let Some((_, first, repeats)) = self
            .observations
            .iter_mut()
            .find(|(seen, _, _)| *seen == key)
        {
            *repeats += 1;
            self.unchanged = *repeats;
            return Some(format!(
                "[Investigation check from dial] This observation is unchanged from tool call {first}. Reuse that evidence; do not reread it for reassurance. If the answer supports a change and its check, implement and verify now, or deliver the requested finding. If unresolved, choose one narrower check that distinguishes the remaining possibilities. Reread only for changed sources, missing context or a specific verification need."
            ));
        }
        self.unchanged = 0;
        if self.observations.len() == 128 {
            self.observations.pop_front();
        }
        self.observations.push_back((key, call.id.clone(), 0));
        None
    }

    /// Several unchanged observations after corrective feedback are not further investigation.
    /// Intervening reads, commands and failures cannot reset an observation's repeat count.
    pub fn stop_message(&self) -> Option<String> {
        (self.unchanged >= 4).then(|| "Stopped: the same observation was repeated four times without changing despite investigation feedback. The work remains incomplete; reuse the existing evidence and choose a different next action before continuing.".into())
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
        guard.observe(&call("edit", json!({})), "edited", false);
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
        guard.observe(
            &call("edit", json!({"path":"version.txt"})),
            "edited",
            false,
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
}
