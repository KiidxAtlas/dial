//! The sealed admission gate: a candidate version replaces the baseline only if the primary suite
//! improves (by a margin and, optionally, with statistical significance) and no guard suite regresses.
//! Cases are verified by deterministic `Check`s; suites come from `load_manual` (private JSONL files)
//! or are generated in memory by the caller.
use super::verify::Check;
use crate::config::types::{Admission, GateConfig, SuiteScore, Version};
use crate::llm::errors::{self, ErrorClass, ThinkingLoop};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

/// Workspace files a case seeds before the agent runs (relative path -> content).
pub type Files = BTreeMap<String, String>;

/// One gate task: a prompt, the workspace it starts from and the deterministic check of its outcome.
#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub prompt: String,
    pub files: Files,
    pub check: Check,
}

#[derive(Debug, Clone)]
pub struct Suite {
    pub name: String,
    pub primary: bool,
    pub cases: Vec<Case>,
}

/// Admission thresholds. Percentages are on a 0..100 scale; case counts are paired differences
/// (`candidate_only` = cases only the candidate solved, `baseline_only` = only the baseline solved).
#[derive(Clone, Debug)]
pub struct Rules {
    /// Largest tolerated drop, in percentage points, of any suite.
    pub max_regression: f64,
    /// Smallest required primary gain, in percentage points.
    pub min_improvement: f64,
    /// Statistical mode when below `1.0`: the one-sided level of the primary sign test, and of every
    /// suite's regression bound (divided evenly across suites). `1.0` is the legacy point-estimate
    /// mode, used by manual suites so their documented behaviour does not change.
    pub alpha: f64,
    /// Required `candidate_only - baseline_only` on the primary suite.
    pub min_net_wins: u32,
    /// Stop as soon as the outcome cannot change: a rejection already certain (a suite over its
    /// regression bound, or a primary suite that could not reach the required gain even if every
    /// remaining case went the candidate's way). Never accepts earlier and never changes whether
    /// a candidate is admitted; it only skips cases that could not matter.
    pub stop_when_futile: bool,
}

impl Rules {
    /// Legacy semantics: point thresholds only, no significance test, no net-win or slack allowances.
    pub fn from_gate(g: &GateConfig) -> Rules {
        Rules {
            max_regression: g.max_regression,
            min_improvement: g.min_improvement,
            alpha: 1.0,
            min_net_wins: 0,
            stop_when_futile: false,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_regression.is_finite() && (0.0..=100.0).contains(&self.max_regression),
            "gate max_regression must be finite and between 0 and 100 percentage points"
        );
        ensure!(
            self.min_improvement.is_finite() && self.min_improvement > 0.0,
            "gate min_improvement must be finite and positive in percentage points"
        );
        ensure!(
            self.alpha.is_finite() && self.alpha > 0.0 && self.alpha <= 1.0,
            "gate alpha must be in (0, 1]"
        );
        Ok(())
    }
}

fn validate_files(files: &Files, suite: usize) -> Result<()> {
    ensure!(files.len() <= 64, "too many seeded files in suite {suite}");
    for (path, content) in files {
        let p = Path::new(path);
        ensure!(
            !path.is_empty()
                && !path.contains('\0')
                && p.is_relative()
                && p.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                && content.len() <= 256 * 1024,
            "invalid seeded file in suite {suite}"
        );
    }
    Ok(())
}

