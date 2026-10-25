//! Reproducible, local benchmark suite parsing, workspace isolation, and scoring.
#[cfg(test)]
mod integration_tests;
pub mod records;
pub mod runtime;
use crate::config::types::LeanLocal;
pub use crate::workspace_copy::clone_project;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Instant,
};

pub const MAX_CASES: usize = 1_000;
pub const MAX_CHECKS_PER_CASE: usize = 100;
pub const MAX_TURNS_PER_CASE: usize = 100;
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PROFILES: usize = 64;

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
    FileExists {
        path: PathBuf,
    },
    FileContains {
        path: PathBuf,
        needle: String,
    },
    FileEquals {
        path: PathBuf,
        content: String,
    },
    /// Exact final answer, ignoring only surrounding whitespace. The runner retains its hash,
    /// not response prose; missing observations make this check unavailable rather than passing.
    ResponseEquals {
        content: String,
    },
    Command {
        command: String,
        #[serde(default)]
        expect_stdout: Option<String>,
        #[serde(default)]
        expect_exit: i32,
    },
    Metric {
        metric: ObservedMetric,
        #[serde(default)]
        min: Option<u64>,
        #[serde(default)]
        max: Option<u64>,
    },
    /// Integration-path coverage, not an independent task outcome or tool-count objective.
    ToolInvoked {
        tool: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunMetrics {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    #[serde(default)]
    pub speculative_preparations: u64,
    #[serde(default)]
    pub speculative_reuses: u64,
    #[serde(default)]
    pub speculative_discards: u64,
    #[serde(default)]
    pub speculative_preparation_ms: u64,
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
    /// True only when the runner observed provider usage, not an estimated token count.
    #[serde(default)]
    pub token_usage_observed: bool,
    #[serde(default)]
    pub token_usage_incomplete: bool,
    /// Actual repair attempts, when supplied by the agent instrumentation.
    #[serde(default)]
    pub repair_attempts: Option<u64>,
    /// Observed API cost: a lower bound unless `observed_cost_complete` is true.
    /// Missing usage or prices and local/subscription routes are not zero-cost evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_cost_usd: Option<f64>,
    #[serde(default)]
    pub observed_cost_complete: bool,
    /// Logical backend requests from the runtime ledger (all purposes); 0 when the ledger was unread.
    #[serde(default)]
    pub requests: u64,
    /// Main-agent tool trace from the child's final snapshot. Absent in legacy or uninstrumented
    /// reports, where tool offers and use stay unknown rather than zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_trace: Option<ToolTrace>,
    /// Exact runtime ledger groups: `all`, `main`, `workers`, `by_purpose`, and coverage.
    /// Purpose and ownership are orthogonal views, never additional usage to sum together.
    /// Absent fields in older runtimes remain absent (unknown), not fabricated zeros.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_usage: Option<serde_json::Value>,
    /// SHA256 of the trimmed final completed response; never stores response text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sha256: Option<String>,
    /// Runtime instrumentation only; unavailable counters must be omitted, not estimated from
    /// tool results or notices. In particular, a tool dispatch does not establish useful work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efficiency: Option<serde_json::Value>,
}

/// Main-agent schema offers and admitted executions for one case. Worker, extension and
/// compaction-internal calls are outside its scope. `complete` is false when the run failed or
/// any turn lacked a snapshot; an incomplete trace never establishes that a tool went unused.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolTrace {
    pub scope: ToolTraceScope,
    pub complete: bool,
    pub successful_requests: u64,
    /// Per tool: successful main requests whose offered schemas named it.
    pub offered: std::collections::BTreeMap<String, u64>,
    /// Per tool: admitted calls that reached execution.
    pub invoked: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolTraceScope {
    #[default]
    MainAgent,
}

impl ToolTrace {
    /// Parses the child's `metrics.tool_trace` snapshot. Any missing or malformed field yields
    /// `None`, so the trace stays unknown instead of being partially trusted.
    pub fn from_snapshot(snapshot: &serde_json::Value, complete: bool) -> Option<Self> {
        let counts = |name: &str| -> Option<std::collections::BTreeMap<String, u64>> {
            let mut map = std::collections::BTreeMap::new();
            for (tool, count) in snapshot.get(name)?.as_object()? {
                if tool.is_empty() {
                    return None;
                }
                map.insert(tool.clone(), count.as_u64()?);
            }
            Some(map)
        };
        Some(Self {
            scope: ToolTraceScope::MainAgent,
            complete,
            successful_requests: snapshot.get("successful_requests")?.as_u64()?,
            offered: counts("offered")?,
            invoked: counts("invoked")?,
        })
    }
}

#[cfg(test)]
mod efficiency_tests;
#[cfg(test)]
mod tool_trace_tests;

/// Verification metadata, not prompt contents or a model's claim of success.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaseEvidence {
    pub categories: Vec<String>,
    pub outcome_checks: usize,
    pub failed_check_indices: Vec<usize>,
    pub unavailable_check_indices: Vec<usize>,
    pub contract_sha256: String,
    /// Redacted, bounded reason the agent run failed (error chain plus a tail of child
    /// diagnostics). Absent for successful runs and legacy reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_error_detail: Option<String>,
}

/// Upper bound on a recorded agent failure reason, in bytes.
pub const MAX_FAILURE_DETAIL_BYTES: usize = 2 * 1024;

/// A runner failure that still carries the metrics observed before it, so partial usage and the
/// failure reason reach the report instead of being replaced by zeros.
#[derive(Debug, Clone)]
pub struct AgentFailure {
    pub metrics: RunMetrics,
    /// Already redacted and bounded by [`failure_detail`].
    pub detail: String,
}

impl std::fmt::Display for AgentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for AgentFailure {}

