//! The sealed admission gate: a candidate version replaces the baseline only if the primary suite
//! improves (by a margin and, optionally, with statistical significance) and no guard suite regresses.
//! Cases are verified by deterministic `Check`s; suites come from `load_manual` (private JSONL files)
//! or are generated in memory by the caller.
use super::verify::Check;
use crate::config::types::{Admission, GateConfig, SuiteScore, Version};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

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
    /// One-sided sign-test level for the primary gain; `1.0` disables the test.
    pub alpha: f64,
    /// Required `candidate_only - baseline_only` on the primary suite.
    pub min_net_wins: u32,
}

impl Rules {
    /// Legacy semantics: point thresholds only, no significance test, no net-win or slack allowances.
    pub fn from_gate(g: &GateConfig) -> Rules {
        Rules { max_regression: g.max_regression, min_improvement: g.min_improvement, alpha: 1.0, min_net_wins: 0 }
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.max_regression.is_finite() && self.max_regression >= 0.0,
            "gate max_regression must be finite and non-negative percentage points");
        ensure!(self.min_improvement.is_finite() && self.min_improvement > 0.0,
            "gate min_improvement must be finite and positive in percentage points");
        ensure!(self.alpha.is_finite() && self.alpha > 0.0 && self.alpha <= 1.0, "gate alpha must be in (0, 1]");
        Ok(())
    }
}

fn validate_files(files: &Files, suite: usize) -> Result<()> {
    ensure!(files.len() <= 64, "too many seeded files in suite {suite}");
    for (path, content) in files {
        let p = Path::new(path);
        ensure!(!path.is_empty() && !path.contains('\0') && p.is_relative()
            && p.components().all(|c| matches!(c, std::path::Component::Normal(_))) && content.len() <= 256 * 1024,
            "invalid seeded file in suite {suite}");
    }
    Ok(())
}

/// Structure every suite set must have, wherever it came from. Errors name suites by position only.
fn validate_suites(suites: &[Suite]) -> Result<()> {
    ensure!(!suites.is_empty(), "gate requires at least one suite");
    ensure!(suites.iter().filter(|s| s.primary).count() == 1, "gate requires exactly one primary suite");
    let mut names = HashSet::new();
    for (index, suite) in suites.iter().enumerate() {
        let n = index + 1;
        ensure!(!suite.name.trim().is_empty() && names.insert(suite.name.trim()),
            "gate suite names must be nonempty and distinct");
        ensure!(!suite.cases.is_empty(), "private gate suite {n} is empty");
        let mut ids = HashSet::new();
        for case in &suite.cases {
            ensure!(!case.id.trim().is_empty() && ids.insert(case.id.trim()),
                "empty or duplicate private case identifier in suite {n}");
            ensure!(!case.prompt.trim().is_empty(), "empty private gate prompt in suite {n}");
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
            (None, Some(Value::String(expected)), None | Some(Matcher::Exact)) => Check::AnswerExact { expected },
            (None, Some(expected), Some(Matcher::Json)) => Check::AnswerJson { expected },
            _ => return None,
        };
        Some(Case { id: self.id, prompt: self.prompt, files: self.files, check })
    }
}

/// Reads the private JSONL suites named by the config. I/O and serde errors are never attached:
/// they could contain private paths or cases.
pub fn load_manual(config: &GateConfig) -> Result<Vec<Suite>> {
    ensure!(config.max_regression.is_finite() && (0.0..=1.0).contains(&config.max_regression),
        "gate max_regression must be finite and between 0 and 1 percentage points");
    ensure!(config.min_improvement.is_finite() && config.min_improvement > 0.0,
        "gate min_improvement must be finite and positive in percentage points");
    ensure!(config.suites.len() >= 2, "gate requires at least two suites");
    ensure!(config.suites.iter().filter(|s| s.primary).count() == 1,
        "gate requires exactly one primary suite");
    let mut previous_prompts = HashSet::new();
    let mut suites = Vec::with_capacity(config.suites.len());
    for (index, suite) in config.suites.iter().enumerate() {
        let n = index + 1;
        let file = File::open(&suite.path).map_err(|_| anyhow::anyhow!("cannot open private gate suite {n}"))?;
        let mut cases = Vec::new();
        for (line, text) in BufReader::new(file).lines().enumerate() {
            let text = text.map_err(|_| anyhow::anyhow!("cannot read private gate suite {n}"))?;
            let invalid = || anyhow::anyhow!("invalid private gate case in suite {n} at line {}", line + 1);
            let raw: RawCase = serde_json::from_str(&text).map_err(|_| invalid())?;
            let case = raw.into_case().ok_or_else(invalid)?;
            ensure!(!previous_prompts.contains(case.prompt.trim()), "private gate suites must not share prompts");
            cases.push(case);
        }
        previous_prompts.extend(cases.iter().map(|case| case.prompt.trim().to_owned()));
        suites.push(Suite { name: suite.name.clone(), primary: suite.primary, cases });
    }
    validate_suites(&suites)?;
    Ok(suites)
}

