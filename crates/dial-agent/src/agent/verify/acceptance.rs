//! Explicit, turn-scoped model helper checks reuse the normal verification execution owner.
use crate::agent::verify::verification::FocusedCheck;
use crate::agent::*;
use crate::runtime::tool_output::ToolOutput;

impl Agent<'_> {
    pub(in crate::agent) fn register_acceptance(
        &mut self,
        args: &Value,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        if ["targets", "repro", "command", "expect"]
            .iter()
            .any(|key| args.get(key).is_some())
        {
            return ToolOutput::err(
                "acceptance cannot be combined with targets, repro, command or expect",
            );
        }
        let parsed = super::verify_tool::requested_repro(&json!({"repro":args["acceptance"]}));
        let mut check = match parsed {
            Some(Ok(check)) => check,
            Some(Err(error)) => return ToolOutput::err(error.replace("repro", "acceptance")),
            None => return ToolOutput::err("acceptance requires name and command"),
        };
        check.name = check.name.replacen("repro: ", "acceptance: ", 1);
        if self
            .config
            .verification
            .commands
            .iter()
            .any(|prior| prior.command == check.command)
            || self
                .step_checks
                .evidence
                .repros()
                .iter()
                .any(|prior| prior.command == check.command)
        {
            return ToolOutput::err(
                "command already belongs to a configured check or reproduction; run it with targets",
            );
        }
        if let Some(index) = self
            .step_checks
            .registered_acceptance
            .iter()
            .position(|prior| prior.name == check.name)
        {
            let old = &self.step_checks.registered_acceptance[index];
            if old.command != check.command {
                self.step_checks
                    .failed_checks
                    .retain(|failed| failed.command != old.command);
                self.step_checks.failed = !self.step_checks.failed_checks.is_empty();
            }
            self.step_checks.registered_acceptance[index] = check.clone();
        } else {
            if self.step_checks.registered_acceptance.len() >= 4 {
                return ToolOutput::err(
                    "at most four acceptance helpers can be registered per turn",
                );
            }
            self.step_checks.registered_acceptance.push(check.clone());
        }
        let output = self.run_registered_checks(std::slice::from_ref(&check), ui);
        self.step_checks.registered_checked = Some(self.step_checks.revision());
        output
    }

    fn run_registered_checks(&mut self, checks: &[FocusedCheck], ui: &mut dyn Ui) -> ToolOutput {
        let mut results = Vec::new();
        let mut failures = Vec::new();
        for check in checks {
            let output = self.run_focused_check(check, ui);
            let status = self.step_checks.evidence.latest_status(&check.command);
            self.step_checks
                .failed_checks
                .retain(|prior| prior.command != check.command);
            if status != Some(super::verification::CheckStatus::Passed) {
                failures.push(format!(
                    "{}: {}",
                    check.name,
                    crate::agent::safety::guard::clip_evidence(&output.text, 2048)
                ));
                self.step_checks.failed_checks.push(check.clone());
            }
            results.push(json!({"name":check.name,"command":check.command,"origin":"model_registered",
                "status":status,"ran":output.process_outcome().is_some_and(|process| process.started),
                "reused":false,"process":output.execution.process}));
        }
        self.step_checks.failed = !self.step_checks.failed_checks.is_empty();
        ToolOutput {
            text: if failures.is_empty() {
                "Registered helper checks passed on this revision. Reuse this result until inputs or acceptance criteria change; further tests need a concrete uncovered case. This is not proof of task-supplied acceptance.".into()
            } else {
                format!(
                    "Registered helper checks failed:\n{}\nIf the helper contradicts earlier passing evidence, minimize the helper and artifact before rewriting the implementation.",
                    failures.join("\n")
                )
            },
            data: Some(json!({"results":results,"origin":"model_registered"})),
            is_error: !failures.is_empty(),
            execution: Default::default(),
        }
    }

    /// Once after a settled mutation batch, not once per write or model round. Checks execute
    /// through normal approval/cancellation; their own effects are included before stamping.
    pub(in crate::agent) fn auto_registered_checks(
        &mut self,
        ui: &mut dyn Ui,
    ) -> Option<ToolOutput> {
        if !self.config.verification.auto_registered_checks
            || self.plan_mode
            || self.step_checks.registered_acceptance.is_empty()
            || self.step_checks.registered_checked == Some(self.step_checks.revision())
            || self.interrupt.load(Ordering::SeqCst)
        {
            return None;
        }
        let checks = self.step_checks.registered_acceptance.clone();
        let output = self.run_registered_checks(&checks, ui);
        self.step_checks.registered_checked = Some(self.step_checks.revision());
        Some(output)
    }
}

#[cfg(test)]
mod tests;
