//! Declared requirement coverage: observed current evidence, never a claim of full correctness.
use super::{
    evidence::{Ledger, Revision},
    visual::Review,
};
use crate::config::types::VerificationConfig;
use serde_json::{Value, json};

pub(in crate::agent) fn snapshot(
    config: &VerificationConfig,
    ledger: &Ledger,
    revision: Revision,
    visual: &Review,
) -> Value {
    let mut covered = 0;
    let requirements: Vec<_> = config.requirements.iter().map(|(id, requirement)| {
        let checks: Vec<_> = requirement.checks.iter().map(|name| {
            let matching: Vec<_> = config.commands.iter().filter(|check| &check.name == name).collect();
            // Validation normally enforces this; callers constructing Config directly also fail closed.
            let status = match matching.as_slice() {
                [check] => ledger.current_status(&check.command, revision),
                _ => "invalid_contract",
            };
            json!({"name":name,"status":status})
        }).collect();
        let pixels = !requirement.visual || visual.rendered_observed(revision);
        let has_evidence = requirement.visual || !checks.is_empty();
        let observed = has_evidence && pixels && checks.iter().all(|check| check["status"] == "passed");
        covered += usize::from(observed);
        json!({"id":id,"coverage":if observed {"observed"} else {"uncovered"},
            "checks":checks,"visual":if !requirement.visual {"not_required"} else if pixels {"observed"} else {"missing"}})
    }).collect();
    json!({
        "coverage":if requirements.is_empty() {"not_independently_assessed"}
            else if covered == requirements.len() {"declared_requirements_observed"} else {"requirements_uncovered"},
        "declared":requirements.len(),"observed":covered,"revision":revision,"requirements":requirements,
        "authority":"Operator-declared evidence mapping. Does not establish that the contract covers the entire user request or that the checks are sufficient."
    })
}

/// Strict mode checks only the operator's declared contract. No model-generated requirement
/// or evaluator opinion can create a new completion obligation.
pub(in crate::agent) fn strict_blocker(
    config: &VerificationConfig,
    ledger: &Ledger,
    revision: Revision,
    visual: &Review,
) -> Option<String> {
    if !config.strict_requirements || config.requirements.is_empty() {
        return None;
    }
    let coverage = snapshot(config, ledger, revision, visual);
    let missing = coverage["requirements"]
        .as_array()?
        .iter()
        .filter(|requirement| requirement["coverage"] != "observed")
        .filter_map(|requirement| requirement["id"].as_str())
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| format!(
        "Declared outcome evidence is missing, failed or stale for: {}. Run the configured checks and obtain required rendered pixels at the current edits, or report the concrete blocker/incomplete outcome. This contract does not grant execution permission.", missing.join(", ")
    ))
}

#[cfg(test)]
mod tests;
