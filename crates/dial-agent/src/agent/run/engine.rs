//! Shared turn execution policy. Controllers supply role-specific admission, persistence and UI;
//! recovery decisions and result settlement live here for both parent and worker turns.
use crate::agent::{
    run::turn_recovery, safety::guard, verify::mutation_cycles, workflow::investigation,
    workflow::recovery_checkpoint,
};
use crate::runtime::tool_output::ToolOutput;
use crate::{
    llm::ToolCall,
    tools::{
        Workspace,
        surface::outcome::{self, Effects},
    },
};

#[derive(Default)]
pub(in crate::agent) struct Engine {
    checkpoint: recovery_checkpoint::RecoveryCheckpoint,
    repeats: guard::RepeatGuard,
    investigation: investigation::InvestigationGuard,
    returned_reads: investigation::ReturnedReads,
    pub narration: guard::NarrationGuard,
    pub recovery: turn_recovery::Recovery,
    uncertain: bool,
    /// Committed-content history and refused loop calls; deliberately not cleared by successful
    /// edits, reads or todos. A cycle steers the model and never ends the turn.
    cycles: mutation_cycles::MutationCycles,
    length_nudges: u32,
    length_stalled: u32,
    // Brent's online cycle detector: constant memory, no recent-history eviction horizon.
    length_anchor: Option<u64>,
    length_power: u64,
    length_distance: u64,
    length_cycle: bool,
    /// Consecutive failures per (tool, recovery kind); a tool's success clears its entries. The
    /// first failure gets compact feedback, a repeat the full diagnose-first contract. Kept across
    /// continuations and compaction, like the other failure counters.
    failures: std::collections::BTreeMap<(String, &'static str), u32>,
}

/// Bound on distinct (tool, kind) failure streaks; past it the streaks are cleared and a repeat
/// is simply treated as a first failure again.
const MAX_FAILURE_STREAKS: usize = 64;

/// State lifetimes are explicit: new instructions release old call restrictions; retries do not.
#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::agent) enum RecoveryTransition {
    Steering,
    Coordination,
    AutomaticContinue,
    DirectedContinue,
}

pub(in crate::agent) enum Tick<R> {
    Again,
    Exhausted,
    Finished(R),
}

/// Role adapters translate the shared terminal policy into their report type.
pub(in crate::agent) trait Terminal {
    fn prevent_completion(&mut self, reason: &str);
}

