//! Measured, model-bound workflow candidates, never heuristic runtime routing.
//!
//! Recorded runs select hypotheses only. The existing sealed correctness gate and drift guard
//! remain mandatory; paired action/protocol/chatter measurements can reject, never admit, a
//! candidate. Only aggregate measurements are persisted, not held-out prompts or outputs.

use super::{mutate, probe};
use crate::config::types::{Admission, EditMode, Harness, Version};
use crate::storage::store::Store;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metrics {
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub first_useful_ms: Option<u64>,
    #[serde(default)]
    pub repeated_calls: u64,
    #[serde(default)]
    pub tool_errors: u64,
    #[serde(default)]
    pub interventions: u64,
    #[serde(default)]
    pub architecture_tokens: u64,
    #[serde(default)]
    pub verification_failures: u64,
    #[serde(default)]
    pub repair_rounds: u64,
    #[serde(default)]
    pub change_reviews: u64,
    pub generations: u64,
    pub tool_calls: u64,
    pub investigation_calls: u64,
    pub protocol_errors: u64,
    /// Visible assistant bytes accompanying tool calls, not hidden reasoning or final answers.
    pub chatter_bytes: u64,
    /// Provider-reported output tokens; zero when the provider does not report usage.
    pub completion_tokens: u64,
}

impl Metrics {
    pub fn observe(&mut self, kind: &str, data: &Value) {
        self.elapsed_ms = self.elapsed_ms.max(data["wall_ms"].as_u64().unwrap_or(0));
        match kind {
            "architecture_synthesis" => {
                self.architecture_tokens = self
                    .architecture_tokens
                    .saturating_add(data["completion_tokens"].as_u64().unwrap_or(0))
            }
            "repair_round" => self.repair_rounds += 1,
            "change_review" => self.change_reviews += 1,
            "verification" if data["passed"] == false => self.verification_failures += 1,
            "generation" => {
                self.generations += 1;
                self.completion_tokens = self
                    .completion_tokens
                    .saturating_add(data["completion_tokens"].as_u64().unwrap_or(0));
                if data["tool_calls"].as_u64().unwrap_or(0) > 0 {
                    self.chatter_bytes = self.chatter_bytes.saturating_add(
                        data["output"].as_str().map_or(0, |text| text.len() as u64),
                    );
                }
            }
            "tool_call" => {
                self.tool_calls += 1;
                if matches!(
                    data["name"].as_str(),
                    Some("read" | "find" | "grep" | "glob" | "lsp" | "ast_search")
                ) {
                    self.investigation_calls += 1;
                }
            }
            "tool_result"
                if self.first_useful_ms.is_none()
                    && !matches!(data["name"].as_str(), Some("todo" | "task_list")) =>
            {
                self.first_useful_ms = data["wall_ms"].as_u64();
            }
            "tool_error" => self.tool_errors += 1,
            "investigation_gate" => {
                self.repeated_calls += 1;
                self.interventions += 1;
            }
            "reasoning_gate" => self.interventions += 1,
            "tool_form_failure" => self.protocol_errors += 1,
            _ => {}
        }
    }

