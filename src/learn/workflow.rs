//! Measured, model-bound workflow candidates, never heuristic runtime routing.
//!
//! Recorded runs select hypotheses only. The existing sealed correctness gate and drift guard
//! remain mandatory; paired action/protocol/chatter measurements can reject, never admit, a
//! candidate. Only aggregate measurements are persisted, not held-out prompts or outputs.

use super::{mutate, probe};
use crate::config::types::{Admission, EditModeConfig, Harness, Version};
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
        self.architecture_tokens += other.architecture_tokens;
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
                EditModeConfig::Hashline => EditModeConfig::Replace,
                EditModeConfig::Replace => EditModeConfig::Hashline,
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
        harness.max_steps = (active.max_steps.saturating_mul(3) / 4).clamp(8, 64);
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
    if admission.passed {
        if let Some(reason) = failure {
            admission.passed = false;
            admission.reason = reason.into();
        }
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
mod tests {
    use super::*;
    use crate::config::types::SuiteScore;
    use serde_json::json;

    #[test]
    fn metrics_count_actual_calls_and_chatter_not_answers_or_results() {
        let mut metrics = Metrics::default();
        metrics.observe(
            "generation",
            &json!({"tool_calls":1,"output":"look","completion_tokens":7}),
        );
        metrics.observe("tool_call", &json!({"name":"read"}));
        metrics.observe("tool_result", &json!({"output":"long tool output"}));
        metrics.observe(
            "generation",
            &json!({"tool_calls":0,"output":"the requested final answer","completion_tokens":9}),
        );
        metrics.observe("tool_form_failure", &json!({"kind":"schema"}));
        assert_eq!(
            metrics,
            Metrics {
                generations: 2,
                tool_calls: 1,
                investigation_calls: 1,
                protocol_errors: 1,
                chatter_bytes: 4,
                completion_tokens: 16,
                ..Metrics::default()
            }
        );
    }

    #[test]
    fn maintenance_and_repair_costs_do_not_count_as_task_success() {
        let mut metrics = Metrics::default();
        metrics.observe(
            "architecture_synthesis",
            &json!({"completion_tokens":120,"wall_ms":2000}),
        );
        metrics.observe("verification", &json!({"passed":false}));
        metrics.observe("repair_round", &json!({"attempt":1}));
        metrics.observe("change_review", &json!({"path":"main.rs"}));
        let mut combined = Metrics::default();
        combined.add(&metrics);
        assert_eq!(combined.architecture_tokens, 120);
        assert_eq!(combined.elapsed_ms, 2000);
        assert_eq!(
            (
                combined.verification_failures,
                combined.repair_rounds,
                combined.change_reviews
            ),
            (1, 1, 1)
        );
        assert_eq!(combined.generations, 0);
    }

    #[test]
    fn timing_and_repetition_are_observed_not_inferred_from_token_counts() {
        let mut metrics = Metrics::default();
        metrics.observe("generation", &json!({"wall_ms":40,"tool_calls":1}));
        metrics.observe("tool_error", &json!({"name":"read","wall_ms":50}));
        assert_eq!(metrics.first_useful_ms, None);
        metrics.observe("tool_result", &json!({"name":"todo","wall_ms":55}));
        assert_eq!(metrics.first_useful_ms, None);
        metrics.observe("tool_result", &json!({"name":"read","wall_ms":70}));
        metrics.observe("investigation_gate", &json!({"wall_ms":90}));
        assert_eq!(metrics.first_useful_ms, Some(70));
        assert_eq!(metrics.elapsed_ms, 90);
        assert_eq!((metrics.tool_errors, metrics.repeated_calls), (1, 1));
    }

    fn evidence() -> Evidence {
        Evidence {
            source_version: 1,
            trajectories: 6,
            protocol_trajectories: 3,
            investigation_trajectories: 3,
            failure_kinds: BTreeMap::from([("schema".into(), 3)]),
            observed: Metrics {
                protocol_errors: 3,
                ..Metrics::default()
            },
        }
    }

    #[test]
    fn recurrence_proposes_real_validated_harness_changes_not_verified_claims() {
        let harness = Harness::default();
        let failures = BTreeMap::new();
        let tried = HashSet::new();
        let valid = |_: &Harness| true;
        let ctx = mutate::Context {
            failure_kinds: &failures,
            selfplay_rates: &[],
            default_edit_mode: EditModeConfig::Hashline,
            tried: &tried,
            valid: &valid,
        };
        let options = candidates(&harness, &ctx, &evidence(), 8);
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].harness.temperature, 0.0);
        assert_eq!(options[1].harness.edit_mode, Some(EditModeConfig::Replace));
        assert_eq!(options[2].harness.max_steps, 30);
        assert!(
            options
                .iter()
                .all(|c| c.harness != harness && c.harness.tools == harness.tools)
        );
        let mut one_run = evidence();
        one_run.protocol_trajectories = 1;
        one_run.investigation_trajectories = 1;
        assert!(candidates(&harness, &ctx, &one_run, 8).is_empty());
        let tried = options
            .iter()
            .map(|c| mutate::harness_digest(&c.harness))
            .collect();
        let ctx = mutate::Context {
            tried: &tried,
            ..ctx
        };
        assert!(candidates(&harness, &ctx, &evidence(), 8).is_empty());
    }

    fn admission(passed: bool, cases: usize) -> Admission {
        Admission {
            baseline: 1,
            candidate: 2,
            passed,
            reason: "sealed gate verdict".into(),
            scores: vec![SuiteScore {
                name: "private".into(),
                primary: true,
                baseline: 50.0,
                candidate: 100.0,
                cases,
                candidate_only: 1,
                baseline_only: 0,
            }],
        }
    }

    fn pair(baseline_pass: bool, candidate_pass: bool) -> (CaseMeasurement, CaseMeasurement) {
        (
            CaseMeasurement {
                passed: baseline_pass,
                metrics: Metrics {
                    tool_calls: 5,
                    investigation_calls: 4,
                    protocol_errors: 2,
                    ..Metrics::default()
                },
            },
            CaseMeasurement {
                passed: candidate_pass,
                metrics: Metrics {
                    tool_calls: 3,
                    investigation_calls: 2,
                    protocol_errors: 0,
                    ..Metrics::default()
                },
            },
        )
    }

    #[test]
    fn efficiency_can_never_override_gate_rejection_or_paired_regression() {
        let mut rejected = admission(false, 1);
        assert!(!compare("workflow:protocol", &mut rejected, &[pair(false, true)]).admitted);
        assert_eq!(rejected.reason, "sealed gate verdict");
        let mut regression = admission(true, 2);
        assert!(
            !compare(
                "workflow:bounded_investigation",
                &mut regression,
                &[pair(true, false), pair(false, true)]
            )
            .admitted
        );
        assert!(regression.reason.contains("previously solved"));
    }

    #[test]
    fn requires_complete_measurements_and_targeted_reduction_without_hidden_tradeoffs() {
        let mut passed = admission(true, 1);
        assert!(compare("workflow:protocol", &mut passed, &[pair(false, true)]).admitted);
        let mut incomplete = admission(true, 2);
        assert!(!compare("workflow:protocol", &mut incomplete, &[pair(false, true)]).admitted);
        let mut unchanged = pair(false, true);
        unchanged.1.metrics = unchanged.0.metrics.clone();
        let mut no_gain = admission(true, 1);
        assert!(!compare("workflow:protocol", &mut no_gain, &[unchanged]).admitted);
        let mut costly = pair(false, true);
        costly.1.metrics.chatter_bytes = 1;
        let mut tradeoff = admission(true, 1);
        assert!(!compare("workflow:protocol", &mut tradeoff, &[costly]).admitted);
    }

    #[test]
    fn profile_provenance_and_measurements_persist_in_model_bound_store() {
        let dir = tempfile::tempdir().unwrap();
        let harness = Harness::default();
        let store = Store::open(dir.path(), "local-model", "coding", &harness).unwrap();
        let base = store.active().unwrap();
        let candidate = Candidate {
            name: "protocol",
            rationale: "recurring failures".into(),
            harness: base.harness.clone(),
            evidence: Evidence {
                source_version: base.id,
                ..evidence()
            },
        };
        let version = store
            .candidate(base.id, "workflow:protocol", &candidate.harness, None)
            .unwrap();
        register(&store, &version, &candidate).unwrap();
        let mut verdict = admission(true, 1);
        verdict.baseline = base.id;
        verdict.candidate = version.id;
        verdict.scores[0].baseline = 0.0;
        verdict.scores.push(SuiteScore {
            name: "guard".into(),
            primary: false,
            baseline: 100.0,
            candidate: 100.0,
            cases: 1,
            candidate_only: 0,
            baseline_only: 0,
        });
        record_comparison(
            &store,
            &base,
            &version,
            &mut verdict,
            &[pair(false, true), pair(true, true)],
        )
        .unwrap();
        store.admit(&verdict).unwrap();
        drop(store);
        let store = Store::open(dir.path(), "local-model", "coding", &harness).unwrap();
        assert_eq!(store.active().unwrap().id, version.id);
        let proposal: Value = serde_json::from_str(
            &store
                .get_meta(&format!("workflow_proposal:{}", version.id))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(proposal["model"], "local-model");
        let comparison: Comparison = serde_json::from_str(
            &store
                .get_meta(&format!("workflow_comparison:{}", version.id))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(comparison.admitted && comparison.measured_reduction);
        store.rollback().unwrap();
        assert_eq!(store.active().unwrap().id, base.id);
        assert!(Store::open(dir.path(), "different-model", "coding", &harness).is_err());
    }
}
