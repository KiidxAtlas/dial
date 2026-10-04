//! Reproducible, local benchmark suite parsing, workspace isolation, and scoring.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Component, Path, PathBuf}, time::Instant};

pub const MAX_CASES: usize = 1_000;
pub const MAX_CHECKS_PER_CASE: usize = 100;
pub const MAX_TURNS_PER_CASE: usize = 100;
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PROFILES: usize = 64;
const EXCLUDED_DIRS: &[&str] = &[".git", ".dial", ".codegraph", ".ssh", ".aws", ".gnupg", ".kube", "secrets", ".secrets", "credentials", "target", "build", "dist", ".cache", ".venv", "venv", ".tox", "__pycache__", "node_modules", "vendor"];
const EXCLUDED_FILES: &[&str] = &[".env", ".env.local", ".env.production", ".netrc", ".npmrc", ".pypirc", ".git-credentials", "credentials.json", "secrets.json", "secrets.yaml", "secrets.yml", "id_rsa", "id_ed25519"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSuite {
    pub version: u32,
    pub name: String,
    pub cases: Vec<BenchmarkCase>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkCase {
    pub id: String,
    /// Optional per-case source directory, relative to the root passed to `run`.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Input is provided to the parent agent callback and is never copied into reports.
    pub prompt: String,
    /// Seeded conversation context; runners must preserve its order and role.
    #[serde(default)]
    pub context_messages: Vec<String>,
    /// Follow-up requests execute in the same agent/session as `prompt`.
    #[serde(default)]
    pub turns: Vec<BenchmarkTurn>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default, alias = "assertions")]
    pub checks: Vec<Assertion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkTurn {
    pub prompt: String,
    #[serde(default)]
    pub compact_before: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservedMetric {
    ToolCalls,
    ToolErrors,
    ToolContractErrors,
    Compactions,
    ArtifactsRetrieved,
    DynamicToolsUsed,
    TruncatedRecoveries,
    TurnsCompleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Assertion {
    FileExists { path: PathBuf },
    FileContains { path: PathBuf, needle: String },
    FileEquals { path: PathBuf, content: String },
    Command { command: String, #[serde(default)] expect_stdout: Option<String>, #[serde(default)] expect_exit: i32 },
    Metric { metric: ObservedMetric, #[serde(default)] min: Option<u64>, #[serde(default)] max: Option<u64> },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMetrics {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    #[serde(default)]
    pub agent_succeeded: bool,
    #[serde(default)]
    pub agent_error: bool,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub tool_contract_errors: u64,
    #[serde(default)]
    pub compactions: u64,
    #[serde(default)]
    pub artifacts_retrieved: u64,
    #[serde(default)]
    pub dynamic_tools_used: u64,
    #[serde(default)]
    pub truncated_recoveries: u64,
    #[serde(default)]
    pub turns_completed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseResult {
    pub id: String,
    pub passed: bool,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub metrics: RunMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkReport {
    pub suite: String,
    pub cases: Vec<CaseResult>,
    pub pass_rate: f64,
    pub average_latency_ms: f64,
    pub total_tokens: u64,
    pub total_tool_calls: u64,
    pub total_tool_errors: u64,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BaselineComparison {
    pub score_delta: f64,
    pub pass_rate_delta: f64,
    pub regressed: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegressionThresholds {

    pub max_latency_increase_ms: Option<u64>,
    pub max_prompt_token_increase: Option<u64>,
    pub max_output_token_increase: Option<u64>,
    pub max_tool_call_increase: Option<u64>,
    pub max_tool_error_increase: Option<u64>,
}

impl RunMetrics {
    pub fn observed(&self, metric: ObservedMetric) -> u64 {
        match metric {
            ObservedMetric::ToolCalls => self.tool_calls,
            ObservedMetric::ToolErrors => self.tool_errors,
            ObservedMetric::ToolContractErrors => self.tool_contract_errors,
            ObservedMetric::Compactions => self.compactions,
            ObservedMetric::ArtifactsRetrieved => self.artifacts_retrieved,
            ObservedMetric::DynamicToolsUsed => self.dynamic_tools_used,
            ObservedMetric::TruncatedRecoveries => self.truncated_recoveries,
            ObservedMetric::TurnsCompleted => self.turns_completed,
        }
    }
}

/// Names identify actual runner-selected model configurations, not simulated results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkProfile {
    pub name: String,
    #[serde(default)]
    pub ablations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedMetrics {
    pub case_count: usize,
    pub verified_success_rate: f64,
    pub check_pass_rate: f64,
    pub successful_latency_ms: Option<f64>,
    pub prompt_tokens_per_case: f64,
    pub output_tokens_per_case: f64,
    pub cached_tokens_per_case: f64,
    pub reasoning_tokens_per_case: f64,
    pub tool_calls_per_case: f64,
    pub tool_errors_per_case: f64,
    pub tool_contract_errors_per_case: f64,
    pub compactions_per_case: f64,
    pub artifacts_retrieved_per_case: f64,
    pub dynamic_tools_used_per_case: f64,
    pub truncated_recoveries_per_case: f64,
    pub latency_relative_to_baseline: Option<f64>,
    pub tokens_relative_to_baseline: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileReport {
    pub profile: BenchmarkProfile,
    pub report: BenchmarkReport,
    pub normalized: NormalizedMetrics,
    pub categories: std::collections::BTreeMap<String, NormalizedMetrics>,
    pub baseline_comparison: BaselineComparison,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileComparisonReport {
    pub suite: String,
    pub baseline_profile: String,
    /// Sorted by verified success, then successful-case latency; never latency first.
    pub profiles: Vec<ProfileReport>,
}

impl BenchmarkSuite {
    pub fn parse_toml(input: &str) -> Result<Self> {
        let suite: Self = toml::from_str(input).context("parsing benchmark suite")?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "benchmark suite name cannot be empty");
        ensure!(self.version == 1, "unsupported benchmark suite version {}", self.version);
        ensure!(!self.cases.is_empty() && self.cases.len() <= MAX_CASES, "benchmark suite must contain 1..={MAX_CASES} cases");
        let mut ids = std::collections::BTreeSet::new();
        for case in &self.cases {
            ensure!(!case.id.trim().is_empty() && ids.insert(&case.id), "benchmark case IDs must be non-empty and unique");
            ensure!(case.checks.len() <= MAX_CHECKS_PER_CASE, "case {} exceeds {MAX_CHECKS_PER_CASE} checks", case.id);
            ensure!(!case.checks.is_empty(), "case {} must contain at least one deterministic check", case.id);
            ensure!(!case.prompt.trim().is_empty(), "case {} prompt cannot be empty", case.id);
            ensure!(case.turns.len() <= MAX_TURNS_PER_CASE, "case {} exceeds {MAX_TURNS_PER_CASE} follow-up turns", case.id);
            ensure!(case.turns.iter().all(|turn| !turn.prompt.trim().is_empty()), "case {} has an empty follow-up prompt", case.id);
            let bytes = case.context_messages.iter().map(String::len)
                .chain(std::iter::once(case.prompt.len()))
                .chain(case.turns.iter().map(|turn| turn.prompt.len()))
                .fold(0usize, usize::saturating_add);
            ensure!(bytes <= MAX_CONTEXT_BYTES, "case {} exceeds {MAX_CONTEXT_BYTES} input bytes", case.id);
            let mut categories = std::collections::BTreeSet::new();
            ensure!(case.categories.iter().all(|category| !category.trim().is_empty() && categories.insert(category)), "case {} categories must be non-empty and unique", case.id);
            if let Some(project) = &case.project { validate_relative(project).context("invalid case project path")?; }
            for assertion in &case.checks {
                match assertion {
                    Assertion::FileExists { path } | Assertion::FileContains { path, .. } | Assertion::FileEquals { path, .. } => validate_relative(path).context("invalid assertion path")?,
                    Assertion::Command { command, .. } => ensure!(!command.trim().is_empty(), "command assertion cannot be empty"),
                    Assertion::Metric { min, max, .. } => {
                        ensure!(min.is_some() || max.is_some(), "metric assertion needs a bound");
                        ensure!((*min).zip(*max).is_none_or(|(min, max)| min <= max), "metric minimum exceeds maximum");
                    }
                }
            }
        }
        Ok(())
    }

    /// Run cases with parent-supplied execution and verifier callbacks. Agent and verifier
    /// failures are recorded per case rather than treated as a passing result.
    pub fn run<A, C>(&self, source_root: &Path, mut agent: A, mut command_check: C) -> Result<BenchmarkReport>
    where
        A: FnMut(&BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
    {
        self.validate()?;
        let mut results = Vec::with_capacity(self.cases.len());
        for case in &self.cases {
            let source = match &case.project {
                Some(relative) => confined_existing(source_root, relative)?,
                None => source_root.to_path_buf(),
            };
            let workspace = clone_project(&source)?;
            let started = Instant::now();
            let mut metrics = match agent(case, workspace.path()) {
                Ok(metrics) => metrics,
                Err(_) => RunMetrics { agent_error: true, ..RunMetrics::default() },
            };
            if metrics.elapsed_ms == 0 { metrics.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64; }
            let mut checks_passed = 0;
            for assertion in &case.checks {
                let passed = match assertion {
                    Assertion::FileExists { path } => confined_existing(workspace.path(), path).is_ok_and(|path| path.is_file()),
                    Assertion::FileContains { path, needle } => confined_existing(workspace.path(), path).ok().and_then(|p| fs::read_to_string(p).ok()).is_some_and(|s| s.contains(needle)),
                    Assertion::FileEquals { path, content } => confined_existing(workspace.path(), path).ok().and_then(|p| fs::read_to_string(p).ok()).is_some_and(|s| s.trim_end() == content.trim_end()),
                    Assertion::Command { command, expect_stdout, expect_exit } => command_check(&CommandCheck { command, expect_stdout: expect_stdout.as_deref(), expect_exit: *expect_exit }, workspace.path()).unwrap_or(false),
                    Assertion::Metric { metric, min, max } => {
                        let value = metrics.observed(*metric);
                        min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
                    }
                };
                checks_passed += usize::from(passed);
            }
            let completed_turns = case.turns.is_empty() || metrics.turns_completed == (case.turns.len() + 1) as u64;
            let forced_compactions = case.turns.iter().filter(|turn| turn.compact_before).count() as u64;
            let passed = metrics.agent_succeeded && !metrics.agent_error && completed_turns
                && metrics.compactions >= forced_compactions && checks_passed == case.checks.len();
            results.push(CaseResult { id: case.id.clone(), passed, checks_passed, checks_total: case.checks.len(), metrics });
        }
        Ok(score_report(&self.name, results))
    }

    /// Each profile gets a fresh workspace for every case. The runner must select
    /// the named real configuration and apply each requested ablation or fail.
    pub fn run_profiles<A, C>(&self, source_root: &Path, profiles: &[BenchmarkProfile], mut agent: A, mut command_check: C) -> Result<ProfileComparisonReport>
    where
        A: FnMut(&BenchmarkProfile, &BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
    {
        ensure!(!profiles.is_empty() && profiles.len() <= MAX_PROFILES, "benchmark needs 1..={MAX_PROFILES} profiles");
        let mut names = std::collections::BTreeSet::new();
        for profile in profiles {
            ensure!(!profile.name.trim().is_empty() && names.insert(&profile.name), "profile names must be non-empty and unique");
            let mut ablations = std::collections::BTreeSet::new();
            ensure!(profile.ablations.iter().all(|item| !item.trim().is_empty() && ablations.insert(item)), "ablations must be non-empty and unique");
        }
        let mut reports = Vec::with_capacity(profiles.len());
        for profile in profiles {
            let report = self.run(source_root, |case, workspace| agent(profile, case, workspace), &mut command_check)?;
            let normalized = normalize(&report, None);
            let categories = category_metrics(self, &report);
            reports.push(ProfileReport { profile: profile.clone(), report, normalized, categories, baseline_comparison: BaselineComparison { score_delta: 0.0, pass_rate_delta: 0.0, regressed: false } });
        }
        let (baseline, others) = reports.split_first_mut().expect("validated non-empty profiles");
        baseline.normalized = normalize(&baseline.report, Some(&baseline.report));
        for report in others {
            report.normalized = normalize_cases(report.report.cases.iter(), Some(&baseline.normalized));
            report.baseline_comparison = compare_baseline(&report.report, &baseline.report);
        }
        reports.sort_by(|a, b| {
            b.normalized.verified_success_rate.total_cmp(&a.normalized.verified_success_rate)
                .then_with(|| a.normalized.successful_latency_ms.unwrap_or(f64::INFINITY).total_cmp(&b.normalized.successful_latency_ms.unwrap_or(f64::INFINITY)))
                .then_with(|| a.profile.name.cmp(&b.profile.name))
        });
        Ok(ProfileComparisonReport { suite: self.name.clone(), baseline_profile: profiles[0].name.clone(), profiles: reports })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommandCheck<'a> {
    pub command: &'a str,
    pub expect_stdout: Option<&'a str>,
    pub expect_exit: i32,
}

/// Copy a source tree into a unique temporary directory, omitting generated/state directories.
/// Symlinks are materialized only when their canonical targets remain inside the source root.
pub fn clone_project(source: &Path) -> Result<tempfile::TempDir> {
    let source = fs::canonicalize(source).with_context(|| format!("resolving benchmark source {}", source.display()))?;
    ensure!(source.is_dir(), "benchmark source must be a directory");
    let destination = tempfile::tempdir().context("creating benchmark workspace")?;
    let mut visited = std::collections::BTreeSet::new();
    copy_directory(&source, destination.path(), &source, &mut visited)?;
    Ok(destination)
}

fn copy_directory(source: &Path, dest: &Path, root: &Path, visited: &mut std::collections::BTreeSet<PathBuf>) -> Result<()> {
    let canonical = fs::canonicalize(source)?;
    ensure!(canonical.starts_with(root), "source symlink escapes project root");
    if !visited.insert(canonical.clone()) { return Ok(()); }
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        if is_excluded_source_path(root, &from) { continue; }
        let name = entry.file_name();
        let to = dest.join(&name);
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            let target = fs::canonicalize(&from).with_context(|| format!("resolving symlink {}", from.display()))?;
            ensure!(target.starts_with(root), "source symlink escapes project root: {}", from.display());
            if is_excluded_source_path(root, &target) { continue; }
            if target.is_dir() { fs::create_dir_all(&to)?; copy_directory(&target, &to, root, visited)?; }
            else { fs::copy(&target, &to).with_context(|| format!("copying {}", from.display()))?; }
        } else if metadata.is_dir() {
            fs::create_dir(&to)?;
            copy_directory(&from, &to, root, visited)?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

fn is_secret_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if EXCLUDED_FILES.contains(&lower.as_str()) { return true; }
    if lower.starts_with(".env.") && !matches!(lower.as_str(), ".env.example" | ".env.sample" | ".env.template") { return true; }
    [".pem", ".p12", ".pfx"].iter().any(|extension| lower.ends_with(extension))
}

fn is_excluded_source_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return true };
    relative.components().any(|component| {
        let Component::Normal(name) = component else { return true };
        let name = name.to_string_lossy();
        EXCLUDED_DIRS.contains(&name.as_ref()) || is_secret_file(&name)
    })
}

fn validate_relative(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty() && path.components().all(|part| matches!(part, Component::Normal(_))), "path must be a non-empty relative path without traversal");
    Ok(())
}

fn confined_existing(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative(relative)?;
    let root = fs::canonicalize(root).context("resolving workspace root")?;
    let mut path = root.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else { bail!("invalid workspace path") };
        path.push(name);
        let metadata = fs::symlink_metadata(&path).context("workspace path does not exist")?;
        ensure!(!metadata.file_type().is_symlink(), "workspace assertion path traverses symlink");
    }
    let resolved = fs::canonicalize(&path)?;
    ensure!(resolved.starts_with(root), "workspace assertion path escapes root");
    Ok(resolved)
}

pub fn score_report(suite: &str, cases: Vec<CaseResult>) -> BenchmarkReport {
    let n = cases.len() as f64;
    let passed = cases.iter().filter(|case| case.passed).count() as f64;
    let pass_rate = if cases.is_empty() { 0.0 } else { passed / n };
    let average_latency_ms = if cases.is_empty() { 0.0 } else { cases.iter().map(|case| case.metrics.elapsed_ms as f64).sum::<f64>() / n };
    let total_tokens = cases.iter().fold(0u64, |sum, case| sum.saturating_add(case.metrics.prompt_tokens).saturating_add(case.metrics.output_tokens));
    let total_tool_calls = cases.iter().fold(0u64, |sum, case| sum.saturating_add(case.metrics.tool_calls));
    let total_tool_errors = cases.iter().fold(0u64, |sum, case| sum.saturating_add(case.metrics.tool_errors));
    BenchmarkReport { suite: suite.to_owned(), cases, pass_rate, average_latency_ms, total_tokens, total_tool_calls, total_tool_errors, score: pass_rate }
}

pub fn compare_baseline(current: &BenchmarkReport, baseline: &BenchmarkReport) -> BaselineComparison {
    compare_baseline_with_thresholds(current, baseline, RegressionThresholds::default())
}

/// Compare aggregate/case outcomes and configured upper bounds on metric growth.
pub fn compare_baseline_with_thresholds(current: &BenchmarkReport, baseline: &BenchmarkReport, thresholds: RegressionThresholds) -> BaselineComparison {
    let score_delta = current.score - baseline.score;
    let pass_rate_delta = current.pass_rate - baseline.pass_rate;
    let current_by_id: std::collections::BTreeMap<_, _> = current.cases.iter().map(|case| (&case.id, case)).collect();
    let failed_previous_case = baseline.cases.iter().filter(|case| case.passed).any(|case| current_by_id.get(&case.id).is_none_or(|now| !now.passed || (case.checks_passed > now.checks_passed)));
    let exceeds = |current: u64, baseline: u64, threshold: Option<u64>| threshold.is_some_and(|max| current.saturating_sub(baseline) > max);
    let metric_regression = exceeds(current.total_tool_calls, baseline.total_tool_calls, thresholds.max_tool_call_increase)
        || exceeds(current.total_tool_errors, baseline.total_tool_errors, thresholds.max_tool_error_increase)
        || exceeds(total_prompt_tokens(current), total_prompt_tokens(baseline), thresholds.max_prompt_token_increase)
        || exceeds(total_output_tokens(current), total_output_tokens(baseline), thresholds.max_output_token_increase)
        || exceeds(current.average_latency_ms.max(0.0) as u64, baseline.average_latency_ms.max(0.0) as u64, thresholds.max_latency_increase_ms);
    BaselineComparison { score_delta, pass_rate_delta, regressed: score_delta < 0.0 || pass_rate_delta < 0.0 || failed_previous_case || metric_regression }
}

fn total_prompt_tokens(report: &BenchmarkReport) -> u64 { report.cases.iter().fold(0u64, |sum, case| sum.saturating_add(case.metrics.prompt_tokens)) }
fn total_output_tokens(report: &BenchmarkReport) -> u64 { report.cases.iter().fold(0u64, |sum, case| sum.saturating_add(case.metrics.output_tokens)) }

pub fn normalize(report: &BenchmarkReport, baseline: Option<&BenchmarkReport>) -> NormalizedMetrics {
    let baseline = baseline.map(|baseline| normalize_cases(baseline.cases.iter(), None));
    normalize_cases(report.cases.iter(), baseline.as_ref())
}

fn normalize_cases<'a>(cases: impl Iterator<Item = &'a CaseResult>, baseline: Option<&NormalizedMetrics>) -> NormalizedMetrics {
    let mut count = 0usize;
    let mut passed = 0usize;
    let mut successful_latency = 0.0;
    let mut checks_total = 0.0;
    let mut checks_passed = 0.0;
    let mut totals = [0.0; 11];
    for case in cases {
        count += 1;
        if case.passed {
            passed += 1;
            successful_latency += case.metrics.elapsed_ms as f64;
        }
        checks_total += case.checks_total as f64;
        checks_passed += case.checks_passed as f64;
        for (total, value) in totals.iter_mut().zip([
            case.metrics.prompt_tokens, case.metrics.output_tokens, case.metrics.cached_tokens,
            case.metrics.reasoning_tokens, case.metrics.tool_calls, case.metrics.tool_errors,
            case.metrics.tool_contract_errors, case.metrics.compactions, case.metrics.artifacts_retrieved,
            case.metrics.dynamic_tools_used, case.metrics.truncated_recoveries,
        ]) {
            *total += value as f64;
        }
    }
    let divisor = count.max(1) as f64;
    let successful_latency_ms = (passed > 0).then(|| successful_latency / passed as f64);
    let ratio = |current: f64, previous: f64| (previous > 0.0).then(|| current / previous);
    let prompt_tokens_per_case = totals[0] / divisor;
    let output_tokens_per_case = totals[1] / divisor;
    NormalizedMetrics {
        case_count: count,
        verified_success_rate: passed as f64 / divisor,
        check_pass_rate: if checks_total > 0.0 { checks_passed / checks_total } else { 0.0 },
        successful_latency_ms,
        prompt_tokens_per_case,
        output_tokens_per_case,
        cached_tokens_per_case: totals[2] / divisor,
        reasoning_tokens_per_case: totals[3] / divisor,
        tool_calls_per_case: totals[4] / divisor,
        tool_errors_per_case: totals[5] / divisor,
        tool_contract_errors_per_case: totals[6] / divisor,
        compactions_per_case: totals[7] / divisor,
        artifacts_retrieved_per_case: totals[8] / divisor,
        dynamic_tools_used_per_case: totals[9] / divisor,
        truncated_recoveries_per_case: totals[10] / divisor,
        latency_relative_to_baseline: baseline.and_then(|baseline| successful_latency_ms.zip(baseline.successful_latency_ms)).and_then(|(now, before)| ratio(now, before)),
        tokens_relative_to_baseline: baseline.and_then(|baseline| ratio(prompt_tokens_per_case + output_tokens_per_case, baseline.prompt_tokens_per_case + baseline.output_tokens_per_case)),
    }
}

fn category_metrics(suite: &BenchmarkSuite, report: &BenchmarkReport) -> std::collections::BTreeMap<String, NormalizedMetrics> {
    let categories: std::collections::BTreeSet<_> = suite.cases.iter().flat_map(|case| case.categories.iter()).collect();
    categories.into_iter().map(|category| {
        let cases = suite.cases.iter().zip(&report.cases)
            .filter(|(case, _)| case.categories.contains(category)).map(|(_, result)| result);
        (category.clone(), normalize_cases(cases, None))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    #[test]
    fn parsing_limits_and_scoring_are_deterministic_and_serializable() {
        let suite = BenchmarkSuite::parse_toml(r#"version = 1
name = "basic"
[[cases]]
id = "one"
prompt = "private"
checks = [{ kind = "file_exists", path = "answer.txt" }]
"#).unwrap();
        assert_eq!(suite.cases[0].id, "one");
        let results = vec![CaseResult { id: "one".into(), passed: true, checks_passed: 1, checks_total: 1, metrics: RunMetrics { prompt_tokens: 4, output_tokens: 8, elapsed_ms: 10, tool_calls: 2, tool_errors: 1, agent_succeeded: true, ..Default::default() } }, CaseResult { id: "two".into(), passed: false, checks_passed: 0, checks_total: 1, metrics: RunMetrics { prompt_tokens: 2, output_tokens: 1, elapsed_ms: 20, tool_calls: 1, tool_errors: 0, ..Default::default() } }];
        let report = score_report("s", results.clone());
        assert_eq!(report, score_report("s", results));
        assert_eq!(report.pass_rate, 0.5);
        assert_eq!(report.average_latency_ms, 15.0);
        assert_eq!((report.total_tokens, report.total_tool_calls, report.total_tool_errors), (15, 3, 1));
        let value = serde_json::to_value(report).unwrap();
        assert!(value.pointer("/cases/0/prompt").is_none());
        assert_eq!(serde_json::from_value::<BenchmarkReport>(value).unwrap().pass_rate, 0.5);
        assert!(BenchmarkSuite::parse_toml("name='x'\n").is_err());
    }

    #[test]
    fn rejects_unknown_versions_and_cases_without_deterministic_checks() {
        let unknown_version = r#"version = 2
name = "unsupported"
[[cases]]
id = "case"
prompt = "do work"
checks = [{ kind = "file_exists", path = "result.txt" }]
"#;
        assert!(BenchmarkSuite::parse_toml(unknown_version).is_err());
        let no_checks = r#"version = 1
name = "unchecked"
[[cases]]
id = "case"
prompt = "do work"
"#;
        assert!(BenchmarkSuite::parse_toml(no_checks).is_err());
    }

    #[test]
    fn failed_checks_never_pass_and_command_checks_are_delegated() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("present"), "ok").unwrap();
        let suite = BenchmarkSuite { version: 1, name: "x".into(), cases: vec![BenchmarkCase { id: "c".into(), project: None, prompt: "do not report".into(), checks: vec![Assertion::FileExists { path: "missing".into() }, Assertion::Command { command: "opaque command".into(), expect_stdout: None, expect_exit: 0 }], ..Default::default() }] };
        let report = suite.run(source.path(), |_, _| Ok(RunMetrics::default()), |check, _| { assert_eq!(check.command, "opaque command"); Ok(false) }).unwrap();
        assert!(!report.cases[0].passed);
        assert_eq!(report.cases[0].checks_passed, 0);
    }
    #[test]
    fn agent_errors_fail_even_when_deterministic_checks_match() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("result.txt"), "correct").unwrap();
        let suite = BenchmarkSuite {
            version: 1,
            name: "agent-error".into(),
            cases: vec![BenchmarkCase {
                id: "case".into(),
                project: None,
                prompt: "produce result".into(),
                checks: vec![Assertion::FileEquals {
                    path: "result.txt".into(),
                    content: "correct".into(),
                }],
                ..Default::default()
            }],
        };
        let report = suite.run(source.path(), |_, _| anyhow::bail!("agent failed"), |_, _| Ok(true)).unwrap();
        assert!(!report.cases[0].passed);
        assert_eq!(report.cases[0].checks_passed, 1);
        assert!(report.cases[0].metrics.agent_error);
        assert!(!report.cases[0].metrics.agent_succeeded);
        let old_metrics = serde_json::json!({"prompt_tokens": 3, "output_tokens": 4, "elapsed_ms": 5, "tool_calls": 0, "tool_errors": 0});
        let parsed: RunMetrics = serde_json::from_value(old_metrics).unwrap();
        assert!(!parsed.agent_succeeded && !parsed.agent_error);
    }

    #[test]
    fn clones_are_isolated_and_skip_generated_directories() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("file"), "original").unwrap();
        for excluded in EXCLUDED_DIRS { fs::create_dir(source.path().join(excluded)).unwrap(); fs::write(source.path().join(excluded).join("x"), "x").unwrap(); }
        let first = clone_project(source.path()).unwrap();
        let second = clone_project(source.path()).unwrap();
        assert_ne!(first.path(), second.path());
        fs::write(first.path().join("file"), "changed").unwrap();
        assert_eq!(fs::read_to_string(second.path().join("file")).unwrap(), "original");
        assert!(EXCLUDED_DIRS.iter().all(|d| !first.path().join(d).exists()));
        assert_eq!(json!({"pass": score_report("", vec![]).pass_rate}), json!({"pass": 0.0}));
    }

    #[test]
    fn rejects_escape_paths_and_symlinks_and_detects_baseline_regressions() {
        let outer = tempfile::tempdir().unwrap();
        let source = outer.path().join("source"); fs::create_dir(&source).unwrap();
        fs::write(outer.path().join("secret"), "private").unwrap();
        symlink(outer.path().join("secret"), source.join("escape")).unwrap();
        assert!(clone_project(&source).is_err());
        assert!(validate_relative(Path::new("../secret")).is_err());
        let metrics = RunMetrics { prompt_tokens: 5, output_tokens: 8, elapsed_ms: 10, tool_calls: 2, tool_errors: 0, agent_succeeded: true, ..Default::default() };
        let good = score_report("s", vec![CaseResult { id: "a".into(), passed: true, checks_passed: 1, checks_total: 1, metrics }]);
        let bad = score_report("s", vec![CaseResult { id: "a".into(), passed: false, checks_passed: 0, checks_total: 1, metrics }]);
        assert!(compare_baseline(&bad, &good).regressed);
        assert!(!compare_baseline(&good, &good).regressed);
        let slower = score_report("s", vec![CaseResult { id: "a".into(), passed: true, checks_passed: 1, checks_total: 1, metrics: RunMetrics { elapsed_ms: 20, ..metrics } }]);
        assert!(compare_baseline_with_thresholds(&slower, &good, RegressionThresholds { max_latency_increase_ms: Some(5), ..Default::default() }).regressed);
    }
    #[test]
    fn benchmark_copy_omits_common_credentials_but_keeps_templates() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join(".env"), "OPENAI_API_KEY=secret").unwrap();
        fs::write(source.path().join(".env.example"), "OPENAI_API_KEY=replace-me").unwrap();
        fs::write(source.path().join("private.pem"), "private key").unwrap();
        symlink(source.path().join(".env"), source.path().join("env_alias")).unwrap();
        fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
        let copied = clone_project(source.path()).unwrap();
        assert!(!copied.path().join(".env").exists());
        assert!(!copied.path().join("env_alias").exists());
        assert!(!copied.path().join("private.pem").exists());
        assert!(copied.path().join(".env.example").is_file());
        assert!(copied.path().join("main.rs").is_file());
    }
}
