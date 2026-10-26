//! Self-improvement: evaluation gates, LoRA training, the improvement loop and its records.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    pub suites: Vec<SuiteConfig>,
    /// Percentage points on a 0..100 scale, per suite.
    pub max_regression: f64,
    pub min_improvement: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrainingConfig {
    pub enabled: bool,
    /// Defaults to `<state>/grounded/train.jsonl` when empty.
    pub train_data: PathBuf,
    pub valid_data: PathBuf,
    pub iters: u32,
    pub batch_size: u32,
    pub num_layers: u32,
    pub rank: u32,
    pub max_seq_length: u32,
    pub interval_secs: u64,
    /// Sibling model DBs (matched by model name, or `provider/model`) whose externally verified
    /// trajectories and tool repairs may be learned from for this model's training data.
    #[serde(alias = "distill_from")]
    pub learn_from: Vec<String>,
    /// Learn from every sibling model DB in the project, not only `learn_from`.
    #[serde(alias = "distill_from_all_verified")]
    pub learn_from_all_verified: bool,
    /// Maximum share of a harvested batch that may come from sibling models (0-50).
    #[serde(alias = "distill_max_percent")]
    pub learn_from_max_percent: u32,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            train_data: PathBuf::new(),
            valid_data: PathBuf::new(),
            iters: 32,
            batch_size: 1,
            num_layers: 4,
            rank: 8,
            max_seq_length: 8192,
            interval_secs: 6 * 3600,
            learn_from: Vec::new(),
            learn_from_all_verified: false,
            learn_from_max_percent: 25,
        }
    }
}

/// Automatic self-improvement. On by default: dial records turns, runs verified self-play on
/// procedurally generated tasks, and proposes harness and LoRA changes that only a sealed gate can admit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImproveConfig {
    pub enabled: bool,
    /// "Teach this model": coding sessions with a local model record verified turns as training
    /// examples and run on the latest admitted version (harness and LoRA adapter). Off by default.
    pub teach: bool,
    /// Minimum seconds between background cycles.
    pub interval_secs: u64,
    /// A cycle needs at least this many new recorded turns (or tool-call repairs) since the last one.
    pub min_new_turns: u32,
    /// Verified self-play tasks per cycle (training data and weakness signal).
    pub selfplay_tasks: u32,
    pub gate_primary_cases: u32,
    pub gate_guard_cases: u32,
    /// One-sided exact sign-test level for the primary suite; admission also needs `min_net_wins`.
    pub alpha: f64,
    pub min_net_wins: u32,
    /// Candidates evaluated per cycle (harness and weight changes compete).
    pub max_candidates: u32,
    /// Percentage points a version may lose against the very first version on the frozen anchor suite.
    pub drift_tolerance: f64,
    /// Learn from live turns whose outcome was verified (your /outcome, passing checks, no /undo).
    pub learn_from_sessions: bool,
    /// Minimum verified training examples before a LoRA candidate is attempted.
    pub lora_min_examples: u32,
    /// Observational warning threshold: lower-bound net share of matched prompts that got worse,
    /// in percentage points, not an unpaired mean score drop or automatic rollback authority.
    pub monitor_drop: f64,
    /// Minimum distinct task-matched prompts before computing a live diagnostic.
    pub monitor_min_turns: u32,
    /// Run the background cycle at session start/exit, subject to idle, AC-power, budget and /compute pause gates. When false, only `dial improve cycle` runs it.
    pub background: bool,
    /// Start a background cycle only after this many minutes without a coding turn (0 = any time).
    pub idle_minutes: u32,
    /// Do not start background cycles on battery power.
    pub require_ac_power: bool,
    /// Background compute allowed per day, in minutes, enforced during a cycle too (0 = unlimited).
    pub daily_compute_minutes: u32,
    /// Maximum observed endpoint-model spend per background cycle/day in USD; zero disables background cloud work.
    pub max_daily_usd: f64,
    /// Failure-driven distillation: the `coach` role solves fresh tasks from weak train families and
    /// only verifier-passed teacher runs become training records (source `teacher-<model>`). Requires
    /// an assigned coach role. Off by default.
    pub distill: bool,
    /// Teacher self-play tasks per cycle when `distill` is on (1..=64).
    pub distill_tasks: u32,
}

impl Default for ImproveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            teach: false,
            interval_secs: 6 * 3600,
            min_new_turns: 5,
            selfplay_tasks: 24,
            gate_primary_cases: 30,
            gate_guard_cases: 20,
            alpha: 0.1,
            min_net_wins: 2,
            max_candidates: 0,
            drift_tolerance: 3.0,
            learn_from_sessions: true,
            lora_min_examples: 24,
            monitor_drop: 15.0,
            monitor_min_turns: 8,
            background: true,
            idle_minutes: 10,
            require_ac_power: true,
            daily_compute_minutes: 0,
            max_daily_usd: 0.0,
            distill: false,
            distill_tasks: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: i64,
    pub parent: Option<i64>,
    pub kind: String,
    pub harness: Harness,
    pub adapter: Option<PathBuf>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub trajectory: i64,
    pub version: i64,
    pub domain: String,
    pub task: String,
    pub step: u32,
    pub kind: String,
    pub data: serde_json::Value,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub trajectory: i64,
    /// Externally assigned score on a 0..100 scale.
    pub score: f64,
    pub source: String,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteScore {
    pub name: String,
    pub primary: bool,
    pub baseline: f64,
    pub candidate: f64,
    pub cases: usize,
    /// Paired outcomes: cases only the candidate passed / only the baseline passed.
    #[serde(default)]
    pub candidate_only: usize,
    #[serde(default)]
    pub baseline_only: usize,
    /// Primary-suite difficulty tier the baseline settled on (auto cycles only; `None` otherwise).
    #[serde(default)]
    pub tier: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admission {
    pub candidate: i64,
    pub baseline: i64,
    pub passed: bool,
    pub reason: String,
    pub scores: Vec<SuiteScore>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub diagnosis: String,
    /// "harness" or "lora". Other operations must be rejected.
    pub operation: String,
    pub harness: Option<Harness>,
}
