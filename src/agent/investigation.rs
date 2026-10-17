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

    /// Forgets observations whose result the model no longer sees in full (compacted, pruned or
    /// superseded). A repeat of such a result is new evidence: it executes and returns full content.
    pub fn retain_visible(&mut self, visible: impl Fn(&str) -> bool) {
        self.observations.retain(|(_, id, _)| visible(id.as_str()));
    }

    /// Releases all duplicate memory, e.g. after compaction rewrote the context.
    pub fn forget(&mut self) {
        self.observations.clear();
        self.unchanged = 0;
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

    /// Several unchanged observations are not further investigation. Intervening reads, commands and
    /// failures cannot reset an observation's repeat count. Guidance escalates at each further multiple
    /// of four and never ends the turn: the repeated observation is already withheld.
    pub fn escalation(&self) -> Option<String> {
        (self.unchanged >= 4 && self.unchanged.is_multiple_of(4)).then(|| format!(
            "[Escalation from dial] This observation has now been repeated {} times unchanged and withheld each time. Its evidence is already in this conversation. Stop re-checking it: act on it, or run one check of a different kind.",
            self.unchanged
        ))
    }
}

/// Shared result identity for duplicate detection and completion progress.
/// Source-tagged rows identify their own path/range; storage handles and range
/// footers are not new evidence. Unstructured observations retain their arguments.
pub(super) fn observation_key(call: &ToolCall, text: &str) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    call.name.hash(&mut hash);
    let anchored_read = call.name == "read"
        && text
            .lines()
            .next()
            .is_some_and(|line| source_header(line).is_some());
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

/// `path#TAG` of a read result's `[path#TAG]` header line. The tag names the file content, so a
/// changed file has a different header.
fn source_header(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, tag) = inner.rsplit_once('#')?;
    (!path.is_empty() && tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(inner)
}

/// Inclusive source-line run a read result showed.
type Run = (u32, u32);

/// The line runs each `[path#TAG]` section of a read result showed. Empty when the result is not
/// made of anchored rows, or when any section shows none (e.g. a per-path error), so that such a
/// result is never replaced by a pointer.
fn shown_runs(text: &str) -> Vec<(&str, Vec<Run>)> {
    let mut sections: Vec<(&str, Vec<Run>)> = Vec::new();
    for line in text.lines() {
        if let Some(header) = source_header(line) {
            sections.push((header, Vec::new()));
            continue;
        }
        let Some(number) = line
            .split_once(':')
            .and_then(|(digits, _)| digits.parse::<u32>().ok())
        else {
            continue;
        };
        let Some((_, runs)) = sections.last_mut() else {
            continue;
        };
        match runs.last_mut() {
            Some(last) if last.1.checked_add(1) == Some(number) => last.1 = number,
            _ => runs.push((number, number)),
        }
    }
    if sections.iter().any(|(_, runs)| runs.is_empty()) {
        sections.clear();
    }
    sections
}

/// Opening of the pointer that answers an identical re-read whose earlier full result is still shown.
pub(super) const UNCHANGED_READ: &str = "[Unchanged read]";

/// Whether a withheld or replacement body is such a pointer rather than advice or the result itself.
pub(super) fn is_unchanged_read(body: &str) -> bool {
    body.starts_with(UNCHANGED_READ)
}

/// Turn-local record of each read target's last full result. An identical re-read whose earlier result is still
/// shown in full gets a short pointer instead of the same text again.
#[derive(Default)]
pub(super) struct ReturnedReads {
    seen: std::collections::HashMap<String, (u64, String)>,
    /// Line runs shown in full by earlier reads, per `path#TAG`, with the call that returned them.
    shown: std::collections::HashMap<String, Vec<(Run, String)>>,
}

impl ReturnedReads {
    const MAX_FILES: usize = 256;
    const MAX_RUNS_PER_FILE: usize = 64;

