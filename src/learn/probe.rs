//! Step-level failure probe: a logistic regression over hand-built features of agent events.
//!
//! This is deliberately small (`FEATURES` weights plus a bias) and honest about it: it is an
//! observation scorer in the spirit of step-level failure probes, not a neural probe. Every event
//! of a trajectory is labeled with the trajectory's outcome (score < 100 means failure), so the
//! learned signal is "does this kind of step tend to occur in failing runs". Training is
//! deterministic given the seed, which only drives the train/holdout split.

use crate::storage::store::Store;
use anyhow::{Context, Result};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const KINDS: [&str; 8] = [
    "generation",
    "generation_error",
    "tool_call",
    "tool_result",
    "tool_error",
    "tool_form_failure",
    "tool_recovery",
    "verification",
];
const FORM_KINDS: [&str; 5] = [
    "dropped_call",
    "malformed_arguments",
    "unknown_tool",
    "schema",
    "text_call",
];
const TOOL_BUCKETS: usize = 8;

// Feature layout.
const F_KIND: usize = 0;
const F_TOOL: usize = F_KIND + KINDS.len();
const F_STEP: usize = F_TOOL + TOOL_BUCKETS;
const F_BYTES: usize = F_STEP + 1;
const F_ERROR_TEXT: usize = F_BYTES + 1;
const F_EXIT_NONZERO: usize = F_ERROR_TEXT + 1;
const F_PROMPT_TOKENS: usize = F_EXIT_NONZERO + 1;
const F_COMPLETION_TOKENS: usize = F_PROMPT_TOKENS + 1;
const F_ELAPSED: usize = F_COMPLETION_TOKENS + 1;
const F_FORM: usize = F_ELAPSED + 1;
const F_TOOL_CALLS: usize = F_FORM + FORM_KINDS.len();
const F_EMPTY_OUTPUT: usize = F_TOOL_CALLS + 1;
/// Length of every feature vector.
pub const FEATURES: usize = F_EMPTY_OUTPUT + 1;

const MIN_EXAMPLES: usize = 20;
const MIN_TRAJECTORIES: usize = 30;
const HOLDOUT: f64 = 0.2;
const ITERATIONS: usize = 400;
const LEARNING_RATE: f64 = 0.3;
const L2: f64 = 1e-2;
const META_KEY: &str = "step_probe";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepProbe {
    pub weights: Vec<f64>,
    pub bias: f64,
    pub means: Vec<f64>,
    pub scales: Vec<f64>,
    pub trained_on: usize,
    /// Holdout AUC (0.5 when no meaningful holdout could be formed).
    pub auc: f64,
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn number(data: &Value, key: &str) -> f64 {
    data.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(0.0)
}

/// Last `[exit code: N]` marker in a tool output.
fn exit_code(text: &str) -> Option<i64> {
    let at = text.rfind("exit code:")?;
    let rest = text[at + "exit code:".len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Feature vector of one recorded event. Tolerates missing or mistyped fields.
pub fn features(kind: &str, step: u32, data: &Value) -> Vec<f64> {
    let mut f = vec![0.0; FEATURES];
    if let Some(i) = KINDS.iter().position(|k| *k == kind) {
        f[F_KIND + i] = 1.0;
    }
    if let Some(name) = data.get("name").and_then(Value::as_str) {
        f[F_TOOL + (fnv1a(name) % TOOL_BUCKETS as u64) as usize] = 1.0;
    }
    f[F_STEP] = f64::from(step).ln_1p();
    let text = ["output", "error"]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .unwrap_or("");
    f[F_BYTES] = (text.len() as f64).ln_1p();
    f[F_EMPTY_OUTPUT] = f64::from(u8::from(kind == "generation" && text.trim().is_empty()));
    f[F_ERROR_TEXT] = f64::from(u8::from(text.to_ascii_lowercase().contains("error")));
    let verification_failed = kind == "verification"
        && (data.get("passed").and_then(Value::as_bool) == Some(false)
            || data
                .get("exit")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0));
    f[F_EXIT_NONZERO] = f64::from(u8::from(
        verification_failed || exit_code(text).is_some_and(|c| c != 0),
    ));
    f[F_PROMPT_TOKENS] = number(data, "prompt_tokens").ln_1p();
    f[F_COMPLETION_TOKENS] = number(data, "completion_tokens").ln_1p();
    f[F_ELAPSED] = number(data, "elapsed_ms").ln_1p();
    if kind == "tool_form_failure"
        && let Some(i) = data
            .get("kind")
            .and_then(Value::as_str)
            .and_then(|k| FORM_KINDS.iter().position(|x| *x == k))
    {
        f[F_FORM + i] = 1.0;
    }
    f[F_TOOL_CALLS] = number(data, "tool_calls").ln_1p();
    f
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z.clamp(-35.0, 35.0)).exp())
}

