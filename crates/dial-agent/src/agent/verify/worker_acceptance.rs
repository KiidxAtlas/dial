//! Controller-owned worker acceptance. Worker answers and artifact references are never evidence.
//! Receipts cannot be deserialized into authority; input identities live only in the controller.
use super::{inputs, verification};
use crate::{
    agent::{
        workers::subagents::{Approver, Env},
        workflow::solve,
    },
    config::types::{VerificationConfig, VerificationInputs},
    llm::ToolCall,
    runtime::tool_output::ToolOutput,
    tools::Workspace,
};
use serde::Serialize;
use serde_json::json;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::agent) enum State {
    Accepted,
    #[default]
    NeedsVerification,
    Rejected,
}

#[derive(Clone, Debug, Serialize)]
struct Observation {
    check: String,
    verdict: verification::CheckStatus,
    #[serde(skip)]
    identity: String,
    #[serde(skip)]
    inputs: VerificationInputs,
    #[serde(skip)]
    snapshot: inputs::Snapshot,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(in crate::agent) struct Acceptance {
    pub state: State,
    pub reason: String,
    checks: Vec<Observation>,
}

impl Acceptance {
    pub fn unverified(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ..Self::default()
        }
    }

    pub fn reject(&mut self, reason: impl Into<String>) {
        self.state = State::Rejected;
        self.reason = reason.into();
    }

    /// Invalidity is sticky. Restoring old bytes does not silently reinstate acceptance.
    pub fn revalidate(&mut self, workspace: &Workspace, config: &VerificationConfig) {
        if self.state != State::Accepted {
            return;
        }
        if self.checks.is_empty()
            || self.checks.iter().any(|check| {
                identity(config, &check.check).as_ref() != Some(&check.identity)
                    || inputs::capture(workspace, &check.inputs).as_ref() != Some(&check.snapshot)
            })
        {
            self.state = State::NeedsVerification;
            self.reason =
                "configured check or declared inputs changed or could not be revalidated".into();
        }
    }
}

/// Check IDs resolve only through operator configuration, never through model-authored commands.
pub(in crate::agent) fn validate_contract(
    ids: &[String],
    config: &VerificationConfig,
) -> Result<(), String> {
    if ids.is_empty() || ids.len() > 16 {
        return Err("acceptance_checks needs 1 to 16 configured check names".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id)
            || config
                .commands
                .iter()
                .filter(|check| &check.name == id)
                .count()
                != 1
        {
            return Err(format!(
                "acceptance check {id:?} must identify one unique configured check"
            ));
        }
    }
    Ok(())
}

fn identity(config: &VerificationConfig, id: &str) -> Option<String> {
    let mut matching = config.commands.iter().filter(|check| check.name == id);
    let command = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    Some(json!([command, config.inputs.get(id)?, config.runners.get(id)]).to_string())
}

/// The existing isolated runner and classifier retain their process/completeness semantics.
/// The worker's configured input contract asserts this check is deterministic and side-effect-free.
pub(in crate::agent) fn evaluate(
    env: Env<'_>,
    ids: &[String],
    approver: &mut dyn Approver,
) -> Acceptance {
    evaluate_with(
        env.workspace,
        &env.config.verification,
        ids,
        |check| {
            let call = ToolCall {
                id: format!("worker-acceptance:{}", check.name),
                name: "bash".into(),
                arguments: json!({"command":check.command,"timeout":check.timeout_secs}),
                malformed_arguments: false,
            };
            let title = format!("Worker acceptance: {}", check.name);
            if env.plan_mode {
                return ToolOutput::err("worker acceptance check refused in plan mode")
                    .with_status(crate::tools::ToolStatus::Blocked);
            }
            if let Err(denial) = approver.authorize_with_cancel(&call, &title, true, env.interrupt)
            {
                return crate::agent::safety::approval::denied_check_output(&check.name, &denial);
            }
            approver.notice(&title);
            solve::run_isolated(
                env.workspace,
                &check.command,
                check.timeout_secs,
                env.interrupt,
                &mut crate::agent::NullUi,
            )
            .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")))
        },
        || env.interrupt.load(Ordering::SeqCst),
    )
}

fn evaluate_with(
    workspace: &Workspace,
    config: &VerificationConfig,
    ids: &[String],
    mut run: impl FnMut(&verification::FocusedCheck) -> ToolOutput,
    cancelled: impl Fn() -> bool,
) -> Acceptance {
    if let Err(reason) = validate_contract(ids, config) {
        return Acceptance::unverified(reason);
    }
    let mut receipt = Acceptance::unverified("configured acceptance checks are incomplete");
    for id in ids {
        let Some(identity) = identity(config, id) else {
            return Acceptance::unverified(format!(
                "check {id:?} has no complete declared input contract"
            ));
        };
        let contract = &config.inputs[id];
        let Some(before) = inputs::capture(workspace, contract) else {
            return Acceptance::unverified(format!("could not capture inputs for {id:?}"));
        };
        let custom = config
            .commands
            .iter()
            .find(|check| check.name == *id)
            .expect("validated check ID");
        let check = verification::FocusedCheck {
            name: id.clone(),
            command: custom.command.clone(),
            timeout_secs: custom.timeout_secs,
        };
        let output = run(&check);
        let runner = config
            .runners
            .get(id)
            .map(|runner| (runner.program.as_str(), runner.module.as_deref()));
        let verdict = verification::classify_with_artifacts(
            Some(&check),
            runner,
            &output,
            cancelled(),
            workspace.artifacts(),
        );
        if inputs::capture(workspace, contract).as_ref() != Some(&before) {
            return Acceptance::unverified(format!(
                "inputs changed during acceptance check {id:?}"
            ));
        }
        receipt.checks.push(Observation {
            check: id.clone(),
            verdict,
            identity,
            inputs: contract.clone(),
            snapshot: before,
        });
        if verdict != verification::CheckStatus::Passed {
            if verdict == verification::CheckStatus::CodeFailed {
                receipt.reject(format!("configured acceptance check {id:?} failed"));
            } else {
                receipt.reason =
                    format!("configured acceptance check {id:?} did not verify the candidate");
            }
            return receipt;
        }
    }
    receipt.state = State::Accepted;
    receipt.reason = "all contracted checks passed on stable declared inputs; acceptance is scoped to these checks".into();
    receipt.revalidate(workspace, config);
    receipt
}

#[cfg(test)]
mod tests;
