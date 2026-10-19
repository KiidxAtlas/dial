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

#[cfg(test)]
mod tests;