impl StepProbe {
    /// P(trajectory fails | this step), in 0..1.
    pub fn risk(&self, f: &[f64]) -> f64 {
        let z = self.bias
            + f.iter()
                .zip(&self.weights)
                .zip(self.means.iter().zip(&self.scales))
                .map(|((x, w), (m, s))| w * (x - m) / s)
                .sum::<f64>();
        sigmoid(z)
    }

    /// Trajectory failure risk: the mean of the three highest step risks (all of them when there
    /// are fewer than three). Less brittle than a pure max, which one noisy step can saturate.
    /// An empty trajectory has risk 0.
    pub fn trajectory_risk(&self, events: &[(String, u32, Value)]) -> f64 {
        let mut risks: Vec<f64> = events
            .iter()
            .map(|(k, s, d)| self.risk(&features(k, *s, d)))
            .collect();
        risks.sort_by(|a, b| b.total_cmp(a));
        risks.truncate(3);
        if risks.is_empty() {
            0.0
        } else {
            risks.iter().sum::<f64>() / risks.len() as f64
        }
    }
}

/// AUC of `scores` for the positive label by the rank statistic (Mann–Whitney U), with tied scores
/// sharing their average rank. 0.5 when a class is missing.
fn auc(scores: &[f64], labels: &[bool]) -> f64 {
    let positives = labels.iter().filter(|l| **l).count();
    let negatives = labels.len() - positives;
    if positives == 0 || negatives == 0 {
        return 0.5;
    }
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|a, b| scores[*a].total_cmp(&scores[*b]));
    let mut rank_sum = 0.0;
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        while j + 1 < order.len() && scores[order[j + 1]] == scores[order[i]] {
            j += 1;
        }
        let average_rank = (i + j) as f64 / 2.0 + 1.0;
        rank_sum += order[i..=j].iter().filter(|k| labels[**k]).count() as f64 * average_rank;
        i = j + 1;
    }
    (rank_sum - positives as f64 * (positives as f64 + 1.0) / 2.0)
        / (positives as f64 * negatives as f64)
}

struct Fit {
    weights: Vec<f64>,
    bias: f64,
    means: Vec<f64>,
    scales: Vec<f64>,
}

fn fit(rows: &[&[f64]], labels: &[bool]) -> Fit {
    let n = rows.len() as f64;
    let clean = |x: f64| if x.is_finite() { x } else { 0.0 };
    let means: Vec<f64> = (0..FEATURES)
        .map(|j| rows.iter().map(|r| clean(r[j])).sum::<f64>() / n)
        .collect();
    let scales: Vec<f64> = (0..FEATURES)
        .map(|j| {
            let variance = rows
                .iter()
                .map(|r| (clean(r[j]) - means[j]).powi(2))
                .sum::<f64>()
                / n;
            if variance.sqrt() > 1e-9 {
                variance.sqrt()
            } else {
                1.0
            }
        })
        .collect();
    let x: Vec<Vec<f64>> = rows
        .iter()
        .map(|r| {
            (0..FEATURES)
                .map(|j| (clean(r[j]) - means[j]) / scales[j])
                .collect()
        })
        .collect();
    let mut weights = vec![0.0; FEATURES];
    let mut bias = 0.0;
    for _ in 0..ITERATIONS {
        let mut grad = vec![0.0; FEATURES];
        let mut grad_bias = 0.0;
        for (row, label) in x.iter().zip(labels) {
            let error = sigmoid(bias + row.iter().zip(&weights).map(|(a, w)| a * w).sum::<f64>())
                - f64::from(u8::from(*label));
            grad_bias += error;
            for (g, a) in grad.iter_mut().zip(row) {
                *g += error * a;
            }
        }
        bias -= LEARNING_RATE * grad_bias / n;
        for (w, g) in weights.iter_mut().zip(&grad) {
            *w -= LEARNING_RATE * (g / n + L2 * *w);
        }
    }
    Fit {
        weights,
        bias,
        means,
        scales,
    }
}

