//! Agent workflow sections: worktrees, the commit cache, experiments, goal mode, named agents and
//! spend caps.
use super::*;

/// `[goal]`: goal-mode (`/goal`) options.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GoalConfig {
    /// Workspace-relative file that mirrors the goal, todos and working state after each goal-mode
    /// turn, for example `.dial/progress.md`. It is a user-visible deliverable written through the
    /// workspace's confinement and checkpoints, never private state. Empty writes nothing.
    pub progress_file: String,
}

impl GoalConfig {
    /// The progress file is a plain relative path inside the workspace: no absolute path, root,
    /// `..` component or control character.
    pub fn validate(&self) -> anyhow::Result<()> {
        let path = self.progress_file.as_str();
        if path.is_empty() {
            return Ok(());
        }
        let relative = std::path::Path::new(path);
        anyhow::ensure!(
            !path.contains(|c: char| c.is_control())
                && relative.components().all(|component| {
                    matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
                && relative
                    .components()
                    .any(|component| matches!(component, std::path::Component::Normal(_))),
            "goal.progress_file must be a workspace-relative file path without `..`, for example .dial/progress.md"
        );
        Ok(())
    }
}

/// `[worktree]`: behavior of `/wt` worktrees.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// After `/wt` moves uncommitted changes into the new worktree, reset tracked files and remove
    /// untracked, non-ignored files from the source checkout.
    pub clean_source: bool,
}

/// `[commit]`: persistent reply cache for `/commit` and `dial commit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommitConfig {
    /// Reuse stored model replies for identical requests.
    pub cache_enabled: bool,
    /// Lifetime of a stored reply in days.
    pub cache_ttl_days: f64,
}

impl Default for CommitConfig {
    fn default() -> Self {
        Self {
            cache_enabled: true,
            cache_ttl_days: 14.0,
        }
    }
}

/// Experimental agent workflow features. Every feature is disabled unless explicitly enabled,
/// except `repro_first = advise`, `streaming_dispatch` and `dependency_dispatch`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExperimentalConfig {
    /// Native Anthropic effort adaptation; explicit output-effort profiles remain authoritative.
    pub adaptive_output_effort: bool,
    /// Offer bounded, approved video frame investigations; image tokens can be expensive.
    pub video_analysis: bool,
    pub work_packet_contracts: bool,
    pub verification_receipts: bool,
    pub context_overlap_dedup: bool,
    pub profile_matching: bool,
    pub tool_speculation: bool,
    /// Rewrite each user message with the `turn_drafter` role before the main model sees it.
    pub turn_optimizer: bool,
    /// Let the turn drafter add Mermaid diagrams that restate a described flow or structure.
    pub turn_drafter_diagrams: bool,
    /// Optional prompt guidance, independent of provider thinking controls and permissions.
    pub reasoning_strategy: ReasoningStrategy,
    /// Opt in to a diagnostic comparison of an operator-declared behavioral check against
    /// turn-start files. At most one attempt per turn, with a command deadline capped at 60s.
    /// Related suites and registered reproductions are not automatically rerun.
    pub fault_witness: bool,
    /// On debugging tasks whose request asks for verification, the first edit to a non-test
    /// source file before any reproduction has failed gets advice (`advise`) or is refused once
    /// (`gate`).
    pub repro_first: ReproFirst,
    /// Isolated check runs allowed to locate the changed files that turned a passing focused check
    /// into a failing one; 0 disables bisection.
    pub edit_bisection_runs: u8,
    /// Consecutive edit rounds without a lower focused-check failure score before a no-progress
    /// note; 0 disables it.
    pub outcome_gradient_rounds: u8,
    /// Start admitted read-only calls as soon as their arguments finish streaming, before the
    /// assistant reply ends. Results are delivered in call order after the reply completes.
    pub streaming_dispatch: bool,
    /// Present exact native edit calls as a finite one-shot approval batch.
    pub batch_approvals: bool,
    /// Schedule only operations with trusted resource contracts across independent calls.
    /// Enabled by default; `false` serializes native mutations (read batching remains).
    pub dependency_dispatch: bool,
    /// Start configured focused verification after committed edits, overlapping safe reads only.
    pub background_checks: bool,
    /// Offer `check.expect` (pass/fail prediction); a mismatch is reported as a fact and the next
    /// request asks the model to account for it before editing again.
    pub check_predictions: bool,
}