    /// Pointer text to send instead of `text`, or None when the full result must be sent (and is recorded).
    /// A read is also answered by a pointer when every line it shows lies inside lines an earlier
    /// read of the same unchanged file returned, and those results are still shown in full.
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
                "{UNCHANGED_READ} This read of the same range returned identical content in tool call {id}, which is still shown above. Reuse that text; reread only after a change or if it is no longer visible."
            ));
        }
        let sections = shown_runs(text);
        if let Some(ids) = self.covering(&sections, &still_full) {
            return Some(format!(
                "{UNCHANGED_READ} Every line of this read was already returned unchanged in tool call {}, which is still shown above. Reuse that text; reread only after a change or if it is no longer visible.",
                ids.join(", ")
            ));
        }
        if self.seen.len() >= 256 {
            self.seen.clear();
        }
        self.seen.insert(target, (key, call.id.clone()));
        for (header, runs) in sections {
            if self.shown.len() >= Self::MAX_FILES && !self.shown.contains_key(header) {
                self.shown.clear();
            }
            let known = self.shown.entry(header.to_owned()).or_default();
            known.extend(runs.into_iter().map(|run| (run, call.id.clone())));
            if known.len() > Self::MAX_RUNS_PER_FILE {
                known.drain(..known.len() - Self::MAX_RUNS_PER_FILE);
            }
        }
        None
    }

    /// Calls whose still-visible results together contain every line of `sections`.
    fn covering(
        &self,
        sections: &[(&str, Vec<Run>)],
        still_full: &impl Fn(&str) -> bool,
    ) -> Option<Vec<String>> {
        if sections.is_empty() {
            return None;
        }
        let mut ids: Vec<String> = Vec::new();
        for (header, runs) in sections {
            let known = self.shown.get(*header)?;
            for &(first, last) in runs {
                let (_, id) = known
                    .iter()
                    .find(|((from, to), id)| *from <= first && last <= *to && still_full(id))?;
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }
        Some(ids)
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
    fn repeated_successful_shell_checks_escalate_even_when_timeout_and_artifact_change() {
        let mut guard = InvestigationGuard::default();
        let command = "cd project && grep '^version' pyproject.toml && grep '__version__' src/app/__init__.py";
        for attempt in 0..5 {
            let check = call("bash", json!({"command":command, "timeout":30+attempt}));
            let text = format!(
                "version = \"0.3.31\"\n__version__ = \"0.3.31\"\n[exit code: 0]\n[complete output: artifact://{attempt}]\n"
            );
            assert_eq!(guard.observe(&check, &text, false).is_some(), attempt > 0);
            assert_eq!(guard.escalation().is_some(), attempt == 4);
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
    fn repeated_evidence_escalates_but_new_evidence_rearms() {
        let mut guard = InvestigationGuard::default();
        let read = call("read", json!({"path":"a.rs"}));
        guard.observe(&read, "original", false);
        for _ in 0..3 {
            guard.observe(&read, "original", false);
            assert!(guard.escalation().is_none());
            guard.observe(&call("todo", json!({"op":"view"})), "plan", false);
        }
        guard.observe(&read, "original", false);
        assert!(guard.escalation().is_some());
        guard.observe(&read, "changed externally", false);
        assert!(guard.escalation().is_none());
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
        assert!(guard.escalation().is_some());
        assert!(guard.observe(&read, "changed externally", false).is_none());
        assert!(guard.escalation().is_none());
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
        assert!(guard.escalation().is_some());
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
        assert!(guard.escalation().is_none());
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
            assert_eq!(guard.escalation().is_some(), attempt == 4);
        }
        guard.observe_output(
            &call("edit", json!({})),
            &crate::tools::ToolOutput::structured(json!({})).with_changes(["changed.rs".into()]),
        );
        assert!(guard.escalation().is_none());
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

    fn rows(tag: &str, lines: std::ops::RangeInclusive<u32>) -> String {
        let mut text = format!("[src/lib.rs#{tag}]\n[Source raw SHA256:00]\n");
        for line in lines {
            text.push_str(&format!("{line}: code {line}\n"));
        }
        text
    }

    fn read_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            ..call("read", json!({ "path": path }))
        }
    }

    #[test]
    fn a_read_inside_lines_already_shown_gets_a_pointer_to_that_call() {
        let mut returned = ReturnedReads::default();
        let whole = read_call("whole", "src/lib.rs");
        assert!(
            returned
                .pointer(&whole, &rows("AAAA", 1..=100), false, |_| true)
                .is_none()
        );
        let part = read_call("part", "src/lib.rs:40-60");
        let pointer = returned
            .pointer(&part, &rows("AAAA", 40..=60), false, |_| true)
            .expect("contained read is answered by a pointer");
        assert!(is_unchanged_read(&pointer) && pointer.contains("whole"));
    }

    #[test]
    fn a_read_is_sent_in_full_when_it_adds_lines_the_file_changed_or_the_earlier_result_is_gone() {
        let mut returned = ReturnedReads::default();
        let whole = read_call("whole", "src/lib.rs:1-100");
        returned.pointer(&whole, &rows("AAAA", 1..=100), false, |_| true);
        let send = |returned: &mut ReturnedReads, id, text: String, full: bool| {
            returned.pointer(&read_call(id, id), &text, false, |_| full)
        };
        // One line past the earlier result.
        assert!(send(&mut returned, "past", rows("AAAA", 90..=101), true).is_none());
        // Same lines but the file changed since (new tag).
        assert!(send(&mut returned, "edited", rows("BBBB", 40..=60), true).is_none());
        // The earlier result is no longer shown in full.
        assert!(send(&mut returned, "pruned", rows("AAAA", 40..=60), false).is_none());
        // A result with no source rows is never replaced.
        assert!(send(&mut returned, "raw", "plain text".into(), true).is_none());
    }

    #[test]
    fn multi_path_reads_are_replaced_only_when_every_section_is_covered() {
        let mut returned = ReturnedReads::default();
        let first = read_call("first", "a");
        let both = format!(
            "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
            rows("AAAA", 1..=50)
        );
        assert!(returned.pointer(&first, &both, false, |_| true).is_none());
        let again = format!(
            "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
            rows("AAAA", 5..=9)
        );
        assert!(
            returned
                .pointer(&read_call("second", "b"), &again, false, |_| true)
                .is_some()
        );
        let extra = format!("{}[src/new.rs#DDDD]\n1: new\n", rows("AAAA", 5..=9));
        assert!(
            returned
                .pointer(&read_call("third", "c"), &extra, false, |_| true)
                .is_none()
        );
    }
}