/// An unmet runtime prerequisite, not a failed coding outcome or simulated integration.
#[derive(Debug, Clone)]
pub struct AgentUnavailable(pub String);
impl std::fmt::Display for AgentUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AgentUnavailable {}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Redacts, then bounds a failure reason to [`MAX_FAILURE_DETAIL_BYTES`]: the head of the error
/// chain (where the cause is named) and the tail of child diagnostics (where the last words are).
pub fn failure_detail(error: &str, diagnostics: &str) -> String {
    const ELLIPSIS: &str = "…";
    let error = crate::storage::redact::redact_text(error.trim());
    let diagnostics = crate::storage::redact::redact_text(diagnostics.trim());
    let error_budget = if diagnostics.is_empty() {
        MAX_FAILURE_DETAIL_BYTES
    } else {
        MAX_FAILURE_DETAIL_BYTES / 2
    };
    let mut detail = if error.len() > error_budget {
        let end = floor_boundary(&error, error_budget - ELLIPSIS.len());
        format!("{}{ELLIPSIS}", &error[..end])
    } else {
        error.into_owned()
    };
    if !diagnostics.is_empty() {
        const LABEL: &str = "\ndiagnostics: ";
        let budget = MAX_FAILURE_DETAIL_BYTES.saturating_sub(detail.len() + LABEL.len());
        if budget > ELLIPSIS.len() {
            detail.push_str(LABEL);
            if diagnostics.len() > budget {
                let mut start = diagnostics.len() - (budget - ELLIPSIS.len());
                while !diagnostics.is_char_boundary(start) {
                    start += 1;
                }
                detail.push_str(ELLIPSIS);
                detail.push_str(&diagnostics[start..]);
            } else {
                detail.push_str(&diagnostics);
            }
        }
    }
    detail
}

