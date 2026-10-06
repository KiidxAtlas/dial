//! Bounded, turn-local detection of successful observations that add no new evidence.
//! Execute first: a source may have changed externally, and a reread can be legitimate.
use crate::llm::ToolCall;
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

#[derive(Default)]
pub(super) struct InvestigationGuard {
    observations: VecDeque<(u64, String)>,
    unchanged: usize,
}

impl InvestigationGuard {
    pub fn observe(&mut self, call: &ToolCall, text: &str, is_error: bool) -> Option<String> {
        // Error recovery has its own guards; it is not a run of successful unchanged observations.
        if is_error {
            self.unchanged = 0;
            return None;
        }
        match call.name.as_str() {
            // Mutations and arbitrary commands may invalidate earlier observations.
            "write" | "edit" | "ast_edit" | "bash" => {
                self.observations.clear();
                self.unchanged = 0;
                return None;
            }
            "read" | "grep" | "glob" | "find" | "ast_search" => {}
            // Polls, checks, artifact recovery and control calls are not duplicate reads.
            _ => return None,
        }
        let mut args = call.arguments.clone();
        if let Some(object) = args.as_object_mut() {
            object.remove("i");
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        call.name.hash(&mut hash);
        args.to_string().hash(&mut hash);
        text.hash(&mut hash);
        let key = hash.finish();
        if let Some((_, first)) = self.observations.iter().find(|(seen, _)| *seen == key) {
            self.unchanged += 1;
            return Some(format!(
                "[Investigation check from dial] This observation is unchanged from tool call {first}. Reuse that evidence and record the finding on the current investigation todo. If the question is still unresolved, choose a different, narrower check that can distinguish the remaining possibilities. Reread only for changed sources, missing context or a specific verification need; once the answer supports a change and its check, move on."
            ));
        }
        self.unchanged = 0;
        if self.observations.len() == 128 {
            self.observations.pop_front();
        }
        self.observations.push_back((key, call.id.clone()));
        None
    }

    /// Several unchanged observations after corrective feedback are not further investigation.
    /// New evidence or an operation that may change the workspace rearms the guard.
    pub fn stop_message(&self) -> Option<String> {
        (self.unchanged >= 4).then(|| "Stopped: four consecutive observations repeated unchanged evidence despite investigation feedback. No further useful evidence was obtained. The work remains incomplete; narrow the unresolved question before continuing.".into())
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
}
