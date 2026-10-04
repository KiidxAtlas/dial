//! The facts the status line can show. The controller fills this in; the status line renders from
//! it. `None`/empty means *unknown* and is shown as absent, never as 0 or "free".

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default] Idle,
    /// A model turn or a tool is running.
    Working,
    Compacting,
    /// Waiting for another process to release local compute.
    WaitingCompute,
    Setup,
}

/// Which special input mode the composer is in (changes its border color).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InputMode { #[default] Normal, Shell }

#[derive(Clone, Debug, Default)]
pub struct StatusData {
    pub provider: String,
    pub model: String,
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
    pub fn busy(&self) -> bool { matches!(self.phase, Phase::Working | Phase::Compacting | Phase::WaitingCompute) }

    /// Fraction of the context window the last prompt used, when both are known.
    pub fn context_fraction(&self) -> Option<f64> {
        let used = self.prompt_tokens?;
        (self.context_window > 0).then(|| (used as f64 / f64::from(self.context_window)).clamp(0.0, 1.0))
    }
}
