//! Read-only discovery of the existing verification contract, never evidence of a run.
use super::*;

const MAX_CHECKS: usize = 64;
const MAX_TEXT_ITEMS: usize = 16;

impl Agent<'_> {
    pub(super) fn verification_plan(&self) -> ToolOutput {
        let revision = self.step_checks.revision();
        let mut focused = self.current_focused_checks();
        for failed in &self.step_checks.failed_checks {
            if !crate::agent::verify::verification::obsolete_check(self.workspace.root(), failed)
                && !focused.iter().any(|check| check.command == failed.command)
            {
                focused.push(failed.clone());
            }
        }
        let configured = &self.config.verification;
        let coverage = crate::agent::verify::requirements::snapshot(
            configured,
            &self.step_checks.evidence,
            revision,
            &self.step_checks.visual,
        );
        // Include configured checks even before any changed path selects their extension filter.
        // Keep name/command pairs distinct: ambiguous configuration is not a valid mapping.
        let mut checks: Vec<_> = configured
            .commands
            .iter()
            .map(|check| {
                (
                    check.name.as_str(),
                    check.command.as_str(),
                    check.timeout_secs,
                )
            })
            .collect();
        for check in &focused {
            if !checks
                .iter()
                .any(|(name, command, _)| *name == check.name && *command == check.command)
            {
                checks.push((&check.name, &check.command, check.timeout_secs));
            }
        }
        let rows: Vec<_> = checks.iter().take(MAX_CHECKS).map(|(name, command, timeout)| {
            let requirements: Vec<_> = configured.requirements.iter()
                .filter(|(_, requirement)| requirement.checks.iter().any(|check| check == name))
                .map(|(id, _)| id.as_str()).collect();
            json!({"name":name,"command":command,"timeout_secs":timeout,
                "selected":focused.iter().any(|check| check.name == *name && check.command == *command),
                "recorded_status":self.step_checks.evidence.current_status(command, revision),
                "requirements":requirements})
        }).collect();
        let mut text = vec![
            "Verification plan: nothing ran; no configuration or evidence was changed.".to_owned(),
            "Recorded statuses are effect-revision observations, not revalidated input freshness or new verification credit.".to_owned(),
        ];
        for row in rows.iter().take(MAX_TEXT_ITEMS) {
            let name = row["name"].as_str().unwrap_or_default();
            let status = row["recorded_status"].as_str().unwrap_or_default();
            text.push(format!(
                "Check {}: {status}; {}",
                plan_label(name),
                if row["selected"] == true {
                    "in focused selection"
                } else {
                    "configured, not currently selected"
                }
            ));
        }
        if checks.len() > MAX_TEXT_ITEMS {
            text.push(format!(
                "{} additional check(s); structured checks show at most {MAX_CHECKS}.",
                checks.len() - MAX_TEXT_ITEMS
            ));
        }
        if let Some(requirements) = coverage["requirements"].as_array() {
            for requirement in requirements.iter().take(MAX_TEXT_ITEMS) {
                let id = requirement["id"].as_str().unwrap_or_default();
                let names = requirement["checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|check| check["name"].as_str())
                    .map(plan_label)
                    .collect::<Vec<_>>()
                    .join(", ");
                let deliverables = requirement["deliverables"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(MAX_TEXT_ITEMS)
                    .map(plan_label)
                    .collect::<Vec<_>>()
                    .join(", ");
                let omitted = requirement["deliverables"]
                    .as_array()
                    .map_or(0, |ids| ids.len().saturating_sub(MAX_TEXT_ITEMS));
                let suffix = if omitted > 0 {
                    format!("; {omitted} more deliverable(s) in structured requirements")
                } else {
                    String::new()
                };
                text.push(format!(
                    "Requirement {} -> checks [{}]; deliverables [{}]; visual {}{}; {}{}",
                    plan_label(id),
                    names,
                    deliverables,
                    requirement["visual"].as_str().unwrap_or_default(),
                    requirement["visual_target"]
                        .as_str()
                        .map(|target| format!(" ({})", plan_label(target)))
                        .unwrap_or_default(),
                    requirement["coverage"].as_str().unwrap_or_default(),
                    suffix
                ));
            }
            if requirements.len() > MAX_TEXT_ITEMS {
                text.push(format!(
                    "{} more requirement(s) in structured requirements.",
                    requirements.len() - MAX_TEXT_ITEMS
                ));
            }
        }
        if configured.requirements.is_empty() {
            text.push("No operator-declared requirements: task coverage is not independently assessed. To establish a mapping, configure named verification.commands and verification.requirements.<id>.checks; optional deliverables bind working-state IDs. Do not invent checks or silently edit operator configuration.".to_owned());
        }
        text.push("To execute, call verify with targets naming a check (or a test file); omit arguments after edits for the focused gate selection. Normal approvals still apply.".to_owned());
        ToolOutput {
            text: text.join("\n"),
            is_error: false,
            execution: Default::default(),
            data: Some(json!({
            "mode":"plan", "ran":false, "revision":revision,
            "evidence_basis":"recorded_effect_revision_not_revalidated",
            "checks":rows,"check_count":checks.len(),
            "omitted_checks":checks.len().saturating_sub(MAX_CHECKS),
            "requirements":coverage,
            })),
        }
    }
}

fn plan_label(text: &str) -> String {
    crate::agent::safety::guard::clip_evidence(text, 120)
}