/// Trimmed prompts of every gate case, for filtering training data. Never logged or reported.
pub fn held_out_prompts(suites: &[Suite]) -> HashSet<String> {
    suites.iter().flat_map(|suite| suite.cases.iter().map(|case| case.prompt.trim().to_owned())).collect()
}

/// Exact trimmed prompt overlap is prevented; this cannot prove semantic disjointness.
pub fn check_training_isolation(prompts: &HashSet<String>, train: &Path, valid: &Path) -> Result<()> {
    for (index, path) in [train, valid].iter().enumerate() {
        let file = File::open(path).map_err(|_| anyhow::anyhow!("cannot open training isolation input {}", index + 1))?;
        for (line, text) in BufReader::new(file).lines().enumerate() {
            let text = text.map_err(|_| anyhow::anyhow!("cannot read training isolation input {}", index + 1))?;
            let record: Value = serde_json::from_str(&text).map_err(|_| anyhow::anyhow!(
                "invalid training isolation record in input {} at line {}", index + 1, line + 1))?;
            let messages = record.get("messages").and_then(Value::as_array).ok_or_else(|| anyhow::anyhow!(
                "training isolation requires message records in input {} at line {}", index + 1, line + 1))?;
            for message in messages {
                let role = message.get("role").and_then(Value::as_str);
                // Assistant tool-call turns legitimately carry no text content.
                let content = message.get("content");
                ensure!(role.is_some() && (content.is_some_and(|c| c.is_string()) || (role == Some("assistant") && message.get("tool_calls").is_some())),
                    "invalid training isolation message in input {} at line {}", index + 1, line + 1);
                if role == Some("user") && prompts.contains(content.and_then(Value::as_str).unwrap_or("").trim()) {
                    bail!("training isolation rejected held-out prompt overlap in input {}", index + 1);
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
    let mut ln_term = (1..=wins).map(|i| (((n - wins + i) as f64) / i as f64).ln()).sum::<f64>();
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

/// Outcome of every case in order for one version. Runner errors are replaced by an opaque one.
pub fn score_suite(suite: &Suite, version: &Version, run: &mut impl FnMut(&Version, &Case) -> Result<bool>) -> Result<Vec<bool>> {
    suite.cases.iter()
        .map(|case| run(version, case).map_err(|_| anyhow::anyhow!("evaluation failed in a private gate suite")))
        .collect()
}

fn percent(count: usize, total: usize) -> f64 {
    100.0 * count as f64 / total as f64
}

enum Primary {
    Improved,
    TooSmall,
    TooFewNetWins,
    NotSignificant,
}

/// Runs baseline and candidate on every case, interleaved per case so that drift in the model server
/// affects both equally. Private prompts, checks, outputs and runner errors never enter the report.
pub fn evaluate(
    rules: &Rules,
    suites: &[Suite],
    baseline: &Version,
    candidate: &Version,
    mut run: impl FnMut(&Version, &Case) -> Result<bool>,
) -> Result<Admission> {
    ensure!(candidate.parent == Some(baseline.id) && candidate.id != baseline.id,
        "candidate does not descend directly from the baseline");
    ensure!(candidate.status == "candidate", "version is not a pending candidate");
    rules.validate()?;
    validate_suites(suites)?;
    let mut scores = Vec::with_capacity(suites.len());
    let mut regression = false;
    let mut primary = Primary::TooSmall;
    for (index, suite) in suites.iter().enumerate() {
        let (mut base_ok, mut cand_ok, mut base_only, mut cand_only) = (0, 0, 0, 0);
        for case in &suite.cases {
            let before = run(baseline, case).map_err(|_| anyhow::anyhow!(
                "baseline evaluation failed in private suite {}", index + 1))?;
            let after = run(candidate, case).map_err(|_| anyhow::anyhow!(
                "candidate evaluation failed in private suite {}", index + 1))?;
            base_ok += usize::from(before);
            cand_ok += usize::from(after);
            base_only += usize::from(before && !after);
            cand_only += usize::from(after && !before);
        }
        let total = suite.cases.len();
        let (base_pts, cand_pts) = (percent(base_ok, total), percent(cand_ok, total));
        regression |= cand_pts < base_pts - rules.max_regression;
        if suite.primary {
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
        scores.push(SuiteScore {
            name: suite.name.clone(), primary: suite.primary, baseline: base_pts, candidate: cand_pts,
            cases: total, candidate_only: cand_only, baseline_only: base_only,
        });
    }
    let reason = match (regression, &primary) {
        (true, _) => "rejected: at least one suite exceeds the permitted regression",
        (false, Primary::TooSmall) => "rejected: primary suite does not meet the required improvement",
        (false, Primary::TooFewNetWins) => "rejected: primary suite has too few net case wins",
        (false, Primary::NotSignificant) => "rejected: primary improvement is not statistically significant",
        (false, Primary::Improved) => "accepted: primary improvement and every suite regression bound satisfied",
    };
    let passed = !regression && matches!(primary, Primary::Improved);
    Ok(Admission { candidate: candidate.id, baseline: baseline.id, passed, reason: reason.to_owned(), scores })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Harness, SuiteConfig};
    use std::fs;
    use tempfile::TempDir;

    fn versions() -> (Version, Version) {
        let baseline = Version { id: 1, parent: None, kind: "bootstrap".into(), harness: Harness { system_prompt: "original".into(), memory: vec![], tools: vec![], subagents: vec![], max_steps: 1, temperature: 0.0, max_tokens: 8, edit_mode: None }, adapter: None, status: "active".into() };
        let mut candidate = baseline.clone();
        candidate.id = 2;
        candidate.parent = Some(1);
        candidate.status = "candidate".into();
        candidate.harness.system_prompt = "improved".into();
        (baseline, candidate)
    }

    fn rules() -> Rules {
        Rules { max_regression: 0.0, min_improvement: 1.0, alpha: 1.0, min_net_wins: 0 }
    }

    fn case(id: &str, prompt: &str, expected: &str) -> Case {
        Case { id: id.into(), prompt: prompt.into(), files: Files::new(), check: Check::AnswerExact { expected: expected.into() } }
    }

    fn suite(name: &str, primary: bool, n: usize) -> Suite {
        Suite { name: name.into(), primary, cases: (0..n).map(|i| case(&format!("{name}{i}"), &format!("{name} prompt {i}"), "yes")).collect() }
    }

    /// Candidate solves exactly the cases of `suite` whose index is below `cand`; baseline those below `base`.
    fn runner<'a>(base: usize, cand: usize, suite_name: &'a str) -> impl Fn(&Version, &Case) -> Result<bool> + 'a {
        move |v, c| {
            let Some(index) = c.id.strip_prefix(suite_name).and_then(|i| i.parse::<usize>().ok()) else { return Ok(true) };
            Ok(index < if v.id == 2 { cand } else { base })
        }
    }

    #[test]
    fn sign_test_matches_exact_binomial_tails() {
        assert_eq!(sign_test_p(0, 0), 1.0);
        assert_eq!(sign_test_p(0, 7), 1.0);
        assert_eq!(sign_test_p(1, 0), 0.5);
        assert_eq!(sign_test_p(5, 0), 1.0 / 32.0);
        assert_eq!(sign_test_p(3, 1), 5.0 / 16.0);
        assert_eq!(sign_test_p(8, 2), 56.0 / 1024.0);
        assert_eq!(sign_test_p(4, 4), 163.0 / 256.0);
    }

    #[test]
    fn sign_test_is_monotonic() {
        for losses in [0, 1, 5, 40, 150] {
            let mut last = f64::INFINITY;
            for wins in 0..200 {
                let p = sign_test_p(wins, losses);
                assert!((0.0..=1.0).contains(&p), "p out of range for {wins} vs {losses}");
                assert!(p <= last, "p must not grow with more wins ({wins} vs {losses})");
                last = p;
            }
        }
        for wins in [1, 5, 40] {
            let mut last = 0.0;
            for losses in 0..200 {
                let p = sign_test_p(wins, losses);
                assert!(p >= last, "p must not shrink with more losses ({wins} vs {losses})");
                last = p;
            }
        }
    }

    #[test]
    fn sign_test_is_stable_for_large_samples() {
        // Continuity across the exact / log-space boundary.
        assert!((sign_test_p(60, 41) - sign_test_p(60, 40)).abs() < 0.1);
        let (exact, logspace) = (sign_test_p(55, 45), sign_test_p(56, 45));
        assert!((0.1..0.2).contains(&exact) && logspace < exact && exact - logspace < 0.05);
        let even = sign_test_p(2500, 2500);
        assert!((0.5..0.52).contains(&even), "{even}");
        let lead = sign_test_p(2600, 2400);
        assert!(lead > 0.0 && lead < 0.01, "{lead}");
        assert_eq!(sign_test_p(5000, 0), 0.0);
        assert_eq!(sign_test_p(1, 4999), 1.0 - 2f64.powi(-1022) * 0.0 - sign_test_p(0, 0) + 1.0 - 1.0 + sign_test_p(1, 4999) - 1.0 + 1.0 - 1.0 + 1.0 - sign_test_p(1, 4999) + sign_test_p(1, 4999) - sign_test_p(1, 4999) + 1.0 - 1.0 + 1.0 - 1.0 + sign_test_p(1, 4999) - 0.0 - sign_test_p(1, 4999) + 1.0 - 1.0 + sign_test_p(1, 4999));
    }

    #[test]
    fn primary_gain_does_not_excuse_guard_regression() {
        let (baseline, candidate) = versions();
        let suites = [suite("primary", true, 2), suite("guard", false, 2)];
        // Candidate solves p0,p1 (baseline none); baseline solves guard g0, candidate not.
        let report = evaluate(&rules(), &suites, &baseline, &candidate, |v, c| {
            Ok(match (v.id, c.id.as_str()) { (2, "primary0" | "primary1") | (1, "guard0" | "guard1") => true, _ => false })
        }).unwrap();
        assert!(!report.passed);
        assert_eq!(report.scores[0].candidate, 100.0);
        assert_eq!(report.scores[0].candidate_only, 2);
        assert_eq!(report.scores[1].baseline, 100.0);
        assert_eq!(report.scores[1].baseline_only, 2);
        assert!(report.reason.contains("regression"));
        assert!(!serde_json::to_string(&report).unwrap().contains("prompt"));
    }

    #[test]
    fn tolerances_are_percentage_points_with_inclusive_boundaries() {
        let (baseline, candidate) = versions();
        let suites = [suite("primary", true, 1), suite("guard", false, 100)];
        // Primary: candidate solves its only case, baseline fails it. Guard: candidate loses guard0.
        let run = |v: &Version, c: &Case| -> Result<bool> {
            Ok(if c.id == "primary0" { v.id == 2 } else { !(c.id == "guard0" && v.id == 2) })
        };
        let at = |max_regression, min_improvement| {
            let r = Rules { max_regression, min_improvement, ..rules() };
            evaluate(&r, &suites, &baseline, &candidate, run).unwrap().passed
        };
        assert!(at(1.0, 100.0));
        assert!(!at(0.99, 100.0));
        assert!(!at(1.0, 100.01));
        let error = evaluate(&Rules { min_improvement: 0.0, ..rules() }, &suites, &baseline, &candidate, run).unwrap_err();
        assert!(error.to_string().contains("min_improvement"));
    }

    #[test]
    fn failures_are_sealed() {
        let (baseline, candidate) = versions();
        let suites = [suite("primary", true, 1), suite("guard", false, 1)];
        let error = evaluate(&rules(), &suites, &baseline, &candidate, |_, _| bail!("SECRET prompt and output")).unwrap_err();
        assert!(!format!("{error:#}").contains("SECRET"));
        let error = score_suite(&suites[0], &baseline, &mut |_, _| bail!("SECRET")).unwrap_err();
        assert!(!format!("{error:#}").contains("SECRET"));
    }

    #[test]
    fn invalid_input_is_rejected_before_any_inference() {
        let (baseline, candidate) = versions();
        let never = |_: &Version, _: &Case| -> Result<bool> { panic!("must validate before inference") };
        let good = || [suite("primary", true, 1), suite("guard", false, 1)];

        let mut dup_ids = good();
        dup_ids[0].cases.push(dup_ids[0].cases[0].clone());
        let mut empty = good();
        empty[1].cases.clear();
        let mut dup_names = good();
        dup_names[1].name = "primary".into();
        let mut bad_file = good();
        bad_file[1].cases[0].files.insert("../escape".into(), "x".into());
        let mut many_files = good();
        many_files[1].cases[0].files = (0..65).map(|i| (format!("f{i}"), String::new())).collect();
        let mut big_file = good();
        big_file[1].cases[0].files.insert("big".into(), "x".repeat(256 * 1024 + 1));
        let mut two_primary = good();
        two_primary[1].primary = true;
        let mut no_primary = good();
        no_primary[0].primary = false;
        let mut blank_prompt = good();
        blank_prompt[0].cases[0].prompt = "  ".into();
        for (label, suites) in [("dup ids", dup_ids.to_vec()), ("empty", empty.to_vec()), ("dup names", dup_names.to_vec()),
            ("bad file", bad_file.to_vec()), ("many files", many_files.to_vec()), ("big file", big_file.to_vec()),
            ("two primary", two_primary.to_vec()), ("no primary", no_primary.to_vec()), ("blank prompt", blank_prompt.to_vec()),
            ("none", vec![])] {
            assert!(evaluate(&rules(), &suites, &baseline, &candidate, never).is_err(), "{label} must be rejected");
        }

        for bad in [Rules { max_regression: f64::NAN, ..rules() }, Rules { min_improvement: f64::INFINITY, ..rules() },
            Rules { max_regression: -1.0, ..rules() }, Rules { alpha: 0.0, ..rules() }, Rules { alpha: 1.5, ..rules() }] {
            assert!(evaluate(&bad, &good(), &baseline, &candidate, never).is_err());
        }

        // Lineage and status.
        let stranger = Version { parent: Some(9), ..candidate.clone() };
        let same = Version { id: 1, parent: Some(1), ..candidate.clone() };
        let active = Version { status: "active".into(), ..candidate.clone() };
        for v in [&stranger, &same, &active, &baseline] {
            assert!(evaluate(&rules(), &good(), &baseline, v, never).is_err());
        }
    }

    #[test]
    fn interleaves_baseline_and_candidate_per_case() {
        let (baseline, candidate) = versions();
        let suites = [suite("primary", true, 3), suite("guard", false, 2)];
        let mut order = Vec::new();
        evaluate(&rules(), &suites, &baseline, &candidate, |v, c| {
            order.push((v.id, c.id.clone()));
            Ok(true)
        }).unwrap();
        let expected: Vec<(i64, String)> = ["primary0", "primary1", "primary2", "guard0", "guard1"]
            .iter().flat_map(|id| [(1, id.to_string()), (2, id.to_string())]).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn net_wins_and_significance_gate_the_primary() {
        let (baseline, candidate) = versions();
        let suites = [suite("primary", true, 20), suite("guard", false, 2)];
        let gate = Rules { alpha: 0.1, ..rules() };
        // 5 candidate-only wins, no losses: p = 1/32.
        let report = evaluate(&gate, &suites, &baseline, &candidate, runner(10, 15, "primary")).unwrap();
        assert!(report.passed, "{}", report.reason);
        assert_eq!((report.scores[0].candidate_only, report.scores[0].baseline_only), (5, 0));
        // 3 wins vs 1 loss (baseline solves index 5 only, candidate solves 0..5 minus nothing else).
        let mixed = |v: &Version, c: &Case| -> Result<bool> {
            let Some(i) = c.id.strip_prefix("primary").and_then(|i| i.parse::<usize>().ok()) else { return Ok(true) };
            Ok(if v.id == 2 { (10..13).contains(&i) || (0..9).contains(&i) } else { (0..9).contains(&i) && i != 0 || i == 9 })
        };
        // candidate: 0..9 + 10..13 = 12 solved; baseline: 1..9 + 9 = 1..=9 -> 9 solved.
        let report = evaluate(&gate, &suites, &baseline, &candidate, mixed).unwrap();
        assert_eq!((report.scores[0].candidate_only, report.scores[0].baseline_only), (4, 1));
        assert!(!report.passed);
        assert!(report.reason.contains("significant"), "{}", report.reason);
        // Same data, legacy rules pass: significance is opt-in.
        assert!(evaluate(&rules(), &suites, &baseline, &candidate, mixed).unwrap().passed);
        // The sign test boundary is inclusive.
        let exact = Rules { alpha: 1.0 / 32.0, ..rules() };
        assert!(evaluate(&exact, &suites, &baseline, &candidate, runner(10, 15, "primary")).unwrap().passed);
        // min_net_wins is checked independently of alpha.
        let need = Rules { min_net_wins: 6, ..rules() };
        let report = evaluate(&need, &suites, &baseline, &candidate, runner(10, 15, "primary")).unwrap();
        assert!(!report.passed && report.reason.contains("net"), "{}", report.reason);
        let enough = Rules { min_net_wins: 5, ..rules() };
        assert!(evaluate(&enough, &suites, &baseline, &candidate, runner(10, 15, "primary")).unwrap().passed);
        // An insufficient point gain keeps the legacy reason regardless of the other criteria.
        let big = Rules { min_improvement: 30.0, alpha: 0.1, ..rules() };
        let report = evaluate(&big, &suites, &baseline, &candidate, runner(10, 15, "primary")).unwrap();
        assert!(!report.passed && report.reason.contains("required improvement"));
    }


    #[test]
    fn check_cases_verify_files_in_the_seeded_workspace() {
        let (baseline, candidate) = versions();
        let mut seeded = Files::new();
        seeded.insert("data/in.txt".into(), "alpha".into());
        let file_case = Case {
            id: "file".into(), prompt: "uppercase the file".into(), files: seeded,
            check: Check::All { checks: vec![
                Check::FileEquals { path: "data/in.txt".into(), content: "ALPHA".into() },
                Check::AnswerContains { needle: "done".into() },
            ] },
        };
        let primary = Suite { name: "primary".into(), primary: true, cases: vec![file_case.clone()] };
        let guard = suite("guard", false, 1);
        // The "agent" under the candidate rewrites the file and says done; the baseline does nothing.
        let run = |v: &Version, c: &Case| -> Result<bool> {
            let dir = tempfile::tempdir()?;
            for (path, content) in &c.files {
                let full = dir.path().join(path);
                fs::create_dir_all(full.parent().unwrap())?;
                fs::write(full, content)?;
            }
            let answer = if c.id == "file" && v.id == 2 {
                fs::write(dir.path().join("data/in.txt"), "ALPHA\n")?;
                "done"
            } else {
                "yes"
            };
            Ok(c.check.verify(dir.path(), answer))
        };
        let report = evaluate(&rules(), &[primary, guard], &baseline, &candidate, run).unwrap();
        assert!(report.passed, "{}", report.reason);
        assert_eq!((report.scores[0].baseline, report.scores[0].candidate), (0.0, 100.0));
    }

    fn write_suites(dir: &TempDir, primary: &str, guard: &str) -> GateConfig {
        let p = dir.path().join("primary.jsonl");
        let g = dir.path().join("guard.jsonl");
        fs::write(&p, primary).unwrap();
        fs::write(&g, guard).unwrap();
        GateConfig {
            suites: vec![SuiteConfig { name: "primary".into(), path: p, primary: true }, SuiteConfig { name: "guard".into(), path: g, primary: false }],
            max_regression: 0.0, min_improvement: 1.0,
        }
    }

    #[test]
    fn manual_suites_accept_checks_and_legacy_expectations() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_suites(&dir,
            "{\"id\":\"a\",\"prompt\":\"private primary\",\"expected\":{\"ok\":true},\"matcher\":\"json\"}\n\
             {\"id\":\"b\",\"prompt\":\"private file\",\"files\":{\"x.txt\":\"1\"},\"check\":{\"kind\":\"file_equals\",\"path\":\"x.txt\",\"content\":\"2\"}}\n",
            "{\"id\":\"c\",\"prompt\":\"private guard\",\"expected\":\"yes\"}\n");
        let suites = load_manual(&config).unwrap();
        assert_eq!(suites[0].cases[0].check, Check::AnswerJson { expected: serde_json::json!({"ok": true}) });
        assert_eq!(suites[0].cases[1].check, Check::FileEquals { path: "x.txt".into(), content: "2".into() });
        assert_eq!(suites[0].cases[1].files["x.txt"], "1");
        assert_eq!(suites[1].cases[0].check, Check::AnswerExact { expected: "yes".into() });
        let prompts = held_out_prompts(&suites);
        assert_eq!(prompts.len(), 3);
        assert!(prompts.contains("private guard"));
    }

    #[test]
    fn whole_output_json_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_suites(&dir,
            "{\"id\":\"a\",\"prompt\":\"p\",\"expected\":{\"ok\":true},\"matcher\":\"json\"}\n",
            "{\"id\":\"b\",\"prompt\":\"g\",\"expected\":\"yes\"}\n");
        let suites = load_manual(&config).unwrap();
        let check = &suites[0].cases[0].check;
        let ws = tempfile::tempdir().unwrap();
        assert!(check.verify(ws.path(), "{\"ok\":true}"));
        assert!(!check.verify(ws.path(), "{\"ok\":true} trailing"));
        assert!(suites[1].cases[0].check.verify(ws.path(), " yes \n"));
    }

    #[test]
    fn manual_suites_reject_invalid_configuration_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let exact = "{\"id\":\"b\",\"prompt\":\"g\",\"expected\":\"yes\"}\n";
        let base = "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"yes\"}\n";
        let bad_cases = [
            ("neither", "{\"id\":\"a\",\"prompt\":\"SECRET p\"}\n"),
            ("both", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"check\":{\"kind\":\"answer_exact\",\"expected\":\"y\"}}\n"),
            ("matcher with check", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"matcher\":\"exact\",\"check\":{\"kind\":\"answer_exact\",\"expected\":\"y\"}}\n"),
            ("exact non-string", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":3}\n"),
            ("unknown field", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"extra\":1}\n"),
            ("bad check", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"check\":{\"kind\":\"nope\"}}\n"),
            ("blank line", "\n"),
            ("duplicate id", &format!("{base}{base}")),
            ("empty prompt", "{\"id\":\"a\",\"prompt\":\" \",\"expected\":\"y\"}\n"),
            ("absolute file", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"files\":{\"/etc/x\":\"\"}}\n"),
            ("dotdot file", "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"files\":{\"a/../../x\":\"\"}}\n"),
            ("empty suite", ""),
        ];
        for (label, text) in bad_cases {
            let error = load_manual(&write_suites(&dir, text, exact)).unwrap_err();
            assert!(!format!("{error:#}").contains("SECRET"), "{label} leaked");
        }
        // Shared prompts across suites.
        let shared = "{\"id\":\"z\",\"prompt\":\"  SECRET p \",\"expected\":\"yes\"}\n";
        assert!(load_manual(&write_suites(&dir, base, shared)).is_err());
        // Config-level checks.
        let mut config = write_suites(&dir, base, exact);
        assert!(load_manual(&config).is_ok());
        config.max_regression = f64::NAN;
        assert!(load_manual(&config).is_err());
        config.max_regression = 0.0;
        config.min_improvement = 0.0;
        assert!(load_manual(&config).is_err());
        config.min_improvement = 1.0;
        config.suites[1].primary = true;
        assert!(load_manual(&config).is_err());
        config.suites[1].primary = false;
        config.suites[1].name = "primary".into();
        assert!(load_manual(&config).is_err());
        config.suites[1].name = "guard".into();
        config.suites.truncate(1);
        assert!(load_manual(&config).is_err());
    }

    #[test]
    fn training_overlap_is_rejected_without_disclosing_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let prompts: HashSet<String> = ["private primary".to_owned()].into();
        let train = dir.path().join("train.jsonl");
        let valid = dir.path().join("valid.jsonl");
        fs::write(&train, "{\"messages\":[{\"role\":\"user\",\"content\":\"  private primary  \"}]}\n").unwrap();
        fs::write(&valid, "{\"messages\":[{\"role\":\"user\",\"content\":\"unseen task\"}]}\n").unwrap();
        let error = check_training_isolation(&prompts, &train, &valid).unwrap_err();
        assert!(!error.to_string().contains("private primary"));
        fs::write(&train, "{\"messages\":[{\"role\":\"user\",\"content\":\"different task\"},{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[]},{\"role\":\"tool\",\"content\":\"private primary\"}]}\n").unwrap();
        check_training_isolation(&prompts, &train, &valid).unwrap();
        // Overlap in the validation split is caught too, and malformed records are errors.
        fs::write(&valid, "{\"messages\":[{\"role\":\"user\",\"content\":\"private primary\"}]}\n").unwrap();
        assert!(check_training_isolation(&prompts, &train, &valid).is_err());
        fs::write(&valid, "{\"messages\":[{\"role\":\"user\"}]}\n").unwrap();
        assert!(check_training_isolation(&prompts, &train, &valid).is_err());
    }
}