    pub(super) fn add(&mut self, other: &Self) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(other.elapsed_ms);
        self.first_useful_ms = match (self.first_useful_ms, other.first_useful_ms) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (None, b) => b,
            (a, None) => a,
        };
        self.repeated_calls += other.repeated_calls;
        self.tool_errors += other.tool_errors;
        self.interventions += other.interventions;
        self.architecture_tokens = self
            .architecture_tokens
            .saturating_add(other.architecture_tokens);
        self.verification_failures += other.verification_failures;
        self.repair_rounds += other.repair_rounds;
        self.change_reviews += other.change_reviews;
        self.generations = self.generations.saturating_add(other.generations);
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
        self.investigation_calls = self
            .investigation_calls
            .saturating_add(other.investigation_calls);
        self.protocol_errors = self.protocol_errors.saturating_add(other.protocol_errors);
        self.chatter_bytes = self.chatter_bytes.saturating_add(other.chatter_bytes);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaseMeasurement {
    pub passed: bool,
    pub metrics: Metrics,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Evidence {
    pub source_version: i64,
    pub trajectories: usize,
    pub protocol_trajectories: usize,
    pub investigation_trajectories: usize,
    pub failure_kinds: BTreeMap<String, usize>,
    /// Sampled counts: Store::recent returns at most the last 128 events of each run.
    pub observed: Metrics,
}

/// Active-version observations among the latest 200 finished runs, each bounded to 128 events.
/// These sampled counts select hypotheses only; sealed comparisons use complete-run metrics.
pub fn collect(store: &Store, active: &Version) -> Result<Evidence> {
    Ok(probe::workflow_evidence(&store.recent(200)?, active.id))
}

pub struct Candidate {
    pub name: &'static str,
    pub rationale: String,
    pub harness: Harness,
    pub evidence: Evidence,
}

const PROTOCOL_NOTE: &str = "Tool protocol workflow: issue one tool call at a time through the tool interface, with one valid JSON arguments object using only the declared parameter names and types. Read the result before choosing the next call. Do not print tool calls in prose. Keep commentary before calls to one short sentence; preserve the requested final answer and verification.";
const INVESTIGATION_NOTE: &str = "Bounded investigation workflow: locate the requested symbols once, read the relevant implementation and nearest check, then make the smallest justified change. Reuse evidence already obtained instead of repeating searches or whole-file reads; investigate again only for a concrete unanswered question. Keep intermediate commentary brief. Run the relevant checks before claiming completion, and report unresolved failures honestly.";

/// Recurrence thresholds identify hypotheses, not improvements. Every returned harness still
/// needs the unchanged correctness admission gate plus paired workflow measurements.
pub fn candidates(
    active: &Harness,
    ctx: &mutate::Context,
    evidence: &Evidence,
    limit: usize,
) -> Vec<Candidate> {
    let mut options = Vec::new();
    if evidence.protocol_trajectories >= 3 {
        let mut harness = active.clone();
        mutate::push_note(&mut harness, PROTOCOL_NOTE);
        harness.temperature = 0.0;
        options.push(Candidate {
            name: "protocol",
            rationale: format!("{} protocol failures across {}/{} active-version trajectories: test serial schema-disciplined calls at temperature zero", evidence.observed.protocol_errors, evidence.protocol_trajectories, evidence.trajectories),
            harness,
            evidence: evidence.clone(),
        });
        if evidence.failure_kinds.get("schema").copied().unwrap_or(0)
            + evidence
                .failure_kinds
                .get("malformed_arguments")
                .copied()
                .unwrap_or(0)
            >= 3
        {
            let mut harness = active.clone();
            mutate::push_note(&mut harness, PROTOCOL_NOTE);
            harness.edit_mode = Some(match active.edit_mode.unwrap_or(ctx.default_edit_mode) {
                EditMode::Hashline => EditMode::Replace,
                _ => EditMode::Hashline,
            });
            options.push(Candidate {
                name: "protocol_edit_format",
                rationale: format!("schema/argument errors recur in {} trajectories: test the alternate edit protocol, not an assumed model capability", evidence.protocol_trajectories),
                harness,
                evidence: evidence.clone(),
            });
        }
    }
    if evidence.investigation_trajectories >= 3 {
        let mut harness = active.clone();
        mutate::push_note(&mut harness, INVESTIGATION_NOTE);
        if active.max_steps != 0 {
            harness.max_steps = (active.max_steps.saturating_mul(3) / 4).clamp(8, 64);
        }
        options.push(Candidate {
            name: "bounded_investigation",
            rationale: format!("{}/{} active-version trajectories made at least eight investigation calls and twice as many investigation as other calls: test focused evidence reuse with max_steps {} -> {}; correctness must not regress", evidence.investigation_trajectories, evidence.trajectories, active.max_steps, harness.max_steps),
            harness,
            evidence: evidence.clone(),
        });
    }
    if evidence.observed.tool_errors >= 3 && evidence.trajectories >= 3 {
        let mut harness = active.clone();
        mutate::push_note(
            &mut harness,
            "Failure recovery workflow: after a failed operation, state what the diagnostic rules out, inspect the smallest relevant region, and change one justified part of the approach. Never repeat the same failed action unchanged. Preserve the original tests and acceptance criteria; a passing edited test is not proof of a fix.",
        );
        options.push(Candidate { name:"recovery", rationale:"Recurring tool failures: evaluate evidence-driven recovery against the unchanged sealed checks".into(), harness, evidence:evidence.clone() });
    }
    let mut seen = HashSet::from([mutate::harness_digest(active)]);
    options
        .into_iter()
        .filter(|candidate| {
            let digest = mutate::harness_digest(&candidate.harness);
            !ctx.tried.contains(&digest) && (ctx.valid)(&candidate.harness) && seen.insert(digest)
        })
        .take(limit)
        .collect()
}

pub fn is_candidate(version: &Version) -> bool {
    version.kind.starts_with("workflow:")
}

/// The Store is already bound to one model/use-case. Keep the binding explicit in audit metadata.
pub fn register(store: &Store, version: &Version, candidate: &Candidate) -> Result<()> {
    ensure!(
        version.kind == format!("workflow:{}", candidate.name),
        "workflow candidate kind mismatch"
    );
    ensure!(
        version.harness == candidate.harness,
        "workflow candidate harness mismatch"
    );
    ensure!(
        version.parent == Some(candidate.evidence.source_version),
        "workflow evidence version mismatch"
    );
    store.set_meta(&format!("workflow_proposal:{}", version.id), &serde_json::to_string(&serde_json::json!({
        "model": store.get_meta("model")?, "use_case": store.get_meta("use_case")?,
        "name": candidate.name, "rationale": candidate.rationale, "evidence": candidate.evidence,
        "harness_digest": mutate::harness_digest(&candidate.harness), "status": "unverified_hypothesis"
    }))?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub baseline_version: i64,
    pub candidate_version: i64,
    pub cases: usize,
    pub baseline_passed: usize,
    pub candidate_passed: usize,
    pub newly_failed: usize,
    pub baseline: Metrics,
    pub candidate: Metrics,
    pub measured_reduction: bool,
    pub admitted: bool,
    pub reason: String,
}

/// Additional fail-closed requirement; this function can never turn a gate rejection into a pass.
/// Pairs must be the exact cached baseline/candidate runs used by the sealed correctness gate.
pub fn compare(
    kind: &str,
    admission: &mut Admission,
    pairs: &[(CaseMeasurement, CaseMeasurement)],
) -> Comparison {
    let mut report = Comparison {
        baseline_version: admission.baseline,
        candidate_version: admission.candidate,
        cases: pairs.len(),
        baseline_passed: 0,
        candidate_passed: 0,
        newly_failed: 0,
        baseline: Metrics::default(),
        candidate: Metrics::default(),
        measured_reduction: false,
        admitted: false,
        reason: String::new(),
    };
    for (baseline, candidate) in pairs {
        report.baseline_passed += usize::from(baseline.passed);
        report.candidate_passed += usize::from(candidate.passed);
        report.newly_failed += usize::from(baseline.passed && !candidate.passed);
        report.baseline.add(&baseline.metrics);
        report.candidate.add(&candidate.metrics);
    }
    let b = &report.baseline;
    let c = &report.candidate;
    report.measured_reduction = match kind {
        "workflow:protocol" | "workflow:protocol_edit_format" => {
            c.protocol_errors < b.protocol_errors
        }
        "workflow:recovery" => c.tool_errors < b.tool_errors || c.repeated_calls < b.repeated_calls,
        "workflow:bounded_investigation" => {
            c.tool_calls < b.tool_calls
                || c.investigation_calls < b.investigation_calls
                || c.chatter_bytes < b.chatter_bytes
        }
        _ => false,
    };
    let expected: usize = admission.scores.iter().map(|s| s.cases).sum();
    let failure = if pairs.is_empty() || pairs.len() != expected {
        Some("workflow comparison missing paired held-out measurements")
    } else if report.newly_failed > 0 {
        Some("workflow comparison lost previously solved held-out cases")
    } else if c.tool_errors > b.tool_errors
        || c.repeated_calls > b.repeated_calls
        || c.protocol_errors > b.protocol_errors
        || c.tool_calls > b.tool_calls
        || c.chatter_bytes > b.chatter_bytes
        || c.generations > b.generations
    {
        Some(
            "workflow comparison increased protocol errors, actions, chatter, or generation rounds",
        )
    } else if !report.measured_reduction {
        Some("workflow comparison found no measured reduction in the targeted behavior")
    } else {
        None
    };
    if admission.passed
        && let Some(reason) = failure
    {
        admission.passed = false;
        admission.reason = reason.into();
    }
    report.admitted = admission.passed;
    report.reason = admission.reason.clone();
    report
}

/// Persist aggregate comparisons for accepted and rejected profiles. The admitted harness itself
/// lives in normal version storage; no second active pointer can bypass rollback or model binding.
pub fn record_comparison(
    store: &Store,
    baseline: &Version,
    candidate: &Version,
    admission: &mut Admission,
    pairs: &[(CaseMeasurement, CaseMeasurement)],
) -> Result<()> {
    if !is_candidate(candidate) {
        return Ok(());
    }
    ensure!(
        admission.baseline == baseline.id && admission.candidate == candidate.id,
        "workflow comparison version mismatch"
    );
    let proposal = store
        .get_meta(&format!("workflow_proposal:{}", candidate.id))?
        .context("workflow proposal evidence missing")?;
    let _: Value = serde_json::from_str(&proposal).context("corrupt workflow proposal evidence")?;
    let comparison = compare(&candidate.kind, admission, pairs);
    store.set_meta(
        &format!("workflow_comparison:{}", candidate.id),
        &serde_json::to_string(&comparison)?,
    )
}

#[cfg(test)]
mod tests;