/// Structure every suite set must have, wherever it came from. Errors name suites by position only.
fn validate_suites(suites: &[Suite]) -> Result<()> {
    ensure!(!suites.is_empty(), "gate requires at least one suite");
    ensure!(
        suites.iter().filter(|s| s.primary).count() == 1,
        "gate requires exactly one primary suite"
    );
    let mut names = HashSet::new();
    for (index, suite) in suites.iter().enumerate() {
        let n = index + 1;
        ensure!(
            !suite.name.trim().is_empty() && names.insert(suite.name.trim()),
            "gate suite names must be nonempty and distinct"
        );
        ensure!(!suite.cases.is_empty(), "private gate suite {n} is empty");
        let mut ids = HashSet::new();
        for case in &suite.cases {
            ensure!(
                !case.id.trim().is_empty() && ids.insert(case.id.trim()),
                "empty or duplicate private case identifier in suite {n}"
            );
            ensure!(
                !case.prompt.trim().is_empty(),
                "empty private gate prompt in suite {n}"
            );
            validate_files(&case.files, n)?;
        }
    }
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Matcher {
    #[default]
    Exact,
    Json,
}

/// Keeps an explicit `null` expectation (valid for the json matcher) distinct from an absent one.
fn present<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCase {
    id: String,
    prompt: String,
    #[serde(default, deserialize_with = "present")]
    expected: Option<Value>,
    #[serde(default)]
    matcher: Option<Matcher>,
    #[serde(default)]
    files: Files,
    #[serde(default)]
    check: Option<Check>,
}

impl RawCase {
    /// `None` when the case is not exactly one of `check` or legacy `expected`(+`matcher`).
    fn into_case(self) -> Option<Case> {
        let check = match (self.check, self.expected, self.matcher) {
            (Some(check), None, None) => check,
            (None, Some(Value::String(expected)), None | Some(Matcher::Exact)) => {
                Check::AnswerExact { expected }
            }
            (None, Some(expected), Some(Matcher::Json)) => Check::AnswerJson { expected },
            _ => return None,
        };
        Some(Case {
            id: self.id,
            prompt: self.prompt,
            files: self.files,
            check,
        })
    }
}

/// Reads the private JSONL suites named by the config. I/O and serde errors are never attached:
/// they could contain private paths or cases.
pub fn load_manual(config: &GateConfig) -> Result<Vec<Suite>> {
    ensure!(
        config.max_regression.is_finite() && (0.0..=100.0).contains(&config.max_regression),
        "gate max_regression must be finite and between 0 and 100 percentage points"
    );
    ensure!(
        config.min_improvement.is_finite() && config.min_improvement > 0.0,
        "gate min_improvement must be finite and positive in percentage points"
    );
    ensure!(
        config.suites.len() >= 2,
        "gate requires at least two suites"
    );
    ensure!(
        config.suites.iter().filter(|s| s.primary).count() == 1,
        "gate requires exactly one primary suite"
    );
    let mut previous_prompts = HashSet::new();
    let mut suites = Vec::with_capacity(config.suites.len());
    for (index, suite) in config.suites.iter().enumerate() {
        let n = index + 1;
        let file = File::open(&suite.path)
            .map_err(|_| anyhow::anyhow!("cannot open private gate suite {n}"))?;
        let mut cases = Vec::new();
        for (line, text) in BufReader::new(file).lines().enumerate() {
            let text = text.map_err(|_| anyhow::anyhow!("cannot read private gate suite {n}"))?;
            let invalid = || {
                anyhow::anyhow!(
                    "invalid private gate case in suite {n} at line {}",
                    line + 1
                )
            };
            let raw: RawCase = serde_json::from_str(&text).map_err(|_| invalid())?;
            let case = raw.into_case().ok_or_else(invalid)?;
            ensure!(
                !previous_prompts.contains(case.prompt.trim()),
                "private gate suites must not share prompts"
            );
            cases.push(case);
        }
        previous_prompts.extend(cases.iter().map(|case| case.prompt.trim().to_owned()));
        suites.push(Suite {
            name: suite.name.clone(),
            primary: suite.primary,
            cases,
        });
    }
    validate_suites(&suites)?;
    Ok(suites)
}

/// Trimmed prompts of every gate case, for filtering training data. Never logged or reported.
pub fn held_out_prompts(suites: &[Suite]) -> HashSet<String> {
    suites
        .iter()
        .flat_map(|suite| suite.cases.iter().map(|case| case.prompt.trim().to_owned()))
        .collect()
}

/// Exact trimmed prompt overlap is prevented; this cannot prove semantic disjointness.
pub fn check_training_isolation(
    prompts: &HashSet<String>,
    train: &Path,
    valid: &Path,
) -> Result<()> {
    for (index, path) in [train, valid].iter().enumerate() {
        let file = File::open(path)
            .map_err(|_| anyhow::anyhow!("cannot open training isolation input {}", index + 1))?;
        for (line, text) in BufReader::new(file).lines().enumerate() {
            let text = text.map_err(|_| {
                anyhow::anyhow!("cannot read training isolation input {}", index + 1)
            })?;
            let record: Value = serde_json::from_str(&text).map_err(|_| {
                anyhow::anyhow!(
                    "invalid training isolation record in input {} at line {}",
                    index + 1,
                    line + 1
                )
            })?;
            let messages = record
                .get("messages")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "training isolation requires message records in input {} at line {}",
                        index + 1,
                        line + 1
                    )
                })?;
            for message in messages {
                let role = message.get("role").and_then(Value::as_str);
                // Assistant tool-call turns legitimately carry no text content.
                let content = message.get("content");
                ensure!(
                    role.is_some()
                        && (content.is_some_and(|c| c.is_string())
                            || (role == Some("assistant") && message.get("tool_calls").is_some())),
                    "invalid training isolation message in input {} at line {}",
                    index + 1,
                    line + 1
                );
                if role == Some("user")
                    && prompts.contains(content.and_then(Value::as_str).unwrap_or("").trim())
                {
                    bail!(
                        "training isolation rejected held-out prompt overlap in input {}",
                        index + 1
                    );
                }
            }
        }
    }
    Ok(())
}