/// An agent error with no observed model activity at all: no usage (even partial), tokens,
/// tool calls, compactions or completed turns. Such a case never measured the model.
fn failed_before_model_activity(metrics: &RunMetrics) -> bool {
    metrics.agent_error
        && !metrics.token_usage_observed
        && !metrics.token_usage_incomplete
        && metrics
            .runtime_usage
            .as_ref()
            .and_then(|usage| usage.pointer("/all/transport_attempts"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            == 0
        && [
            metrics.prompt_tokens,
            metrics.output_tokens,
            metrics.cached_tokens,
            metrics.reasoning_tokens,
            metrics.tool_calls,
            metrics.compactions,
            metrics.turns_completed,
        ]
        .iter()
        .all(|value| *value == 0)
}

/// Why a set of case results is not a measurement: every case failed before any observed model
/// activity (typically missing credentials or a broken child), so its zero score is meaningless.
pub fn invalid_run_reason(cases: &[CaseResult]) -> Option<String> {
    if cases.is_empty()
        || !cases
            .iter()
            .all(|case| failed_before_model_activity(&case.metrics))
    {
        return None;
    }
    let first = cases
        .iter()
        .find_map(|case| {
            case.evidence
                .as_ref()
                .and_then(|evidence| evidence.agent_error_detail.as_deref())
                .map(|detail| (case.id.as_str(), detail))
        })
        .map_or_else(
            || "no failure detail recorded".to_owned(),
            |(id, detail)| {
                let end = floor_boundary(detail, 512);
                let ellipsis = if end < detail.len() { "…" } else { "" };
                format!("case {id}: {}{ellipsis}", &detail[..end])
            },
        );
    Some(format!(
        "invalid run: all {} cases failed before any observed model activity (no token usage, tool calls or completed turns); first failure: {first}",
        cases.len()
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseResult {
    pub id: String,
    pub passed: bool,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub metrics: RunMetrics,
    #[serde(default)]
    pub evidence: Option<CaseEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkReport {
    pub suite: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<records::Contract>,
    pub cases: Vec<CaseResult>,
    pub pass_rate: f64,
    pub average_latency_ms: f64,
    pub total_tokens: u64,
    pub total_tool_calls: u64,
    pub total_tool_errors: u64,
    pub score: f64,
    /// Compiled executable identity; legacy reports may not carry it.
    #[serde(default)]
    pub execution_build: Option<serde_json::Value>,
    /// Set when the run never measured the model (see [`invalid_run_reason`]); its scores are
    /// not results and must not be compared against or ranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_run: Option<String>,
    /// Outcome-verified work with all attempted-case costs, including failures, in the numerator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub useful_work: Option<UsefulWork>,
}

impl BenchmarkReport {
    /// The recorded invalid-run reason, re-derived from the cases for legacy reports.
    pub fn invalid_reason(&self) -> Option<String> {
        self.invalid_run
            .clone()
            .or_else(|| invalid_run_reason(&self.cases))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsefulWork {
    pub verified_completed_cases: usize,
    pub outcome_checked_cases: usize,
    /// Sum of case runtimes, not benchmark wall time (which includes external verification).
    pub attempted_elapsed_ms: u64,
    pub elapsed_ms_per_verified_completion: Option<f64>,
    /// Unknown unless every attempted case has complete observed token usage.
    pub tokens_per_verified_completion: Option<f64>,
    /// Unknown unless every attempted case has complete observed API pricing.
    pub cost_usd_per_verified_completion: Option<f64>,
}

/// Only independent outcome checks can establish correctness; manual review stays unverified.
fn outcome_verified_case(case: &CaseResult) -> bool {
    case.evidence.as_ref().is_some_and(|evidence| {
        evidence.outcome_checks > 0
            && !evidence
                .categories
                .iter()
                .any(|category| category == "answer_requires_manual_review")
    })
}

impl UsefulWork {
    fn measured(cases: &[CaseResult]) -> Self {
        let checked = |case: &&CaseResult| outcome_verified_case(case);
        let completed = cases.iter().filter(checked).filter(|c| c.passed).count();
        let elapsed = cases
            .iter()
            .fold(0u64, |sum, c| sum.saturating_add(c.metrics.elapsed_ms));
        let tokens = cases.iter().try_fold(0u64, |sum, c| {
            (c.metrics.token_usage_observed && !c.metrics.token_usage_incomplete).then(|| {
                sum.saturating_add(c.metrics.prompt_tokens)
                    .saturating_add(c.metrics.output_tokens)
            })
        });
        let cost = cases.iter().try_fold(0.0, |sum, c| {
            c.metrics
                .observed_cost_complete
                .then_some(c.metrics.observed_cost_usd)
                .flatten()
                .map(|cost| sum + cost)
        });
        Self {
            verified_completed_cases: completed,
            outcome_checked_cases: cases.iter().filter(checked).count(),
            attempted_elapsed_ms: elapsed,
            elapsed_ms_per_verified_completion: (completed > 0 && elapsed > 0)
                .then(|| elapsed as f64 / completed as f64),
            tokens_per_verified_completion: tokens
                .filter(|_| completed > 0)
                .map(|tokens| tokens as f64 / completed as f64),
            cost_usd_per_verified_completion: cost
                .filter(|_| completed > 0)
                .map(|cost| cost / completed as f64),
        }
    }
}

pub fn response_sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.trim().as_bytes()))
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct BaselineComparison {
    pub score_delta: f64,
    pub pass_rate_delta: f64,
    pub regressed: bool,
    /// Either side is an invalid run; deltas are meaningless and the comparison counts as regressed.
    #[serde(default)]
    pub invalid: bool,
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
    /// `profile: reason` for every profile that never measured its model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalid_runs: Vec<String>,
}

/// `name: reason` for each named report that is an invalid run (see [`invalid_run_reason`]).
pub fn named_invalid_runs<'a>(
    reports: impl IntoIterator<Item = (&'a str, &'a BenchmarkReport)>,
) -> Vec<String> {
    reports
        .into_iter()
        .filter_map(|(name, report)| {
            report
                .invalid_reason()
                .map(|reason| format!("{name}: {reason}"))
        })
        .collect()
}

/// Explicit configurations are measured, never selected from hardware/model-name guesses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MachineConfiguration {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub context_window: u32,
    /// Expected local config.json quantization bits; this is checked, not a conversion request.
    #[serde(default)]
    pub quantization_bits: Option<u8>,
    #[serde(default)]
    pub adapter: Option<PathBuf>,
    /// Feature overrides applied on top of the user's config; unset fields keep the configured value.
    #[serde(default)]
    pub assistance: AssistanceOverrides,
    /// Local MLX runtime overrides for this configuration (speculative draft model, KV-cache quantization).
    #[serde(default)]
    pub draft_model: Option<String>,
    #[serde(default)]
    pub num_draft_tokens: Option<u32>,
    #[serde(default)]
    pub kv_bits: Option<u8>,
    #[serde(default)]
    pub kv_group_size: Option<u32>,
    #[serde(default)]
    pub quantized_kv_start: Option<u32>,
    /// Context-economy overrides (`efficiency.*`, `compaction.*`); unset keeps the configured value.
    #[serde(default)]
    pub efficiency: EfficiencyOverrides,
    /// Outcome-contract and selective review levers; never changes the operator config.
    #[serde(default)]
    pub policy: PolicyOverrides,
    /// Real service/delegation lane. Core isolation remains the default.
    #[serde(default)]
    pub integration: IntegrationOverrides,
    /// `experimental.*` workflow switches for A/B rows; unset keeps the configured value.
    #[serde(default)]
    pub experimental: ExperimentalOverrides,
}

/// Per-configuration assistance feature switches for ablation. `None` keeps the configured value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AssistanceOverrides {
    /// Master switch for experience reuse, syntax guard and selective verification.
    pub enabled: Option<bool>,
    pub compact_tools: Option<bool>,
    pub initial_tool_limit: Option<usize>,
    pub tool_grammar: Option<bool>,
    pub repair_hints: Option<bool>,
    pub syntax_guard: Option<bool>,
    pub selective_verification: Option<bool>,
    pub edit_mode: Option<crate::config::types::EditMode>,
    /// Maps to `reliability.enabled`.
    pub reliability_gate: Option<bool>,
    /// Maps to `assistance.constrained_repair`: schema-constrained repair of malformed tool calls.
    pub constrained_repair: Option<bool>,
    /// Maps to `assistance.lean_local`: the lean prompt and tool profile for locally served models.
    pub lean_local: Option<LeanLocal>,
    /// Maps to `assistance.local.enabled`: the local-model profile (tiers, edit default, sampling,
    /// reasoning cap, escalation) for locally served models. Absent in older reports.
    pub local_profile: Option<bool>,
}

impl AssistanceOverrides {
    /// Initial tool budget follows the same bounds as `assistance.initial_tool_limit`.
    pub fn validate(&self) -> Result<()> {
        if let Some(limit) = self.initial_tool_limit {
            ensure!(
                limit <= 64,
                "configuration assistance.initial_tool_limit must be 0 or 1..=64"
            );
        }
        Ok(())
    }
}

/// Per-configuration context-economy levers for comparison runs. `None` keeps the configured value;
/// bounds match `Config::validate`, and overrides apply only to the run's isolated config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EfficiencyOverrides {
    /// Maps to `efficiency.tool_preview_bytes`: 0 or 1024..=16384.
    pub tool_preview_bytes: Option<usize>,
    /// Maps to `efficiency.history_token_cap`: 0 or 8192..=2_000_000.
    pub history_token_cap: Option<u32>,
    /// Maps to `compaction.enabled`.
    pub compaction_enabled: Option<bool>,
    /// Maps to `compaction.keep_recent_tokens`.
    pub keep_recent_tokens: Option<u32>,
}

impl EfficiencyOverrides {
    pub fn validate(&self) -> Result<()> {
        if let Some(bytes) = self.tool_preview_bytes {
            ensure!(
                bytes == 0 || (1024..=16384).contains(&bytes),
                "configuration efficiency.tool_preview_bytes must be 0 or 1024..=16384"
            );
        }
        if let Some(cap) = self.history_token_cap {
            ensure!(
                cap == 0 || (8192..=2_000_000).contains(&cap),
                "configuration efficiency.history_token_cap must be 0 or 8192..=2000000"
            );
        }
        Ok(())
    }
}

/// Per-configuration `experimental.*` switches. `None` keeps the configured value; the bounds
/// match the settings schema, and overrides apply only to the run's isolated config.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExperimentalOverrides {
    pub fault_witness: Option<bool>,
    pub repro_first: Option<crate::config::types::ReproFirst>,
    /// 0..=16 isolated check runs.
    pub edit_bisection_runs: Option<u8>,
    /// 0..=10 non-improving edit rounds.
    pub outcome_gradient_rounds: Option<u8>,
    pub streaming_dispatch: Option<bool>,
    pub check_predictions: Option<bool>,
}

impl ExperimentalOverrides {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.edit_bisection_runs.is_none_or(|runs| runs <= 16),
            "configuration experimental.edit_bisection_runs must be 0..=16"
        );
        ensure!(
            self.outcome_gradient_rounds
                .is_none_or(|rounds| rounds <= 10),
            "configuration experimental.outcome_gradient_rounds must be 0..=10"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMode {
    #[default]
    Core,
    Mcp,
    Browser,
    Delegation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IntegrationOverrides {
    pub mode: IntegrationMode,
    /// Exact operator-configured server names, never programs, URLs or credentials.
    pub mcp_servers: Vec<String>,
}

impl IntegrationOverrides {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.mcp_servers.len() <= 16,
            "integration MCP allowlist exceeds 16 servers"
        );
        ensure!(
            self.mode == IntegrationMode::Mcp || self.mcp_servers.is_empty(),
            "integration.mcp_servers is only valid in mcp mode"
        );
        ensure!(
            self.mode != IntegrationMode::Mcp || !self.mcp_servers.is_empty(),
            "mcp mode requires an exact server allowlist"
        );
        let mut names = std::collections::BTreeSet::new();
        ensure!(
            self.mcp_servers.iter().all(|name| !name.is_empty()
                && name.len() <= 128
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                && names.insert(name)),
            "invalid or duplicate integration MCP server name"
        );
        Ok(())
    }
}

/// Explicit one-factor experiments, not learned routing or automatic default changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyOverrides {
    pub strict_requirements: Option<bool>,
    pub review_enabled: Option<bool>,
    pub review_min_changed_files: Option<usize>,
    pub review_sensitive_paths: Option<Vec<String>>,
    pub review_max_rounds: Option<u8>,
}

