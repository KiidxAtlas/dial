//! Shared turn execution policy. Controllers supply role-specific admission, persistence and UI;
//! recovery decisions and result settlement live here for both parent and worker turns.
use super::{guard, investigation, recovery_checkpoint, turn_recovery};
use crate::{
    llm::ToolCall,
    tools::{
        ToolOutput, Workspace,
        outcome::{self, Effects},
    },
};

#[derive(Default)]
pub(super) struct Engine {
    checkpoint: recovery_checkpoint::RecoveryCheckpoint,
    repeats: guard::RepeatGuard,
    investigation: investigation::InvestigationGuard,
    returned_reads: investigation::ReturnedReads,
    pub narration: guard::NarrationGuard,
    pub recovery: turn_recovery::Recovery,
    uncertain: bool,
    length_nudges: u32,
}

pub(super) enum Tick<R> {
    Again,
    Exhausted,
    Finished(R),
}

/// Role adapters translate the shared terminal policy into their report type.
pub(super) trait Terminal {
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
        if matches!(action, guard::LengthAction::Nudge { .. }) {
            self.length_nudges += 1;
        }
        action
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
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn rejection_at(&mut self, workspace: &Workspace, call: &ToolCall) -> Option<String> {
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
    pub fn settle(
        &mut self,
        workspace: &Workspace,
        call: &ToolCall,
        output: &mut ToolOutput,
        check_failed: bool,
        task_changed: bool,
        holds_result: impl Fn(&str) -> bool,
    ) -> (Option<String>, Option<String>, Option<String>) {
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
        let mutated = !output.execution.changed_paths.is_empty();
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
        let result = self.checkpoint.observe_at(
            workspace,
            call,
            output,
            &mut self.investigation,
            &mut self.repeats,
        );
        if let Some(pointer) =
            self.returned_reads
                .pointer(call, &output.text, output.is_error, holds_result)
        {
            output.text = pointer;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    impl Terminal for () {
        fn prevent_completion(&mut self, _: &str) {}
    }
    impl Terminal for u32 {
        fn prevent_completion(&mut self, _: &str) {}
    }
    #[test]
    fn shared_driver_cannot_publish_a_completed_report_after_uncertain_effects() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let call = ToolCall {
            id: "effect".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
            malformed_arguments: false,
        };
        let mut engine = Engine::default();
        let mut output = ToolOutput::err("execution disappeared").with_effects(Effects::Unknown);
        engine.settle(&workspace, &call, &mut output, false, false, |_| false);
        let report = engine
            .drive(|_| {
                Ok(Tick::Finished(super::super::TurnReport {
                    text: "candidate report".into(),
                    outcome: super::super::TurnOutcome::Completed,
                    steps: 1,
                    interrupted: false,
                    trajectory: None,
                }))
            })
            .unwrap()
            .unwrap();
        assert_eq!(report.outcome, super::super::TurnOutcome::Incomplete);
        assert!(report.text.contains("uncertain effects"));
    }
    #[test]
    fn shared_driver_refuses_automatic_retry_after_an_uncertain_effect() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let call = ToolCall {
            id: "effect".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
            malformed_arguments: false,
        };
        let mut engine = Engine::default();
        let mut cycles = 0;
        let result = engine.drive::<()>(|engine| {
            cycles += 1;
            let mut output = ToolOutput::err("process vanished").with_effects(Effects::Unknown);
            engine.settle(&workspace, &call, &mut output, false, false, |_| false);
            Ok(Tick::Again)
        });
        assert!(result.is_err());
        assert_eq!(cycles, 1);
    }
    #[test]
    fn productive_driver_has_no_hidden_global_step_limit() {
        let mut engine = Engine::default();
        let mut cycles = 0;
        let result = engine
            .drive(|_| {
                cycles += 1;
                Ok(if cycles == 128 {
                    Tick::Finished(cycles)
                } else {
                    Tick::Again
                })
            })
            .unwrap();
        assert_eq!(result, Some(128));
    }
}