/// One-sided exact binomial tail `P(X >= wins)` for `X ~ Binomial(wins + losses, 1/2)`: the chance
/// of at least this many candidate-only wins if candidate and baseline were equally good.
/// Exact integer arithmetic for small samples, log-space summation (no overflow) for large ones.
pub fn sign_test_p(wins: usize, losses: usize) -> f64 {
    if wins == 0 {
        return 1.0;
    }
    let n = wins + losses;
    if n <= 100 {
        let (n, wins) = (n as u128, wins as u128);
        // Binomial coefficients C(n, k) <= 2^100 stay far below u128::MAX in the multiply below.
        let mut c: u128 = 1;
        let mut tail: u128 = 0;
        for k in 0..=n {
            if k >= wins {
                tail += c;
            }
            c = c * (n - k) / (k + 1);
        }
        return tail as f64 / 2f64.powi(n as i32);
    }
    // Sum the smaller tail. Directly summing an almost-one tail accumulates cancellation error
    // and can make p-values spuriously increase as evidence improves.
    if wins <= losses {
        return (1.0 - sign_test_p(losses + 1, wins - 1)).clamp(0.0, 1.0);
    }
    // ln C(n, wins) accumulated as a product of ratios, then walked up the tail term by term.
    let mut ln_term = (1..=wins)
        .map(|i| (((n - wins + i) as f64) / i as f64).ln())
        .sum::<f64>();
    let ln_total = n as f64 * std::f64::consts::LN_2;
    let mut p = 0.0;
    for k in wins..=n {
        p += (ln_term - ln_total).exp();
        if k < n {
            ln_term += ((n - k) as f64).ln() - ((k + 1) as f64).ln();
        }
    }
    p.clamp(0.0, 1.0)
}

/// Fewest discordant pairs, all favouring one side, at which the one-sided sign test can reach `alpha`.
/// This is the sample-size floor below which no primary gain can be significant at that level.
pub fn min_discordant_for(alpha: f64) -> usize {
    let mut k = 0;
    while 0.5f64.powi(k as i32) > alpha {
        k += 1;
    }
    k
}

/// `P(X <= k)` for `X ~ Binomial(n, p)`, summed in log space so large `n` cannot overflow.
pub fn binomial_cdf(k: usize, n: usize, p: f64) -> f64 {
    if k >= n || p <= 0.0 {
        return 1.0;
    }
    if p >= 1.0 {
        return 0.0;
    }
    let (lp, lq) = (p.ln(), (1.0 - p).ln());
    // log P(X = i), walked up from i = 0 by the ratio of consecutive terms.
    let mut ln_term = n as f64 * lq;
    let mut total = 0.0;
    for i in 0..=k {
        total += ln_term.exp();
        if i < n {
            ln_term += ((n - i) as f64 / (i + 1) as f64).ln() + lp - lq;
        }
    }
    total.min(1.0)
}