impl Default for ExperimentalConfig {
    fn default() -> Self {
        Self {
            adaptive_output_effort: false,
            video_analysis: false,
            work_packet_contracts: false,
            verification_receipts: false,
            context_overlap_dedup: false,
            profile_matching: false,
            tool_speculation: false,
            turn_optimizer: false,
            turn_drafter_diagrams: false,
            reasoning_strategy: ReasoningStrategy::default(),
            fault_witness: false,
            repro_first: ReproFirst::default(),
            edit_bisection_runs: 0,
            outcome_gradient_rounds: 0,
            streaming_dispatch: true,
            batch_approvals: false,
            dependency_dispatch: true,
            background_checks: false,
            check_predictions: false,
        }
    }
}

/// `experimental.repro_first`: how debugging tasks are steered toward a failing reproduction
/// before the first source edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReproFirst {
    Off,
    /// One advisory note at the first unreproduced source edit.
    #[default]
    Advise,
    /// Refuse the first unreproduced source edit once; a repeated attempt proceeds.
    Gate,
}

/// Experimental reasoning guidance. Default preserves the existing prompt unchanged.
/// These presets do not expose or replace private reasoning or change inference budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStrategy {
    #[default]
    Default,
    /// Direct goals and explicit success criteria, inspired by OpenAI's public guidance.
    GptGoalDirected,
    /// Context-grounded work, inspired by Anthropic's public guidance.
    ClaudeGrounded,
    /// Context and example synthesis, inspired by Google's public guidance.
    GeminiSynthesis,
    /// Bounded self-verification, inspired by DeepSeek's published reasoning research.
    DeepseekCrossCheck,
    EvidenceFirst,
    HypothesisDriven,
    PlanThenAct,
    Decompose,
    CompareAlternatives,
    CritiqueAndRefine,
    VerificationFirst,
    MinimalChange,
}

/// A user-defined subagent built on a built-in type. Writers (`base = "worker"`) are allowed:
/// launching one still needs approval (agent names are registered as writers, fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// `scout`, `reviewer`, `planner`, `worker` or `test-runner`.
    #[serde(default = "default_agent_base")]
    pub base: String,
    /// One line shown to the model in the `task` description.
    #[serde(default)]
    pub description: String,
    /// Instructions appended to the base agent's prompt.
    pub prompt: String,
    /// Restrict the base agent's tools to these (never adds any).
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Agents this one may start (`["*"]` any; `[]` none); default: the base type's policy.
    #[serde(default)]
    pub spawns: Option<Vec<String>>,
    /// Model fallback list: `@role`, `provider/model[:level]` or a bare model name.
    #[serde(default)]
    pub model: Vec<String>,
    /// Thinking level: off, minimal, low, medium, high, xhigh or max.
    #[serde(default)]
    pub thinking_level: Option<String>,
    /// Default JSON Schema for the agent's structured result.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    /// Skills whose instructions are loaded into the agent's context at start.
    #[serde(default)]
    pub autoload_skills: Vec<String>,
}

fn default_agent_base() -> String {
    "scout".into()
}

/// Spend caps in USD, enforced between model requests. They need catalog pricing for the model and
/// reported token usage: a model with no known price is never stopped by them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Stop the turn once the session has cost this much. `0` (the default) or unset disables this cap.
    pub max_session_cost_usd: Option<f64>,
    /// Stop the turn once it alone has cost this much. `0` (the default) or unset disables this cap.
    pub max_turn_cost_usd: Option<f64>,
    /// Notify once when the session (subagents included) first reaches this spend. Warnings never
    /// stop work. `0` (the default) or unset disables the warning.
    pub warn_session_cost_usd: Option<f64>,
    /// Notify once when the current turn first reaches this spend. `0` (the default) or unset disables it.
    pub warn_turn_cost_usd: Option<f64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_session_cost_usd: Some(0.0),
            max_turn_cost_usd: Some(0.0),
            warn_session_cost_usd: Some(0.0),
            warn_turn_cost_usd: Some(0.0),
        }
    }
}