impl Engine {
    pub fn length(
        &mut self,
        requested: u32,
        window: u32,
        prompt: u64,
        reply: &crate::llm::Reply,
    ) -> guard::LengthAction {
        let action =
            guard::recover_from_length(requested, window, prompt, reply, self.length_nudges);
        if let guard::LengthAction::Nudge { partial, .. } = &action {
            use std::hash::{Hash, Hasher};
            const MAX_STALLED: u32 = 8;
            let advancing = partial.as_ref().is_some_and(|text| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                for word in text.split_whitespace() {
                    word.hash(&mut hash);
                }
                let fingerprint = hash.finish();
                if self.length_cycle {
                    return false;
                }
                let Some(anchor) = self.length_anchor else {
                    self.length_anchor = Some(fingerprint);
                    self.length_power = 1;
                    return true;
                };
                self.length_distance = self.length_distance.saturating_add(1);
                if anchor == fingerprint {
                    // Once a recurrence is observed, changing partial fragments cannot hide it.
                    // Require a usable nontruncated reply within the remaining stall allowance.
                    self.length_cycle = true;
                    return false;
                }
                if self.length_distance >= self.length_power {
                    self.length_anchor = Some(fingerprint);
                    self.length_power = self.length_power.saturating_mul(2);
                    self.length_distance = 0;
                }
                true
            });
            if advancing {
                self.length_stalled = 0;
                self.length_nudges = 0;
            } else {
                self.length_stalled += 1;
                if self.length_stalled >= MAX_STALLED {
                    return guard::LengthAction::Incomplete("Stopped with incomplete work: repeated output truncation made no progress after bounded recovery. No partial tool calls were executed; completed effects and the conversation are kept; no completion was recorded.".into());
                }
            }
            self.length_nudges = self.length_nudges.saturating_add(1);
        }
        action
    }
    /// A usable, nontruncated answer or tool call re-arms length recovery. Empty or malformed
    /// replies do not hide an ongoing stall by alternating with truncated generations.
    pub fn length_recovered(&mut self, reply: &crate::llm::Reply) {
        if !reply.dropped_tool_call()
            && (!reply.text.trim().is_empty() || !reply.tool_calls.is_empty())
        {
            self.length_stalled = 0;
            self.length_nudges = 0;
            self.length_anchor = None;
            self.length_power = 0;
            self.length_distance = 0;
            self.length_cycle = false;
        }
    }
    /// One lifecycle driver for foreground and worker adapters. Adapters supply admission,
    /// role-specific request preparation and persistence; they never own a second retry loop.
    pub fn drive<R: Terminal>(
        &mut self,
        mut advance: impl FnMut(&mut Self) -> anyhow::Result<Tick<R>>,
    ) -> anyhow::Result<Option<R>> {
        loop {
            match advance(self)? {
                Tick::Finished(mut result) => {
                    if self.uncertain {
                        result.prevent_completion("Stopped because a tool ended with uncertain effects. Inspect affected state before retrying; completed effects were not rolled back or replayed.");
                    }
                    return Ok(Some(result));
                }
                Tick::Exhausted => return Ok(None),
                Tick::Again => anyhow::ensure!(
                    self.pending_refusal().is_none(),
                    "automatic continuation refused after uncertain effects"
                ),
            }
        }
    }
    pub fn pending_refusal(&self) -> Option<&'static str> {
        self.uncertain.then_some("This call was not executed: an earlier tool ended with uncertain effects. Inspect affected state before starting further actions.")
    }
    pub fn transition(&mut self, reason: RecoveryTransition) {
        match reason {
            RecoveryTransition::Steering | RecoveryTransition::Coordination => {
                // Even a new instruction cannot turn an uninspected partial effect into permission.
                let uncertain = self.uncertain;
                *self = Self::default();
                self.uncertain = uncertain;
            }
            RecoveryTransition::AutomaticContinue | RecoveryTransition::DirectedContinue => {
                // A retry changes the approach, not the evidence or committed-content history.
                self.narration = Default::default();
                self.recovery.reset();
            }
        }
    }
    pub fn rejection_at(&mut self, workspace: &Workspace, call: &ToolCall) -> Option<String> {
        if let Some(path) = self.cycles.blocked(&call_identity(call)) {
            return Some(format!(
                "This call was not executed: it is one of the edits that kept returning `{}` to content it already had earlier in this turn. Read the file's current content, decide its final state, then make one different, complete edit (for example, write the full intended content) or continue with the rest of the task.",
                workspace.rel(path)
            ));
        }
        self.checkpoint.rejection_at(workspace, call)
    }
    pub fn apply(
        &self,
        schemas: &mut Vec<serde_json::Value>,
        names: &mut Vec<String>,
        messages: &mut Vec<serde_json::Value>,
    ) {
        self.checkpoint.apply(schemas, names, messages);
    }
    pub fn arm(&mut self, call: &ToolCall, advice: &str) {
        self.checkpoint.arm(call, advice);
    }
    /// Observe actual effects before any output clipping or repetition elision. Unknown effects
    /// stop automatic continuation; retry coaching cannot turn uncertainty into permission.
    /// Committed paths are observed even when the tool reports a later error: an error does not
    /// undo a known commit. An edit loop is reported on this result and never ends the turn.
    pub fn settle(
        &mut self,
        workspace: &Workspace,
        call: &ToolCall,
        output: &mut ToolOutput,
        check_failed: bool,
        task_changed: bool,
        holds_result: impl Fn(&str) -> bool,
    ) -> (Option<String>, Option<String>, Option<String>) {
        match crate::agent::run::failure_guidance::classify(call, output) {
            Some((kind, advice)) => {
                if self.failures.len() >= MAX_FAILURE_STREAKS
                    && !self.failures.contains_key(&(call.name.clone(), kind))
                {
                    self.failures.clear();
                }
                let consecutive = self.failures.entry((call.name.clone(), kind)).or_default();
                *consecutive += 1;
                let recovery = crate::agent::run::failure_guidance::report(
                    call,
                    output,
                    kind,
                    advice,
                    *consecutive,
                );
                output.text.push_str(&recovery);
            }
            None => self.failures.retain(|(tool, _), _| *tool != call.name),
        }
        if outcome::effects(output) == Effects::Unknown {
            self.uncertain = true;
            return (
                None,
                None,
                Some(format!(
                    "Stopped: `{}` ended with uncertain effects. Inspect affected state before retrying; completed effects were not rolled back or replayed.",
                    call.name
                )),
            );
        }
        let observation = self.cycles.observe(
            workspace,
            &output.execution.changed_paths,
            &call_identity(call),
        );
        if let Some((path, refused)) = &observation.cycle {
            let file = workspace.rel(path);
            output.text.push_str(&format!(
                "\n\n[Dial: edit loop] This edit returned `{file}` to content it already had at least twice before in this turn: recent edits to it are undoing each other. The change is applied and the turn continues. The {refused} edit call(s) that produced these alternating versions will be refused if repeated. Read `{file}` once, decide its final content, then either leave it as it is or make one different, complete edit, and continue with the rest of the task."
            ));
        }
        // Returning a file to content it already had is not progress and cannot clear recovery.
        let mutated = observation.novel;
        if mutated
            || (!output.is_error
                && task_changed
                && call.name == "todo"
                && matches!(call.arguments["op"].as_str(), Some("done" | "block")))
        {
            self.narration.progress();
        }
        self.checkpoint.progress(
            call,
            !output.is_error && !check_failed,
            task_changed,
            mutated,
        );
        // Duplicate memory may only cite results the model still sees in full.
        self.investigation.retain_visible(&holds_result);
        let (duplicate, repeat, stop) = self.checkpoint.observe_at(
            workspace,
            call,
            output,
            &mut self.investigation,
            &mut self.repeats,
        );
        // An identical re-read is answered with a pointer as its replacement body. The output keeps
        // the real result, so artifacts and evidence still describe what the tool returned, and the
        // pointer is never recorded as an observation of its own.
        let pointer =
            self.returned_reads
                .pointer(call, &output.text, output.is_error, holds_result);
        (pointer.or(duplicate), repeat, stop)
    }

    /// Compaction rewrote what the model can see. Duplicate memory, pointers, cached observations
    /// and withheld calls are released, so a repeat executes and returns full content. Failure
    /// counters and the edit-cycle history are kept.
    pub fn evidence_compacted(&mut self) {
        self.investigation.forget();
        self.returned_reads = Default::default();
        self.checkpoint.forget_evidence();
    }
}

/// Normalized identity shared with the recovery checkpoint, so equivalent retries match.
fn call_identity(call: &ToolCall) -> String {
    format!("{}\0{}", call.name, guard::normalized_arguments(call))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod evidence_tests;

#[cfg(test)]
mod length_tests;