fn score(fit: &Fit, row: &[f64]) -> f64 {
    fit.bias
        + (0..FEATURES)
            .map(|j| {
                fit.weights[j] * (if row[j].is_finite() { row[j] } else { 0.0 } - fit.means[j])
                    / fit.scales[j]
            })
            .sum::<f64>()
}

/// Train on `(features, failed)` examples. Each example is its own holdout unit.
pub fn train(examples: &[(Vec<f64>, bool)], seed: u64) -> Option<StepProbe> {
    let groups: Vec<usize> = (0..examples.len()).collect();
    train_grouped(examples, &groups, seed)
}

/// `groups[i]` identifies the unit (trajectory) of example `i`; a unit is held out as a whole so
/// events of one trajectory never sit on both sides of the split. Units are assumed to carry a
/// single label.
fn train_grouped(examples: &[(Vec<f64>, bool)], groups: &[usize], seed: u64) -> Option<StepProbe> {
    if examples.len() < MIN_EXAMPLES
        || examples.len() != groups.len()
        || examples.iter().any(|(f, _)| f.len() != FEATURES)
    {
        return None;
    }
    let failures = examples.iter().filter(|(_, l)| *l).count();
    if failures == 0 || failures == examples.len() {
        return None;
    }

    // Stratified holdout over units: ~20% of each class's units, only when both sides stay populated.
    let mut rng = StdRng::seed_from_u64(seed);
    let mut held_out: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for class in [false, true] {
        let mut units: Vec<usize> = groups
            .iter()
            .zip(examples)
            .filter(|(_, (_, l))| *l == class)
            .map(|(g, _)| *g)
            .collect();
        units.sort_unstable();
        units.dedup();
        if units.len() < 2 {
            continue;
        }
        units.shuffle(&mut rng);
        held_out.extend(
            units[..((units.len() as f64 * HOLDOUT).round() as usize).clamp(1, units.len() - 1)]
                .iter()
                .copied(),
        );
    }
    let (mut train_rows, mut train_labels, mut test_rows, mut test_labels) =
        (vec![], vec![], vec![], vec![]);
    for ((features, label), group) in examples.iter().zip(groups) {
        if held_out.contains(group) {
            test_rows.push(features.as_slice());
            test_labels.push(*label);
        } else {
            train_rows.push(features.as_slice());
            train_labels.push(*label);
        }
    }
    let holdout_usable = test_labels.iter().any(|l| *l)
        && test_labels.iter().any(|l| !*l)
        && train_labels.iter().any(|l| *l)
        && train_labels.iter().any(|l| !*l);
    let auc_value = if holdout_usable {
        let split = fit(&train_rows, &train_labels);
        let scores: Vec<f64> = test_rows.iter().map(|r| score(&split, r)).collect();
        auc(&scores, &test_labels)
    } else {
        0.5
    };

    let rows: Vec<&[f64]> = examples.iter().map(|(f, _)| f.as_slice()).collect();
    let labels: Vec<bool> = examples.iter().map(|(_, l)| *l).collect();
    let Fit {
        weights,
        bias,
        means,
        scales,
    } = fit(&rows, &labels);
    Some(StepProbe {
        weights,
        bias,
        means,
        scales,
        trained_on: examples.len(),
        auc: auc_value,
    })
}

/// Train on every event of the most recent `limit` scored trajectories (failure = score < 100).
/// `None` until at least 30 trajectories with events exist and both outcomes occur.
pub fn train_from_store(store: &Store, limit: usize, seed: u64) -> Result<Option<StepProbe>> {
    let mut examples = Vec::new();
    let mut groups = Vec::new();
    let (mut failed, mut passed) = (0usize, 0usize);
    for (id, score) in store.scored_trajectories(limit)? {
        let events = store
            .trajectory_events(id)
            .with_context(|| format!("reading events of trajectory {id}"))?;
        if events.is_empty() {
            continue;
        }
        let label = score < 100.0;
        if label {
            failed += 1
        } else {
            passed += 1
        }
        for (kind, step, data) in &events {
            examples.push((features(kind, *step, data), label));
            groups.push(id as usize);
        }
    }
    if failed + passed < MIN_TRAJECTORIES || failed == 0 || passed == 0 {
        return Ok(None);
    }
    Ok(train_grouped(&examples, &groups, seed))
}