/// One-sided Clopper-Pearson upper bound on a success probability: the largest `p` for which seeing
/// at most `successes` of `trials` still has probability at least `alpha`.
pub fn clopper_pearson_upper(successes: usize, trials: usize, alpha: f64) -> f64 {
    if successes >= trials {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        // The cdf falls as p rises; the bound is where it crosses alpha.
        if binomial_cdf(successes, trials, mid) > alpha {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// Upper confidence bound, in percentage points, on how far the baseline leads the candidate over the
/// population behind `pairs` paired cases, given `base_only` and `cand_only` discordant pairs.
/// The drop is `q * (2p - 1)`: `q` is the discordance rate and `p` the share of discordant pairs that
/// favour the baseline. Each gets a one-sided Clopper-Pearson bound at `alpha / 2` (union bound, so the
/// total is `alpha`). A sample without discordant pairs bounds the drop only by `q`: small suites
/// therefore cannot clear a tight tolerance, and the gate says so rather than assuming no regression.
pub fn regression_bound(base_only: usize, cand_only: usize, pairs: usize, alpha: f64) -> f64 {
    let discordant = base_only + cand_only;
    let q = clopper_pearson_upper(discordant, pairs, alpha / 2.0);
    let p = if discordant == 0 {
        1.0
    } else {
        clopper_pearson_upper(base_only, discordant, alpha / 2.0)
    };
    100.0 * q * (2.0 * p - 1.0).max(0.0)
}

fn percent(count: usize, total: usize) -> f64 {
    100.0 * count as f64 / total as f64
}

/// Retry delays for an infrastructure failure: one further attempt after each, then the pair is excluded.
const INFRA_BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

/// Largest share of a suite that infrastructure failures may exclude; beyond it nothing is admissible.
const MAX_EXCLUDED_PERCENT: usize = 20;

/// Why a runner failure matters to the evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// Provider or transport trouble unrelated to the model: retried, then the pair is excluded.
    Infra(&'static str),
    /// The model's own output failed (thinking loop, rejected tool-call JSON, context overflow): unsolved.
    Model,
    /// Operator stop, budgets, auth, quota, cancellation or anything unrecognised: aborts the evaluation.
    Fatal,
}

/// One side of one case after infrastructure retries.
enum Measured {
    Solved(bool),
    /// Infrastructure failure that survived every retry; carries a fixed, non-private category.
    Excluded(&'static str),
}

/// Classifies a runner error by cause. Categories are fixed labels: they never carry prompts,
/// outputs or server text.
pub(crate) fn classify_failure(error: &anyhow::Error) -> FailureKind {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<ThinkingLoop>().is_some())
        || errors::is_model_output_rejection(error)
    {
        return FailureKind::Model;
    }
    let text = format!("{error:#}");
    if text.contains("Failed to load model") {
        return FailureKind::Infra("model_load");
    }
    match errors::classify(error) {
        ErrorClass::ContextOverflow => FailureKind::Model,
        ErrorClass::RateLimited { .. } if http_status(&text) == Some(529) => {
            FailureKind::Infra("transport_5xx")
        }
        ErrorClass::RateLimited { .. } => FailureKind::Infra("transport_429"),
        ErrorClass::Transient => FailureKind::Infra(transient_category(&text)),
        _ => FailureKind::Fatal,
    }
}

fn http_status(text: &str) -> Option<u16> {
    text.split("HTTP ").nth(1)?.get(..3)?.parse().ok()
}

fn transient_category(text: &str) -> &'static str {
    if let Some(status) = http_status(text) {
        return if status == 408 {
            "timeout"
        } else {
            "transport_5xx"
        };
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if ["connection", "connect", "socket", "reset"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "transport_connection"
    } else {
        "transport_error"
    }
}

fn describe_infra(counts: &BTreeMap<&'static str, usize>) -> String {
    counts
        .iter()
        .map(|(category, n)| format!("{category} x{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One side of one case. Infrastructure failures are retried with `backoff`, then reported as excluded;
/// model-caused failures count as unsolved; any other failure aborts with a detail-free `Err`.
fn measure(
    run: &mut impl FnMut(&Version, &Case) -> Result<bool>,
    version: &Version,
    case: &Case,
    backoff: &[Duration],
    stop: &dyn Fn() -> bool,
) -> std::result::Result<Measured, ()> {
    let mut retries = 0;
    loop {
        if stop() {
            return Err(());
        }
        match run(version, case) {
            Ok(solved) => return Ok(Measured::Solved(solved)),
            Err(error) => match classify_failure(&error) {
                FailureKind::Model => return Ok(Measured::Solved(false)),
                FailureKind::Fatal => return Err(()),
                FailureKind::Infra(category) => match backoff.get(retries) {
                    Some(delay) => {
                        retries += 1;
                        if !sleep_unless(*delay, stop) {
                            return Err(());
                        }
                    }
                    None => return Ok(Measured::Excluded(category)),
                },
            },
        }
    }
}

fn measure_error(stop: &dyn Fn() -> bool, detail: String) -> anyhow::Error {
    if stop() {
        crate::llm::Interrupted.into()
    } else {
        anyhow::anyhow!(detail)
    }
}

/// Sleeps for `delay` in short slices; false when `stop` fired first.
fn sleep_unless(delay: Duration, stop: &dyn Fn() -> bool) -> bool {
    let until = std::time::Instant::now() + delay;
    loop {
        if stop() {
            return false;
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Runs baseline and candidate on every case, interleaved per case so that drift in the model server
/// affects both equally. Private prompts, checks, outputs and runner errors never enter the report.
pub fn evaluate(
    rules: &Rules,
    suites: &[Suite],
    baseline: &Version,
    candidate: &Version,
    stop: &dyn Fn() -> bool,
    run: impl FnMut(&Version, &Case) -> Result<bool>,
) -> Result<Admission> {
    evaluate_outcome(rules, suites, baseline, candidate, stop, run).map(|(admission, _)| admission)
}

/// [`evaluate`] that also returns the typed verdict, so callers never classify it from reason text.
pub fn evaluate_outcome(
    rules: &Rules,
    suites: &[Suite],
    baseline: &Version,
    candidate: &Version,
    stop: &dyn Fn() -> bool,
    run: impl FnMut(&Version, &Case) -> Result<bool>,
) -> Result<(Admission, Outcome)> {
    judge_with(
        rules,
        suites,
        baseline,
        candidate,
        run,
        &INFRA_BACKOFF,
        stop,
    )
}

/// [`evaluate`] with explicit infrastructure-retry delays: one further attempt after each delay.
#[cfg(test)]
fn evaluate_with(
    rules: &Rules,
    suites: &[Suite],
    baseline: &Version,
    candidate: &Version,
    run: impl FnMut(&Version, &Case) -> Result<bool>,
    backoff: &[Duration],
    stop: &dyn Fn() -> bool,
) -> Result<Admission> {
    judge_with(rules, suites, baseline, candidate, run, backoff, stop)
        .map(|(admission, _)| admission)
}

fn judge_with(
    rules: &Rules,
    suites: &[Suite],
    baseline: &Version,
    candidate: &Version,
    mut run: impl FnMut(&Version, &Case) -> Result<bool>,
    backoff: &[Duration],
    stop: &dyn Fn() -> bool,
) -> Result<(Admission, Outcome)> {
    ensure!(
        candidate.parent == Some(baseline.id) && candidate.id != baseline.id,
        "candidate does not descend directly from the baseline"
    );
    ensure!(
        candidate.status == "candidate",
        "version is not a pending candidate"
    );
    rules.validate()?;
    validate_suites(suites)?;
    let mut scores = Vec::with_capacity(suites.len());
    let mut regression = false;
    let mut primary = Primary::TooSmall;
    let mut exclusions = Vec::new();
    // Statistical mode (alpha < 1): guard suites must be bounded, not just point-estimated. The primary
    // suite is judged for gain by the sign test, so its regression bound is not separately required.
    let statistical = rules.alpha < 1.0;
    let guard_count = suites.iter().filter(|s| !s.primary).count().max(1);
    let guard_alpha = rules.alpha / guard_count as f64;
    let mut unproven: Option<String> = None;
    for (index, suite) in suites.iter().enumerate() {
        let (mut base_ok, mut cand_ok, mut base_only, mut cand_only) = (0, 0, 0, 0);
        let (mut evaluated, mut excluded, mut processed) = (0, 0, 0);
        let mut infra = BTreeMap::new();
        let mut futile_stop = false;
        for case in &suite.cases {
            let before = measure(&mut run, baseline, case, backoff, stop).map_err(|()| {
                measure_error(
                    stop,
                    format!("baseline evaluation failed in private suite {}", index + 1),
                )
            })?;
            // A pair is measured or excluded as a whole, so both sides always score the same cases.
            let pair = match before {
                Measured::Excluded(category) => Err(category),
                Measured::Solved(before) => {
                    match measure(&mut run, candidate, case, backoff, stop).map_err(|()| {
                        measure_error(
                            stop,
                            format!("candidate evaluation failed in private suite {}", index + 1),
                        )
                    })? {
                        Measured::Solved(after) => Ok((before, after)),
                        Measured::Excluded(category) => Err(category),
                    }
                }
            };
            processed += 1;
            match pair {
                Ok((before, after)) => {
                    base_ok += usize::from(before);
                    cand_ok += usize::from(after);
                    base_only += usize::from(before && !after);
                    cand_only += usize::from(after && !before);
                    evaluated += 1;
                }
                Err(category) => {
                    excluded += 1;
                    *infra.entry(category).or_insert(0usize) += 1;
                    ensure!(
                        excluded * 100 <= suite.cases.len() * MAX_EXCLUDED_PERCENT,
                        "evaluation did not complete: infrastructure failures ({}) excluded too many cases of private suite {}",
                        describe_infra(&infra),
                        index + 1
                    );
                }
            }
            let remaining = suite.cases.len() - processed;
            // Futility projects the full suite; once a pair is excluded that projection no longer holds.
            if rules.stop_when_futile
                && suite.primary
                && remaining > 0
                && excluded == 0
                && !regression
                && primary_futile(
                    rules,
                    suite.cases.len(),
                    (base_ok, cand_ok, base_only, cand_only),
                    remaining,
                )
            {
                futile_stop = true;
                break;
            }
        }
        // Scores are over the cases actually measured; a regression bound is judged on the full suite only.
        let total = evaluated;
        let (base_pts, cand_pts) = (percent(base_ok, total), percent(cand_ok, total));
        regression |= !futile_stop && cand_pts < base_pts - rules.max_regression;
        if statistical && !suite.primary && !futile_stop && total > 0 {
            let bound = regression_bound(base_only, cand_only, total, guard_alpha);
            if bound > rules.max_regression {
                unproven.get_or_insert_with(|| {
                    format!(
                        "suite {}: regression upper bound {bound:.1} points over {total} paired cases exceeds the {:.1}-point tolerance",
                        suite.name, rules.max_regression
                    )
                });
            }
        }
        if futile_stop {
            primary = Primary::Futile;
        } else if suite.primary {
            let net_wins = cand_only as i64 - base_only as i64;
            primary = if cand_pts < base_pts + rules.min_improvement {
                Primary::TooSmall
            } else if net_wins < i64::from(rules.min_net_wins) {
                Primary::TooFewNetWins
            } else if rules.alpha < 1.0 && sign_test_p(cand_only, base_only) > rules.alpha {
                Primary::NotSignificant
            } else {
                Primary::Improved
            };
        }
        if excluded > 0 {
            exclusions.push(format!(
                "private suite {}: {excluded} case pairs excluded after infrastructure failures ({})",
                index + 1,
                describe_infra(&infra)
            ));
        }
        scores.push(SuiteScore {
            name: suite.name.clone(),
            primary: suite.primary,
            baseline: base_pts,
            candidate: cand_pts,
            cases: total,
            candidate_only: cand_only,
            baseline_only: base_only,
            tier: None,
        });
        if rules.stop_when_futile && (regression || futile_stop) {
            break;
        }
    }
    let outcome = match (regression, unproven.is_some(), &primary) {
        (true, _, _) => Outcome::Regressed,
        (false, true, _) => Outcome::Unproven,
        (false, false, Primary::Futile) => Outcome::Futile,
        (false, false, Primary::TooSmall) => Outcome::TooSmall,
        (false, false, Primary::TooFewNetWins) => Outcome::TooFewNetWins,
        (false, false, Primary::NotSignificant) => Outcome::NotSignificant,
        (false, false, Primary::Improved) => Outcome::Accepted,
    };
    let mut reason = match outcome {
        Outcome::Regressed => "rejected: at least one suite exceeds the permitted regression",
        Outcome::Futile => {
            "rejected: primary suite cannot reach the required improvement (stopped early)"
        }
        Outcome::TooSmall => "rejected: primary suite does not meet the required improvement",
        Outcome::TooFewNetWins => "rejected: primary suite has too few net case wins",
        Outcome::NotSignificant => "rejected: primary improvement is not statistically significant",
        Outcome::Unproven => "rejected: a suite's regression cannot be ruled out at the required confidence (sample too small)",
        Outcome::Accepted => {
            "accepted: primary improvement and every suite regression bound satisfied"
        }
        Outcome::Overridden => "rejected: admission overridden after the gate",
    }
    .to_owned();
    if outcome == Outcome::Unproven
        && let Some(detail) = &unproven
    {
        reason.push_str(&format!(" ({detail})"));
    }
    if !exclusions.is_empty() {
        reason.push_str(&format!(" ({})", exclusions.join("; ")));
    }
    let passed = outcome == Outcome::Accepted;
    Ok((
        Admission {
            candidate: candidate.id,
            baseline: baseline.id,
            passed,
            reason,
            scores,
        },
        outcome,
    ))
}

/// Typed verdict of one sealed gate run. Callers branch on this, never on the reason text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    /// A suite exceeded the permitted regression.
    Regressed,
    /// The primary suite could not reach the required gain even if every remaining case went its way.
    Futile,
    TooSmall,
    TooFewNetWins,
    NotSignificant,
    /// The guard suites' regression bounds cannot exclude the permitted regression at the required
    /// confidence: too few paired cases (or too many discordant ones) to show the candidate is safe.
    Unproven,
    /// The gate accepted, but a later check (drift anchor, workflow measurement) overrode it.
    Overridden,
}

impl Outcome {
    /// Primary shortfalls where more capacity may help: the gain was too small or too uncertain.
    /// Regressions and net-win shortfalls do not raise LoRA capacity.
    pub fn needs_more_signal(self) -> bool {
        matches!(
            self,
            Outcome::Futile | Outcome::TooSmall | Outcome::NotSignificant
        )
    }
}

enum Primary {
    Futile,
    Improved,
    TooSmall,
    TooFewNetWins,
    NotSignificant,
}

/// Whether the primary suite is certain to fail its gain, net-win or significance requirement even
/// if every remaining case were solved only by the candidate (the best case for admission).
fn primary_futile(
    rules: &Rules,
    total: usize,
    now: (usize, usize, usize, usize),
    remaining: usize,
) -> bool {
    let (base_ok, cand_ok, base_only, cand_only) = now;
    let best_cand_ok = cand_ok + remaining;
    let best_cand_only = cand_only + remaining;
    percent(best_cand_ok, total) < percent(base_ok, total) + rules.min_improvement
        || (best_cand_only as i64 - base_only as i64) < i64::from(rules.min_net_wins)
        || (rules.alpha < 1.0 && sign_test_p(best_cand_only, base_only) > rules.alpha)
}

#[cfg(test)]
mod tests;