impl PolicyOverrides {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.review_min_changed_files
                .is_none_or(|n| (1..=1000).contains(&n)),
            "policy.review_min_changed_files must be 1..=1000"
        );
        ensure!(
            self.review_max_rounds.is_none_or(|n| (1..=3).contains(&n)),
            "policy.review_max_rounds must be 1..=3"
        );
        if let Some(paths) = &self.review_sensitive_paths {
            ensure!(
                paths.len() <= 32,
                "policy.review_sensitive_paths exceeds 32 paths"
            );
            for path in paths {
                ensure!(
                    !path.is_empty()
                        && path.len() <= 256
                        && !path
                            .chars()
                            .any(|c| c.is_control() || matches!(c, '\\' | '*' | '?' | '[' | ']'))
                        && !path.contains(':')
                        && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
                    "policy.review_sensitive_paths requires workspace-relative prefixes"
                );
                validate_relative(Path::new(path))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MachineBenchmarkPolicy {
    pub repetitions: usize,
    pub case_timeout_secs: u64,
    pub minimum_correctness: f64,
    /// Remote inference requires this explicit opt-in; verification tools stay offline.
    pub allow_remote: bool,
    pub regression_thresholds: RegressionThresholds,
}

impl Default for MachineBenchmarkPolicy {
    fn default() -> Self {
        Self {
            repetitions: 1,
            case_timeout_secs: 0,
            minimum_correctness: 1.0,
            allow_remote: false,
            regression_thresholds: RegressionThresholds::default(),
        }
    }
}

impl MachineBenchmarkPolicy {
    pub fn validate(
        &self,
        suite: &BenchmarkSuite,
        configurations: &[MachineConfiguration],
    ) -> Result<()> {
        suite.validate()?;
        ensure!(
            (1..=10).contains(&self.repetitions),
            "machine benchmark repetitions must be 1..=10"
        );
        ensure!(
            self.case_timeout_secs <= 600,
            "machine benchmark case timeout must be 0 (unlimited) or 1..=600 seconds"
        );
        ensure!(
            self.minimum_correctness.is_finite() && (0.0..=1.0).contains(&self.minimum_correctness),
            "minimum_correctness must be between zero and one"
        );
        ensure!(
            !configurations.is_empty() && configurations.len() <= MAX_PROFILES,
            "machine benchmark needs 1..={MAX_PROFILES} configurations"
        );
        ensure!(
            suite
                .cases
                .len()
                .saturating_mul(configurations.len())
                .saturating_mul(self.repetitions)
                <= MAX_CASES,
            "machine benchmark exceeds {MAX_CASES} total case runs"
        );
        let mut names = std::collections::BTreeSet::new();
        for configuration in configurations {
            ensure!(
                !configuration.name.trim().is_empty() && names.insert(&configuration.name),
                "configuration names must be nonempty and unique"
            );
            ensure!(
                !configuration.provider.trim().is_empty() && !configuration.model.trim().is_empty(),
                "configuration provider and model must be explicit"
            );
            ensure!(
                (2048..=1_048_576).contains(&configuration.context_window),
                "configuration context_window must be 2048..=1048576"
            );
            ensure!(
                configuration
                    .quantization_bits
                    .is_none_or(|bits| (1..=16).contains(&bits)),
                "quantization_bits must be 1..=16"
            );
            configuration.assistance.validate()?;
            configuration.efficiency.validate()?;
            configuration.experimental.validate()?;
            configuration.policy.validate()?;
            configuration.integration.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineIdentity {
    pub hostname: String,
    pub operating_system: String,
    pub architecture: String,
    pub logical_cpus: Option<usize>,
    pub dial_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineConfigurationReport {
    pub configuration: MachineConfiguration,
    /// Effective runtime settings and on-disk metadata, not advertised capabilities.
    pub settings: serde_json::Value,
    pub report: BenchmarkReport,
    /// Includes failed attempts and deterministic verification, not just successful latency.
    pub wall_time_ms: u64,
    pub observed_prompt_tokens: Option<u64>,
    pub observed_output_tokens: Option<u64>,
    /// Observed API cost; absent unless every case has complete tokens and observed pricing.
    #[serde(default)]
    pub observed_cost_usd: Option<f64>,
    pub repair_attempts: Option<u64>,
    pub correct_cases_per_second: Option<f64>,
    pub eligible: bool,
    pub baseline_comparison: BaselineComparison,
}

impl MachineConfigurationReport {
    pub fn measured(
        configuration: MachineConfiguration,
        settings: serde_json::Value,
        report: BenchmarkReport,
        wall_time_ms: u64,
    ) -> Self {
        let all_usage = !report.cases.is_empty()
            && report.cases.iter().all(|case| {
                case.metrics.token_usage_observed && !case.metrics.token_usage_incomplete
            });
        let repairs = report.cases.iter().try_fold(0u64, |sum, case| {
            case.metrics.repair_attempts.map(|n| sum.saturating_add(n))
        });
        let successes = report
            .cases
            .iter()
            .filter(|case| case.passed && outcome_verified_case(case))
            .count();
        Self {
            configuration,
            settings,
            wall_time_ms,
            observed_prompt_tokens: all_usage.then(|| total_prompt_tokens(&report)),
            observed_output_tokens: all_usage.then(|| total_output_tokens(&report)),
            observed_cost_usd: if all_usage
                && report
                    .cases
                    .iter()
                    .all(|case| case.metrics.observed_cost_complete)
            {
                report
                    .cases
                    .iter()
                    .map(|case| case.metrics.observed_cost_usd)
                    .sum()
            } else {
                None
            },
            repair_attempts: repairs,
            correct_cases_per_second: (wall_time_ms > 0)
                .then(|| successes as f64 * 1000.0 / wall_time_ms as f64),
            eligible: false,
            baseline_comparison: BaselineComparison::default(),
            report,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineBenchmarkReport {
    pub machine: MachineIdentity,
    pub suite: String,
    pub suite_sha256: String,
    pub measured_at_unix_secs: u64,
    pub policy: MachineBenchmarkPolicy,
    pub baseline_configuration: String,
    pub recommended_configuration: Option<String>,
    pub configurations: Vec<MachineConfigurationReport>,
    /// `configuration: reason` for every configuration that never measured its model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalid_runs: Vec<String>,
}

/// First configuration is the baseline. Only floor-passing, non-regressing
/// configurations with at least one verified success and measured wall time can win.
pub fn rank_machine_configurations(
    reports: &mut [MachineConfigurationReport],
    policy: &MachineBenchmarkPolicy,
) -> Result<Option<String>> {
    let baseline = reports
        .first()
        .context("machine benchmark has no reports")?
        .report
        .clone();
    for report in reports.iter_mut() {
        ensure!(
            report.report.suite == baseline.suite
                && report.report.cases.len() == baseline.cases.len()
                && report
                    .report
                    .cases
                    .iter()
                    .zip(&baseline.cases)
                    .all(|(now, before)| now.id == before.id
                        && now.checks_total == before.checks_total),
            "machine benchmark configurations must execute identical verified tasks"
        );
        report.baseline_comparison = compare_baseline_with_thresholds(
            &report.report,
            &baseline,
            policy.regression_thresholds,
        );
        let checked = report
            .report
            .cases
            .iter()
            .filter(|case| outcome_verified_case(case))
            .count();
        let successes = report
            .report
            .cases
            .iter()
            .filter(|case| case.passed && outcome_verified_case(case))
            .count();
        // Reports may be deserialized from an older scoring version or modified after measurement.
        // Never let a cached derived rate override the outcomes and elapsed time being ranked.
        report.correct_cases_per_second = (report.wall_time_ms > 0)
            .then(|| successes as f64 * 1000.0 / report.wall_time_ms as f64);
        // An invalid run never measured its model; it cannot be recommended, and comparisons
        // against an invalid baseline are flagged as regressed.
        report.eligible = report.report.invalid_reason().is_none()
            && checked > 0
            && successes as f64 / checked as f64 >= policy.minimum_correctness
            && !report.baseline_comparison.regressed
            && report
                .correct_cases_per_second
                .is_some_and(|rate| rate > 0.0);
    }
    reports.sort_by(|a, b| {
        b.eligible
            .cmp(&a.eligible)
            .then_with(|| {
                b.correct_cases_per_second
                    .unwrap_or(0.0)
                    .total_cmp(&a.correct_cases_per_second.unwrap_or(0.0))
            })
            .then_with(|| a.configuration.name.cmp(&b.configuration.name))
    });
    Ok(reports
        .first()
        .filter(|report| report.eligible)
        .map(|report| report.configuration.name.clone()))
}

impl BenchmarkSuite {
    pub fn parse_toml(input: &str) -> Result<Self> {
        let suite: Self = toml::from_str(input).context("parsing benchmark suite")?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty(),
            "benchmark suite name cannot be empty"
        );
        ensure!(
            self.version == 1,
            "unsupported benchmark suite version {}",
            self.version
        );
        ensure!(
            !self.cases.is_empty() && self.cases.len() <= MAX_CASES,
            "benchmark suite must contain 1..={MAX_CASES} cases"
        );
        let mut ids = std::collections::BTreeSet::new();
        for case in &self.cases {
            ensure!(
                !case.id.trim().is_empty() && ids.insert(&case.id),
                "benchmark case IDs must be non-empty and unique"
            );
            ensure!(
                case.checks.len() <= MAX_CHECKS_PER_CASE,
                "case {} exceeds {MAX_CHECKS_PER_CASE} checks",
                case.id
            );
            ensure!(
                !case.checks.is_empty(),
                "case {} must contain at least one deterministic check",
                case.id
            );
            ensure!(
                !case.prompt.trim().is_empty(),
                "case {} prompt cannot be empty",
                case.id
            );
            ensure!(
                case.turns.len() <= MAX_TURNS_PER_CASE,
                "case {} exceeds {MAX_TURNS_PER_CASE} follow-up turns",
                case.id
            );
            ensure!(
                case.turns.iter().all(|turn| !turn.prompt.trim().is_empty()),
                "case {} has an empty follow-up prompt",
                case.id
            );
            let bytes = case
                .context_messages
                .iter()
                .map(String::len)
                .chain(std::iter::once(case.prompt.len()))
                .chain(case.turns.iter().map(|turn| turn.prompt.len()))
                .fold(0usize, usize::saturating_add);
            ensure!(
                bytes <= MAX_CONTEXT_BYTES,
                "case {} exceeds {MAX_CONTEXT_BYTES} input bytes",
                case.id
            );
            let mut categories = std::collections::BTreeSet::new();
            ensure!(
                case.categories
                    .iter()
                    .all(|category| !category.trim().is_empty() && categories.insert(category)),
                "case {} categories must be non-empty and unique",
                case.id
            );
            if let Some(project) = &case.project {
                validate_relative(project).context("invalid case project path")?;
            }
            for assertion in &case.checks {
                match assertion {
                    Assertion::FileExists { path }
                    | Assertion::FileContains { path, .. }
                    | Assertion::FileEquals { path, .. } => {
                        validate_relative(path).context("invalid assertion path")?
                    }
                    Assertion::Command { command, .. } => ensure!(
                        !command.trim().is_empty(),
                        "command assertion cannot be empty"
                    ),
                    Assertion::ResponseEquals { content } => ensure!(
                        !content.trim().is_empty() && content.len() <= MAX_CONTEXT_BYTES,
                        "response_equals needs a nonempty bounded answer"
                    ),
                    Assertion::ToolInvoked { tool } => ensure!(
                        !tool.is_empty()
                            && tool.len() <= 256
                            && !tool.chars().any(char::is_control),
                        "tool_invoked needs an exact nonempty tool name"
                    ),
                    Assertion::Metric { min, max, .. } => {
                        ensure!(
                            min.is_some() || max.is_some(),
                            "metric assertion needs a bound"
                        );
                        ensure!(
                            (*min).zip(*max).is_none_or(|(min, max)| min <= max),
                            "metric minimum exceeds maximum"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Run cases with parent-supplied execution and verifier callbacks. Agent and verifier
    /// failures are recorded per case rather than treated as a passing result.
    pub fn run<A, C>(
        &self,
        source_root: &Path,
        mut agent: A,
        mut command_check: C,
    ) -> Result<BenchmarkReport>
    where
        A: FnMut(&BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
    {
        self.run_recorded(source_root, &mut agent, &mut command_check, None)
    }

    pub fn run_recorded<A, C>(
        &self,
        source_root: &Path,
        agent: A,
        command_check: C,
        recorder: Option<&mut records::Recorder>,
    ) -> Result<BenchmarkReport>
    where
        A: FnMut(&BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
    {
        self.run_observed(source_root, agent, command_check, recorder, |_| Ok(()))
    }

    /// Observe every checked result before starting the next case. Observer failures stop the run;
    /// failed and unavailable trials are retained just like successes, including resumed results.
    pub fn run_observed<A, C, O>(
        &self,
        source_root: &Path,
        mut agent: A,
        mut command_check: C,
        mut recorder: Option<&mut records::Recorder>,
        mut observe: O,
    ) -> Result<BenchmarkReport>
    where
        A: FnMut(&BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
        O: FnMut(&CaseResult) -> Result<()>,
    {
        self.validate()?;
        let frozen = clone_project(source_root)?;
        let source_root = frozen.path();
        let contract = if let Some(recorder) = recorder.as_ref() {
            ensure!(
                records::source_digest(source_root)? == recorder.contract().source_sha256,
                "run source no longer matches the checkpoint contract"
            );
            recorder.contract().clone()
        } else {
            records::Contract::new(self, source_root, &serde_json::json!({"runner":"custom"}))?
        };
        let mut results = Vec::with_capacity(self.cases.len());
        for case in &self.cases {
            if let Some(result) = recorder.as_ref().and_then(|record| record.completed(case)) {
                observe(&result)?;
                results.push(result);
                continue;
            }
            let source = match &case.project {
                Some(relative) => confined_existing(source_root, relative)?,
                None => source_root.to_path_buf(),
            };
            let workspace = clone_project(&source)?;
            let started = Instant::now();
            let mut agent_unavailable = false;
            let (mut metrics, agent_error_detail) = match agent(case, workspace.path()) {
                Ok(metrics) => (metrics, None),
                Err(error) if error.is::<AgentUnavailable>() => {
                    agent_unavailable = true;
                    (
                        RunMetrics {
                            agent_error: true,
                            ..RunMetrics::default()
                        },
                        Some(failure_detail(&error.to_string(), "")),
                    )
                }
                Err(error) => match error.downcast::<AgentFailure>() {
                    Ok(failure) => (
                        RunMetrics {
                            agent_error: true,
                            agent_succeeded: false,
                            ..failure.metrics
                        },
                        Some(failure.detail),
                    ),
                    Err(error) => (
                        RunMetrics {
                            agent_error: true,
                            ..RunMetrics::default()
                        },
                        Some(failure_detail(&format!("{error:#}"), "")),
                    ),
                },
            };
            if metrics.elapsed_ms == 0 {
                metrics.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            }
            let mut checks_passed = 0;
            let mut failed_check_indices = Vec::new();
            let mut unavailable_check_indices = Vec::new();
            for (index, assertion) in case.checks.iter().enumerate() {
                if agent_unavailable {
                    unavailable_check_indices.push(index);
                    failed_check_indices.push(index);
                    continue;
                }
                let passed = match assertion {
                    Assertion::FileExists { path } => {
                        confined_existing(workspace.path(), path).is_ok_and(|path| path.is_file())
                    }
                    Assertion::FileContains { path, needle } => {
                        confined_existing(workspace.path(), path)
                            .ok()
                            .and_then(|p| fs::read_to_string(p).ok())
                            .is_some_and(|s| s.contains(needle))
                    }
                    Assertion::FileEquals { path, content } => {
                        confined_existing(workspace.path(), path)
                            .ok()
                            .and_then(|p| fs::read_to_string(p).ok())
                            .is_some_and(|s| s == *content)
                    }
                    Assertion::Command {
                        command,
                        expect_stdout,
                        expect_exit,
                    } => match command_check(
                        &CommandCheck {
                            command,
                            expect_stdout: expect_stdout.as_deref(),
                            expect_exit: *expect_exit,
                        },
                        workspace.path(),
                    ) {
                        Ok(passed) => passed,
                        Err(_) => {
                            unavailable_check_indices.push(index);
                            false
                        }
                    },
                    Assertion::Metric { metric, min, max } => {
                        let value = metrics.observed(*metric);
                        min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
                    }
                    Assertion::ResponseEquals { content } => {
                        if metrics.response_sha256.is_none() {
                            unavailable_check_indices.push(index);
                        }
                        metrics.response_sha256.as_deref()
                            == Some(response_sha256(content).as_str())
                    }
                    Assertion::ToolInvoked { tool } => {
                        let invoked = metrics.tool_trace.as_ref().is_some_and(|trace| {
                            trace.invoked.get(tool).is_some_and(|count| *count > 0)
                        });
                        if !invoked
                            && metrics
                                .tool_trace
                                .as_ref()
                                .is_none_or(|trace| !trace.complete)
                        {
                            unavailable_check_indices.push(index);
                        }
                        invoked
                    }
                };
                checks_passed += usize::from(passed);
                if !passed {
                    failed_check_indices.push(index);
                }
            }
            let completed_turns =
                case.turns.is_empty() || metrics.turns_completed == (case.turns.len() + 1) as u64;
            let forced_compactions =
                case.turns.iter().filter(|turn| turn.compact_before).count() as u64;
            let passed = metrics.agent_succeeded
                && !metrics.agent_error
                && completed_turns
                && metrics.compactions >= forced_compactions
                && checks_passed == case.checks.len();
            results.push(CaseResult {
                id: case.id.clone(),
                passed,
                checks_passed,
                checks_total: case.checks.len(),
                metrics,
                evidence: Some(CaseEvidence {
                    categories: case.categories.clone(),
                    outcome_checks: case
                        .checks
                        .iter()
                        .filter(|check| {
                            !matches!(
                                check,
                                Assertion::Metric { .. } | Assertion::ToolInvoked { .. }
                            )
                        })
                        .count(),
                    failed_check_indices,
                    unavailable_check_indices,
                    contract_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(case)?)),
                    agent_error_detail,
                }),
            });
            if let Some(recorder) = recorder.as_deref_mut() {
                recorder.record(results.last().expect("just recorded case"))?;
            }
            observe(results.last().expect("just recorded case"))?;
        }
        let mut report = score_report(&self.name, results);
        report.contract = Some(contract);
        Ok(report)
    }

    /// Each profile gets a fresh workspace for every case. The runner must select
    /// the named real configuration and apply each requested ablation or fail.
    pub fn run_profiles<A, C>(
        &self,
        source_root: &Path,
        profiles: &[BenchmarkProfile],
        mut agent: A,
        mut command_check: C,
    ) -> Result<ProfileComparisonReport>
    where
        A: FnMut(&BenchmarkProfile, &BenchmarkCase, &Path) -> Result<RunMetrics>,
        C: FnMut(&CommandCheck, &Path) -> Result<bool>,
    {
        ensure!(
            !profiles.is_empty() && profiles.len() <= MAX_PROFILES,
            "benchmark needs 1..={MAX_PROFILES} profiles"
        );
        let mut names = std::collections::BTreeSet::new();
        for profile in profiles {
            ensure!(
                !profile.name.trim().is_empty() && names.insert(&profile.name),
                "profile names must be non-empty and unique"
            );
            let mut ablations = std::collections::BTreeSet::new();
            ensure!(
                profile
                    .ablations
                    .iter()
                    .all(|item| !item.trim().is_empty() && ablations.insert(item)),
                "ablations must be non-empty and unique"
            );
        }
        let mut reports = Vec::with_capacity(profiles.len());
        for profile in profiles {
            let report = self.run(
                source_root,
                |case, workspace| agent(profile, case, workspace),
                &mut command_check,
            )?;
            let normalized = normalize(&report, None);
            let categories = category_metrics(self, &report);
            reports.push(ProfileReport {
                profile: profile.clone(),
                report,
                normalized,
                categories,
                baseline_comparison: BaselineComparison::default(),
            });
        }
        let (baseline, others) = reports
            .split_first_mut()
            .expect("validated non-empty profiles");
        baseline.normalized = normalize(&baseline.report, Some(&baseline.report));
        for report in others {
            report.normalized =
                normalize_cases(report.report.cases.iter(), Some(&baseline.normalized));
            report.baseline_comparison = compare_baseline(&report.report, &baseline.report);
        }
        reports.sort_by(|a, b| {
            b.normalized
                .verified_success_rate
                .total_cmp(&a.normalized.verified_success_rate)
                .then_with(|| {
                    a.normalized
                        .successful_latency_ms
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&b.normalized.successful_latency_ms.unwrap_or(f64::INFINITY))
                })
                .then_with(|| a.profile.name.cmp(&b.profile.name))
        });
        let invalid_runs = named_invalid_runs(
            reports
                .iter()
                .map(|report| (report.profile.name.as_str(), &report.report)),
        );
        Ok(ProfileComparisonReport {
            suite: self.name.clone(),
            baseline_profile: profiles[0].name.clone(),
            profiles: reports,
            invalid_runs,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommandCheck<'a> {
    pub command: &'a str,
    pub expect_stdout: Option<&'a str>,
    pub expect_exit: i32,
}

fn validate_relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "path must be a non-empty relative path without traversal"
    );
    Ok(())
}

fn confined_existing(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative(relative)?;
    let root = fs::canonicalize(root).context("resolving workspace root")?;
    let mut path = root.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("invalid workspace path")
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path).context("workspace path does not exist")?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "workspace assertion path traverses symlink"
        );
    }
    let resolved = fs::canonicalize(&path)?;
    ensure!(
        resolved.starts_with(root),
        "workspace assertion path escapes root"
    );
    Ok(resolved)
}

pub fn score_report(suite: &str, cases: Vec<CaseResult>) -> BenchmarkReport {
    let n = cases.len() as f64;
    let passed = cases.iter().filter(|case| case.passed).count() as f64;
    let pass_rate = if cases.is_empty() { 0.0 } else { passed / n };
    let average_latency_ms = if cases.is_empty() {
        0.0
    } else {
        cases
            .iter()
            .map(|case| case.metrics.elapsed_ms as f64)
            .sum::<f64>()
            / n
    };
    let total_tokens = cases.iter().fold(0u64, |sum, case| {
        sum.saturating_add(case.metrics.prompt_tokens)
            .saturating_add(case.metrics.output_tokens)
    });
    let total_tool_calls = cases.iter().fold(0u64, |sum, case| {
        sum.saturating_add(case.metrics.tool_calls)
    });
    let total_tool_errors = cases.iter().fold(0u64, |sum, case| {
        sum.saturating_add(case.metrics.tool_errors)
    });
    BenchmarkReport {
        suite: suite.to_owned(),
        contract: None,
        execution_build: Some(crate::build_info::report()),
        invalid_run: invalid_run_reason(&cases),
        useful_work: Some(UsefulWork::measured(&cases)),
        cases,
        pass_rate,
        average_latency_ms,
        total_tokens,
        total_tool_calls,
        total_tool_errors,
        score: pass_rate,
    }
}

pub fn compare_baseline(
    current: &BenchmarkReport,
    baseline: &BenchmarkReport,
) -> BaselineComparison {
    compare_baseline_with_thresholds(current, baseline, RegressionThresholds::default())
}

/// Compare aggregate/case outcomes and configured upper bounds on metric growth.
pub fn compare_baseline_with_thresholds(
    current: &BenchmarkReport,
    baseline: &BenchmarkReport,
    thresholds: RegressionThresholds,
) -> BaselineComparison {
    let score_delta = current.score - baseline.score;
    let pass_rate_delta = current.pass_rate - baseline.pass_rate;
    let current_by_id: std::collections::BTreeMap<_, _> =
        current.cases.iter().map(|case| (&case.id, case)).collect();
    let failed_previous_case = baseline
        .cases
        .iter()
        .filter(|case| case.passed)
        .any(|case| {
            current_by_id
                .get(&case.id)
                .is_none_or(|now| !now.passed || (case.checks_passed > now.checks_passed))
        });
    let exceeds = |current: u64, baseline: u64, threshold: Option<u64>| {
        threshold.is_some_and(|max| current.saturating_sub(baseline) > max)
    };
    let metric_regression = exceeds(
        current.total_tool_calls,
        baseline.total_tool_calls,
        thresholds.max_tool_call_increase,
    ) || exceeds(
        current.total_tool_errors,
        baseline.total_tool_errors,
        thresholds.max_tool_error_increase,
    ) || exceeds(
        total_prompt_tokens(current),
        total_prompt_tokens(baseline),
        thresholds.max_prompt_token_increase,
    ) || exceeds(
        total_output_tokens(current),
        total_output_tokens(baseline),
        thresholds.max_output_token_increase,
    ) || exceeds(
        current.average_latency_ms.max(0.0) as u64,
        baseline.average_latency_ms.max(0.0) as u64,
        thresholds.max_latency_increase_ms,
    );
    let unmatched = match (&current.contract, &baseline.contract) {
        (Some(current), Some(baseline)) => !current.matched_tasks(baseline),
        (None, None) => current.cases.iter().any(|case| {
            baseline
                .cases
                .iter()
                .find(|prior| prior.id == case.id)
                .is_some_and(|prior| {
                    case.evidence
                        .as_ref()
                        .zip(prior.evidence.as_ref())
                        .is_some_and(|(a, b)| a.contract_sha256 != b.contract_sha256)
                })
        }),
        _ => true,
    };
    let invalid =
        unmatched || current.invalid_reason().is_some() || baseline.invalid_reason().is_some();
    BaselineComparison {
        score_delta,
        pass_rate_delta,
        regressed: invalid
            || score_delta < 0.0
            || pass_rate_delta < 0.0
            || failed_previous_case
            || metric_regression,
        invalid,
    }
}

fn total_prompt_tokens(report: &BenchmarkReport) -> u64 {
    report.cases.iter().fold(0u64, |sum, case| {
        sum.saturating_add(case.metrics.prompt_tokens)
    })
}
fn total_output_tokens(report: &BenchmarkReport) -> u64 {
    report.cases.iter().fold(0u64, |sum, case| {
        sum.saturating_add(case.metrics.output_tokens)
    })
}

pub fn normalize(
    report: &BenchmarkReport,
    baseline: Option<&BenchmarkReport>,
) -> NormalizedMetrics {
    let baseline = baseline.map(|baseline| normalize_cases(baseline.cases.iter(), None));
    normalize_cases(report.cases.iter(), baseline.as_ref())
}

fn normalize_cases<'a>(
    cases: impl Iterator<Item = &'a CaseResult>,
    baseline: Option<&NormalizedMetrics>,
) -> NormalizedMetrics {
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
            case.metrics.prompt_tokens,
            case.metrics.output_tokens,
            case.metrics.cached_tokens,
            case.metrics.reasoning_tokens,
            case.metrics.tool_calls,
            case.metrics.tool_errors,
            case.metrics.tool_contract_errors,
            case.metrics.compactions,
            case.metrics.artifacts_retrieved,
            case.metrics.dynamic_tools_used,
            case.metrics.truncated_recoveries,
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
        check_pass_rate: if checks_total > 0.0 {
            checks_passed / checks_total
        } else {
            0.0
        },
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
        latency_relative_to_baseline: baseline
            .and_then(|baseline| successful_latency_ms.zip(baseline.successful_latency_ms))
            .and_then(|(now, before)| ratio(now, before)),
        tokens_relative_to_baseline: baseline.and_then(|baseline| {
            ratio(
                prompt_tokens_per_case + output_tokens_per_case,
                baseline.prompt_tokens_per_case + baseline.output_tokens_per_case,
            )
        }),
    }
}

fn category_metrics(
    suite: &BenchmarkSuite,
    report: &BenchmarkReport,
) -> std::collections::BTreeMap<String, NormalizedMetrics> {
    let categories: std::collections::BTreeSet<_> = suite
        .cases
        .iter()
        .flat_map(|case| case.categories.iter())
        .collect();
    categories
        .into_iter()
        .map(|category| {
            let cases = suite
                .cases
                .iter()
                .zip(&report.cases)
                .filter(|(case, _)| case.categories.contains(category))
                .map(|(_, result)| result);
            (category.clone(), normalize_cases(cases, None))
        })
        .collect()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod machine_tests;

#[cfg(test)]
mod observer_tests;