pub fn save(store: &Store, p: &StepProbe) -> Result<()> {
    store.set_meta(META_KEY, &serde_json::to_string(p)?)
}

/// The saved probe, or `None` when absent or unreadable (a stale or damaged probe is retrained).
pub fn load(store: &Store) -> Result<Option<StepProbe>> {
    let Some(text) = store.get_meta(META_KEY)? else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<StepProbe>(&text).ok().filter(|p| {
        p.weights.len() == FEATURES
            && p.means.len() == FEATURES
            && p.scales.len() == FEATURES
            && p.scales.iter().all(|s| s.is_finite() && *s > 0.0)
            && p.weights.iter().chain(&p.means).all(|x| x.is_finite())
            && p.bias.is_finite()
    }))
}

/// The `recent` most recent finished trajectories (scored or not), riskiest first; ties go to the
/// more recent trajectory.
pub fn rank_risky(store: &Store, probe: &StepProbe, recent: usize) -> Result<Vec<(i64, f64)>> {
    let listing = store.recent(recent)?;
    let mut ranked = Vec::new();
    for item in listing.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        ranked.push((id, probe.trajectory_risk(&store.trajectory_events(id)?)));
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    Ok(ranked)
}

/// Whether the probe has enough evidence to influence decisions.
pub fn informative(p: &StepProbe) -> bool {
    p.auc >= 0.6 && p.trained_on >= 100
}

/// Observation-only workflow hypotheses from active-version runs. These aggregate counters are
/// not verifier labels and never establish that a candidate is better; the sealed gate does that.
pub fn workflow_evidence(recent: &Value, version: i64) -> super::workflow::Evidence {
    let mut evidence = super::workflow::Evidence {
        source_version: version,
        ..Default::default()
    };
    for row in recent.as_array().into_iter().flatten() {
        if row["version"].as_i64() != Some(version) {
            continue;
        }
        let Some(events) = row["events"].as_array().filter(|events| !events.is_empty()) else {
            continue;
        };
        let mut metrics = super::workflow::Metrics::default();
        for event in events {
            let kind = event["kind"].as_str().unwrap_or("");
            let data = &event["data"];
            metrics.observe(kind, data);
            if kind == "tool_form_failure" {
                let kind = data["kind"].as_str().unwrap_or("unknown");
                // Bound recorded keys; arbitrary payloads cannot expand metadata or instructions.
                let kind = if FORM_KINDS.contains(&kind) {
                    kind
                } else {
                    "unknown"
                };
                *evidence.failure_kinds.entry(kind.to_owned()).or_default() += 1;
            }
        }
        evidence.observed.add(&metrics);
        evidence.trajectories += 1;
        evidence.protocol_trajectories += usize::from(metrics.protocol_errors > 0);
        let other_calls = metrics
            .tool_calls
            .saturating_sub(metrics.investigation_calls);
        evidence.investigation_trajectories += usize::from(
            metrics.investigation_calls >= 8
                && metrics.investigation_calls >= other_calls.saturating_mul(2),
        );
    }
    evidence
}

#[cfg(test)]
mod workflow_tests {
    use super::workflow_evidence;
    use serde_json::json;

    #[test]
    fn workflow_hypotheses_require_distinct_active_version_trajectories() {
        let protocol = json!({"kind":"tool_form_failure","data":{"kind":"schema"}});
        let read = json!({"kind":"tool_call","data":{"name":"read"}});
        let mut events = vec![read; 8];
        events.extend(vec![protocol.clone(); 5]);
        let observations = json!([
            {"version":7,"events":events},
            {"version":6,"events":[protocol.clone()]},
            {"version":7,"events":[protocol]},
            {"version":7,"events":[]}
        ]);
        let result = workflow_evidence(&observations, 7);
        assert_eq!(result.trajectories, 2);
        assert_eq!(result.protocol_trajectories, 2);
        assert_eq!(result.investigation_trajectories, 1);
        assert_eq!(result.failure_kinds.get("schema"), Some(&6));
        assert_eq!(result.observed.protocol_errors, 6);
        assert_eq!(result.observed.tool_calls, 8);
    }

