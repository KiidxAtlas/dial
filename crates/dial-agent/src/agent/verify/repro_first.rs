//! `experimental.repro_first`: on a debugging task whose request explicitly asks for
//! verification ("verify", "verification"), the first edit to a non-test source file before any
//! reproduction has failed this turn gets a note (`advise`) or is refused once (`gate`). A
//! failure counts when a `bash` test command fails or a `check` result is `code_failed`. It fires
//! at most once per turn; a second attempt proceeds. Without a verification request the model
//! decides how much reproduction a fix needs. The advice follows an edit that already applied, so
//! it asks for a focused check of the reported behavior; only `gate`, which acts before the edit,
//! asks for a reproduction first.

use crate::agent::Agent;
use crate::config::types::ReproFirst;
use crate::learn::task_policy::Kind;
use crate::llm::ToolCall;

pub(in crate::agent) const GATE: &str = "Not applied (repro_first gate): this debugging request asks for verification and no reproduction has failed yet this turn. Reproduce the bug first, for example with `verify` `repro` (a command that fails while the bug is present) or a failing test, then edit. This refusal happens once per turn; the next edit attempt proceeds.";
pub(in crate::agent) const ADVICE: &str = "\n[repro_first: this debugging fix was applied before any reproduction failed this turn. Confirm it with a focused check of the reported behavior, such as one command asserting the expected result; reproducing the failure after the fix is not required.]";

/// Docs, notes and test files do not count as fixing source.
fn source_target(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !crate::commit::agentic::validation::is_test_path(&lower)
        && ![".md", ".txt", ".rst", ".adoc"]
            .iter()
            .any(|extension| lower.ends_with(extension))
}

/// Whether the policy applies to an edit of `targets` in the current turn state.
pub(in crate::agent) fn applies(
    mode: ReproFirst,
    kind: Kind,
    verification_requested: bool,
    reproduced: bool,
    fired: bool,
    targets: &[String],
) -> bool {
    mode != ReproFirst::Off
        && kind == Kind::Debugging
        && verification_requested
        && !reproduced
        && !fired
        && targets.iter().any(|target| source_target(target))
}

impl Agent<'_> {
    /// File targets of a native edit call; empty for every other tool.
    fn repro_first_targets(&self, call: &ToolCall) -> Vec<String> {
        match call.name.as_str() {
            "edit" => crate::tools::edit::editing::targets(self.edit_mode(), &call.arguments)
                .unwrap_or_default(),
            "write" | "ast_edit" => call.arguments["path"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Consume the once-per-turn trigger when `mode` is configured and the policy applies.
    pub(in crate::agent) fn repro_first_fires(
        &mut self,
        call: &ToolCall,
        mode: ReproFirst,
    ) -> bool {
        if self.config.experimental.repro_first != mode {
            return false;
        }
        let targets = self.repro_first_targets(call);
        let fires = applies(
            mode,
            self.task_policy.kind,
            self.task_policy.verification_requested,
            self.step_checks.reproduced,
            self.step_checks.repro_first_fired,
            &targets,
        );
        self.step_checks.repro_first_fired |= fires;
        fires
    }
}

#[cfg(test)]
mod tests;
