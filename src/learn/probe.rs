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

const KINDS: [&str; 9] = [
    "generation",
    "generation_error",
    "tool_call",
    "tool_result",
    "tool_error",
    "tool_form_failure",
    "constrained_repair",
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
#[cfg(test)]
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
mod workflow_tests;

#[cfg(test)]
mod tests;
