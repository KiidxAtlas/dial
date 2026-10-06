//! The facts the status line can show. The controller fills this in; the status line renders from
//! it. `None`/empty means *unknown* and is shown as absent, never as 0 or "free".

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// A model turn or a tool is running.
    Working,
    Compacting,
    /// Waiting for another process to release local compute.
    WaitingCompute,
    Setup,
}

/// Which special input mode the composer is in (changes its border color).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Normal,
    Shell,
}

/// Whether the main model can be, or is being, improved with LoRA weight training.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Lora {
    /// Not known yet (the status has not been read, or mlx-lm is not installed).
    #[default]
    Unknown,
    /// The model cannot take a LoRA adapter here: an external server serves it, or mlx-lm lacks its architecture.
    Unsupported,
    /// Compatible, but weight training is disabled in the configuration.
    Off,
    /// Compatible and enabled; no adapter has been admitted yet.
    Ready,
    /// MLX implements the architecture, but Dial has not verified adapter training targets.
    Unverified,
    VerificationQueued,
    Verifying,
    /// Accepted version; runtime loading has not been confirmed.
    Active(i64),
    Training,
    Paused,
    Evaluating,
    Loading,
    Loaded,
    Used,
    Failed,
}

impl Lora {
    /// Short status-line label; empty when unknown.
    pub fn label(self) -> String {
        match self {
            Lora::Unknown => String::new(),
            Lora::Unsupported => "no LoRA".into(),
            Lora::Off => "LoRA off".into(),
            Lora::Ready => "LoRA awaiting training".into(),
            Lora::Unverified => "LoRA unverified".into(),
            Lora::VerificationQueued => "LoRA waiting for compute".into(),
            Lora::Verifying => "LoRA verifying".into(),
            Lora::Active(id) => format!("LoRA accepted v{id}"),
            Lora::Training => "LoRA training".into(),
            Lora::Paused => "LoRA paused for coding".into(),
            Lora::Evaluating => "LoRA evaluating".into(),
            Lora::Loading => "LoRA loading".into(),
            Lora::Loaded => "LoRA loaded".into(),
            Lora::Used => "LoRA used on last response".into(),
            Lora::Failed => "LoRA request failed".into(),
        }
    }

    pub fn with_runtime(self, runtime: &crate::llm::provider::AdapterRuntime) -> Self {
        use crate::llm::provider::AdapterPhase;
        if matches!(self, Self::Training | Self::Paused | Self::Evaluating) {
            return self;
        }
        if runtime.adapter.is_none() {
            return self;
        }
        match runtime.phase {
            AdapterPhase::Idle => self,
            AdapterPhase::Loading => Self::Loading,
            AdapterPhase::Loaded => Self::Loaded,
            AdapterPhase::Used => Self::Used,
            AdapterPhase::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub provider: String,
    pub model: String,
    /// Configured reasoning level, toggle or provider default; empty if metadata is unavailable.
    pub thinking: String,
    /// Which role the running request uses (`main`, `subagent`, …); empty when idle on main.
    pub role: String,
    /// Absolute project path; the segment abbreviates `$HOME` to `~`.
    pub project_path: String,
    pub branch: String,
    /// (staged, modified, untracked) when git status is known.
    pub git_counts: Option<(u32, u32, u32)>,
    pub session: String,
    /// Prompt tokens of the last request, with the window actually available.
    pub prompt_tokens: Option<u64>,
    pub context_window: u32,
    pub output_tokens: u64,
    pub tokens_per_second: Option<f64>,
    pub approval: String,
    pub sandbox: String,
    /// Admitted version id of the learning loop, if it is active.
    pub version: Option<i64>,
    pub learning: String,
    pub lora: Lora,
    pub phase: Phase,
    pub elapsed_secs: f64,
    /// What the running turn is doing right now (`thinking`, a tool name, …); empty when unknown.
    pub activity: String,
    /// Monotonic animation counter (about 12 per second) for spinners.
    pub tick: u64,
    pub mode: InputMode,
    /// Wall-clock `HH:MM` for the `time` segment, empty to hide.
    pub clock: String,
}

impl StatusData {
    pub fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Working | Phase::Compacting | Phase::WaitingCompute
        )
    }

    /// Fraction of the context window the last prompt used, when both are known.
    pub fn context_fraction(&self) -> Option<f64> {
        let used = self.prompt_tokens?;
        (self.context_window > 0)
            .then(|| (used as f64 / f64::from(self.context_window)).clamp(0.0, 1.0))
    }
}