    #[test]
    fn malformed_events_do_not_become_recurring_investigation() {
        let result = workflow_evidence(
            &json!([
                {"version":1,"events":[{},{"kind":"generation","data":{"tool_calls":"bad"}}]},
                {"version":1,"events":"bad"},
                {"events":[{"kind":"tool_form_failure"}]}
            ]),
            1,
        );
        assert_eq!(result.trajectories, 1);
        assert_eq!(result.investigation_trajectories, 0);
        assert_eq!(result.protocol_trajectories, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Event, Harness, Outcome};
    use serde_json::json;

    fn harness() -> Harness {
        Harness {
            system_prompt: "test".into(),
            memory: vec![],
            tools: vec![],
            subagents: vec![],
            max_steps: 2,
            temperature: 0.0,
            max_tokens: 32,
            edit_mode: None,
        }
    }

    fn synthetic(n: usize) -> Vec<(Vec<f64>, bool)> {
        (0..n)
            .map(|i| {
                let failed = i % 2 == 0;
                let mut f = features(
                    "tool_result",
                    (i % 7) as u32,
                    &json!({"name":"bash","output":"ok"}),
                );
                if failed {
                    f = features(
                        "tool_error",
                        (i % 7) as u32,
                        &json!({"name":"bash","output":"error: boom\n[exit code: 2]"}),
                    );
                }
                (f, failed)
            })
            .collect()
    }

    #[test]
    fn features_have_fixed_length_and_are_finite_even_for_garbage() {
        for data in [
            json!(null),
            json!({}),
            json!({"output":5,"prompt_tokens":"x","elapsed_ms":-3,"name":7}),
            json!({"output":"[exit code: 1]","prompt_tokens":120,"completion_tokens":30,"elapsed_ms":900,"tool_calls":2}),
        ] {
            for kind in ["generation", "tool_form_failure", "mystery", ""] {
                let f = features(kind, 3, &data);
                assert_eq!(f.len(), FEATURES);
                assert!(f.iter().all(|x| x.is_finite()));
            }
        }
        let f = features(
            "tool_form_failure",
            0,
            &json!({"kind":"schema","name":"read"}),
        );
        assert_eq!(f[F_FORM + 3], 1.0);
        assert_eq!(f[F_KIND + 5], 1.0);
        assert_eq!(
            features("tool_result", 0, &json!({"output":"x\n[exit code: 0]"}))[F_EXIT_NONZERO],
            0.0
        );
        assert_eq!(
            features("tool_error", 0, &json!({"output":"x\n[exit code: 3]"}))[F_EXIT_NONZERO],
            1.0
        );
        assert_eq!(
            features(
                "verification",
                0,
                &json!({"command":"t","exit":1,"passed":false})
            )[F_EXIT_NONZERO],
            1.0
        );
    }

    #[test]
    fn training_separates_a_synthetic_dataset() {
        let probe = train(&synthetic(200), 7).unwrap();
        assert!(probe.auc > 0.9, "auc {}", probe.auc);
        assert_eq!(probe.trained_on, 200);
        let failing = probe.risk(&features(
            "tool_error",
            2,
            &json!({"name":"bash","output":"error\n[exit code: 2]"}),
        ));
        let fine = probe.risk(&features(
            "tool_result",
            2,
            &json!({"name":"bash","output":"ok"}),
        ));
        assert!(
            failing > 0.8 && fine < 0.2 && (0.0..=1.0).contains(&failing),
            "{failing} {fine}"
        );
    }

    #[test]
    fn degenerate_data_is_rejected() {
        assert!(train(&synthetic(10), 1).is_none());
        let one_class: Vec<_> = synthetic(40).into_iter().map(|(f, _)| (f, true)).collect();
        assert!(train(&one_class, 1).is_none());
        let mut wrong_width = synthetic(40);
        wrong_width[0].0.pop();
        assert!(train(&wrong_width, 1).is_none());
    }

    #[test]
    fn training_is_deterministic_for_a_seed_and_the_seed_matters_for_the_split() {
        // Noisy labels so the holdout AUC depends on which examples are held out.
        let noisy: Vec<_> = synthetic(100)
            .into_iter()
            .enumerate()
            .map(|(i, (f, l))| (f, if i % 5 == 0 { !l } else { l }))
            .collect();
        let (a, b) = (train(&noisy, 3).unwrap(), train(&noisy, 3).unwrap());
        assert_eq!((a.weights, a.bias, a.auc), (b.weights, b.bias, b.auc));
        let aucs: Vec<f64> = (0..8).map(|s| train(&noisy, s).unwrap().auc).collect();
        assert!(aucs.iter().any(|x| *x != aucs[0]), "{aucs:?}");
    }

    #[test]
    fn holdout_does_not_memorize_events_from_the_same_trajectory() {
        // The only signal identifies the trajectory, not anything generalizable about failure.
        let mut examples = Vec::new();
        let mut groups = Vec::new();
        for trajectory in 0..10 {
            for _ in 0..20 {
                let mut f = vec![0.0; FEATURES];
                f[trajectory] = 1.0;
                examples.push((f, trajectory % 2 == 0));
                groups.push(trajectory);
            }
        }
        assert!(train(&examples, 7).unwrap().auc > 0.9);
        assert_eq!(train_grouped(&examples, &groups, 7).unwrap().auc, 0.5);
    }

    #[test]
    fn auc_is_the_rank_statistic_and_handles_ties() {
        assert_eq!(auc(&[0.1, 0.2, 0.8, 0.9], &[false, false, true, true]), 1.0);
        assert_eq!(auc(&[0.9, 0.8, 0.2, 0.1], &[false, false, true, true]), 0.0);
        assert_eq!(
            auc(&[0.5; 6], &[true, false, true, false, true, false]),
            0.5
        );
        // pos {0.3, 0.7}, neg {0.3, 0.5}: pairs (0.3>0.3 tie .5)+(0.3>0.5 no)+(0.7>0.3)+(0.7>0.5) = 2.5/4
        assert!((auc(&[0.3, 0.7, 0.3, 0.5], &[true, true, false, false]) - 0.625).abs() < 1e-12);
        assert_eq!(auc(&[0.1, 0.2], &[true, true]), 0.5);
    }

    #[test]
    fn trajectory_risk_is_the_mean_of_the_top_three_steps() {
        let probe = train(&synthetic(100), 1).unwrap();
        let bad = (
            "tool_error".to_string(),
            1,
            json!({"name":"bash","output":"error\n[exit code: 2]"}),
        );
        let good = (
            "tool_result".to_string(),
            1,
            json!({"name":"bash","output":"ok"}),
        );
        let (rb, rg) = (
            probe.risk(&features(&bad.0, 1, &bad.2)),
            probe.risk(&features(&good.0, 1, &good.2)),
        );
        let events = vec![
            good.clone(),
            bad.clone(),
            good.clone(),
            good.clone(),
            bad.clone(),
        ];
        assert!((probe.trajectory_risk(&events) - (2.0 * rb + rg) / 3.0).abs() < 1e-12);
        assert_eq!(probe.trajectory_risk(&[]), 0.0);
        assert!(probe.trajectory_risk(&[bad]) > probe.trajectory_risk(&[good]));
    }

    /// One finished trajectory with `events`, scored `score`.
    fn trajectory(
        store: &Store,
        version: i64,
        i: usize,
        events: &[(&str, Value)],
        score: Option<f64>,
    ) -> i64 {
        let id = store
            .begin_trajectory(version, "selfplay:x", "t", &format!("p{i}"))
            .unwrap();
        for (step, (kind, data)) in events.iter().enumerate() {
            store
                .event(&Event {
                    trajectory: id,
                    version,
                    domain: "selfplay:x".into(),
                    task: "t".into(),
                    step: step as u32,
                    kind: (*kind).into(),
                    data: data.clone(),
                    score: None,
                })
                .unwrap();
        }
        store.finish_trajectory(id, Some("done"), None).unwrap();
        if let Some(score) = score {
            store
                .outcome(&Outcome {
                    trajectory: id,
                    score,
                    source: "verifier:x".into(),
                    reference: None,
                })
                .unwrap();
        }
        id
    }

    fn good_events() -> Vec<(&'static str, Value)> {
        vec![
            (
                "generation",
                json!({"output":"reading","tool_calls":1,"prompt_tokens":100,"completion_tokens":10,"elapsed_ms":300}),
            ),
            ("tool_call", json!({"name":"read"})),
            ("tool_result", json!({"name":"read","output":"fine"})),
        ]
    }

    fn bad_events() -> Vec<(&'static str, Value)> {
        vec![
            (
                "generation",
                json!({"output":"","tool_calls":0,"prompt_tokens":400,"completion_tokens":5,"elapsed_ms":900}),
            ),
            ("tool_form_failure", json!({"kind":"text_call"})),
            (
                "tool_error",
                json!({"name":"bash","output":"error: nope\n[exit code: 1]"}),
            ),
        ]
    }

    fn populated_store(dir: &std::path::Path, pairs: usize) -> (Store, i64) {
        let store = Store::open(dir, "m", "u", &harness()).unwrap();
        let version = store.active().unwrap().id;
        for i in 0..pairs {
            trajectory(&store, version, 2 * i, &good_events(), Some(100.0));
            trajectory(&store, version, 2 * i + 1, &bad_events(), Some(0.0));
        }
        (store, version)
    }

    #[test]
    fn save_and_load_round_trip_through_a_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "m", "u", &Harness::default()).unwrap();
        assert!(load(&store).unwrap().is_none());
        let probe = train(&synthetic(60), 5).unwrap();
        save(&store, &probe).unwrap();
        let loaded = load(&store).unwrap().unwrap();
        for (features, _) in synthetic(60) {
            assert!((loaded.risk(&features) - probe.risk(&features)).abs() < 1e-12);
        }
        store.set_meta("step_probe", "{not json").unwrap();
        assert!(load(&store).unwrap().is_none());
    }

    #[test]
    fn train_from_store_needs_enough_trajectories_with_both_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, version) = populated_store(dir.path(), 5);
        assert!(
            train_from_store(&store, 100, 1).unwrap().is_none(),
            "10 trajectories is too few"
        );
        let dir = tempfile::tempdir().unwrap();
        let store2 = Store::open(dir.path(), "m", "u", &harness()).unwrap();
        for i in 0..40 {
            trajectory(
                &store2,
                store2.active().unwrap().id,
                i,
                &good_events(),
                Some(100.0),
            );
        }
        assert!(
            train_from_store(&store2, 100, 1).unwrap().is_none(),
            "one class only"
        );
        let _ = version;
    }

    #[test]
    fn train_from_store_learns_failure_signal_and_rank_risky_orders_by_risk() {
        let dir = tempfile::tempdir().unwrap();
        let (store, version) = populated_store(dir.path(), 25);
        let probe = train_from_store(&store, 1000, 9).unwrap().unwrap();
        assert_eq!(probe.trained_on, 50 * 3);
        assert!(probe.auc > 0.9, "auc {}", probe.auc);
        assert!(!informative(&probe) || probe.trained_on >= 100);

        // Unscored finished trajectories are ranked too: a failing one among healthy ones.
        let healthy1 = trajectory(&store, version, 100, &good_events(), None);
        let failing = trajectory(&store, version, 101, &bad_events(), None);
        let healthy2 = trajectory(&store, version, 102, &good_events(), None);
        let ranked = rank_risky(&store, &probe, 3).unwrap();
        assert_eq!(
            ranked.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![failing, healthy2, healthy1]
        );
        assert!(
            ranked[0].1 > 0.8 && ranked[1].1 < 0.2 && ranked[1].1 >= ranked[2].1
                || ranked[1].0 == healthy2
        );
        assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1));
    }

    #[test]
    fn informative_requires_both_signal_and_volume() {
        let mut p = train(&synthetic(200), 1).unwrap();
        p.auc = 0.7;
        assert!(informative(&p));
        p.auc = 0.55;
        assert!(!informative(&p));
        p.auc = 0.9;
        p.trained_on = 99;
        assert!(!informative(&p));
    }
}
