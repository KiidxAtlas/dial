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
        let pixels = !requirement.visual || match requirement.visual_target.as_deref() {
            Some(target) => visual.rendered_target_observed(revision, target),
            None => visual.rendered_observed(revision),
        };
        let has_evidence = requirement.visual || !checks.is_empty();
        let observed = has_evidence && pixels && checks.iter().all(|check| check["status"] == "passed");
        covered += usize::from(observed);
        json!({"id":id,"coverage":if observed {"observed"} else {"uncovered"},
            "checks":checks,"deliverables":requirement.deliverables,
            "visual_target":requirement.visual_target,
            "visual":if !requirement.visual {"not_required"} else if pixels {"observed"} else {"missing"}})
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

/// Project operator-owned acceptance separately from the model's claimed status and citations.
/// No mapping is explicitly unassessed, rather than silently inventing a machine-checkable task.
pub(in crate::agent) fn deliverable_acceptance(coverage: &Value, id: &str) -> Value {
    let contracts: Vec<&Value> = coverage["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|requirement| {
            requirement["deliverables"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|candidate| candidate.as_str() == Some(id)))
        })
        .collect();
    let status = if contracts.is_empty() {
        "unassessed"
    } else if contracts
        .iter()
        .all(|requirement| requirement["coverage"] == "observed")
    {
        "observed"
    } else {
        "needs_verification"
    };
    json!({"status":status,"requirements":contracts,
        "authority":"operator_contract_controller_observations_not_model_claims"})
}

/// Claimed satisfaction cannot bypass a mapped contract by citing another check or arbitrary text.
/// Confirmed cancellation/removal is handled by the obligation owner, not by this evidence gate.
pub(in crate::agent) fn deliverable_blocker(coverage: &Value, working: &Value) -> Option<String> {
    let missing: Vec<String> = working["deliverables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["status"] == "satisfied")
        .filter_map(|item| item["id"].as_str())
        .filter(|id| deliverable_acceptance(coverage, id)["status"] == "needs_verification")
        .map(str::to_owned)
        .collect();
    (!missing.is_empty()).then(|| format!(
        "Deliverable acceptance evidence is missing, failed or stale for: {}. Observe the operator-declared requirements or report incomplete work; citations and claimed satisfaction do not replace the contract.",
        missing.join(", ")
    ))
}

#[cfg(test)]
mod tests;
