//! The facts the status line can show. The controller fills this in; the status line renders from
//! it. `None`/empty means *unknown* and is shown as absent, never as 0 or "free".

use super::statusline::{elapsed, human};

/// ` · ~12s left` from the measured rate; nothing while the estimate is unknown or not positive.
fn push_eta(label: &mut String, eta: Option<f64>) {
    if let Some(secs) = eta.filter(|s| s.is_finite() && *s > 0.0) {
        label.push_str(&format!(" · ~{} left", elapsed(secs.ceil())));
    }
}

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
    /// Short status-line label; empty when unknown or when the model cannot take adapters at all
    /// (a remote model has no LoRA to report).
    pub fn label(self) -> String {
        match self {
            Lora::Unknown | Lora::Unsupported => String::new(),
            Lora::Off => "LoRA off".into(),
            Lora::Ready => "LoRA awaiting training".into(),
            Lora::Unverified => "LoRA not checked yet".into(),
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
    /// Current context occupancy, including streamed output and newly added evidence.
    pub prompt_tokens: Option<u64>,
    pub context_estimated: bool,
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
    pub model_progress: Option<crate::llm::ModelProgress>,
    pub model_silence_secs: u64,
    /// Monotonic animation counter (about 12 per second) for spinners.
    pub tick: u64,
    pub mode: InputMode,
    /// Wall-clock `HH:MM` for the `time` segment, empty to hide.
    pub clock: String,
    /// One-line usage summary from cached provider reports (`usage_report::status_summary`);
    /// `None` hides the `usage` segment. Filled by the app from cache only, never from the network.
    pub usage: Option<String>,
    /// Running and total workers for the `workers` segment; `None` hides it. Filled by the app from the worker snapshot cache.
    pub workers: Option<super::worker_pins::WorkerTally>,
}

impl StatusData {
    /// Prefer lifecycle state over stale model telemetry, especially while stopping.
    pub fn activity_label(&self) -> String {
        let activity = crate::ui::text::one_line(&self.activity);
        if self.stopping() {
            return activity;
        }
        match self.phase {
            Phase::Compacting => return "compacting context".into(),
            Phase::WaitingCompute => {
                return if activity.trim().is_empty() {
                    "waiting for local compute".into()
                } else {
                    activity
                };
            }
            _ => {}
        }
        self.model_activity().unwrap_or_else(|| {
            if activity.trim().is_empty() {
                "working".into()
            } else {
                activity
            }
        })
    }

    pub fn stopping(&self) -> bool {
        self.activity.starts_with("stopping") || self.activity.starts_with("cancelling")
    }

    /// Engine-reported prompt progress only; an unknown total is not a percentage.
    pub fn prompt_fraction(&self) -> Option<f64> {
        let p = self.model_progress.as_ref()?;
        if p.phase != crate::llm::InferencePhase::Processing || self.stopping() {
            return None;
        }
        let (done, total) = (p.processed_tokens?, p.prompt_tokens?);
        (total > 0).then(|| done.min(total) as f64 / total as f64)
    }

    pub fn model_activity(&self) -> Option<String> {
        use crate::llm::InferencePhase;
        let p = self.model_progress.as_ref()?;
        let mut label = match p.phase {
            InferencePhase::Loading => "loading model".to_owned(),
            InferencePhase::Processing => {
                let mut label = match (p.processed_tokens, p.prompt_tokens) {
                    (Some(done), Some(total)) if total > 0 => format!(
                        "processing prompt {}/{total} tokens ({:.0}%)",
                        done.min(total),
                        done.min(total) as f64 * 100.0 / total as f64
                    ),
                    (_, Some(total)) => format!("processing prompt {total} tokens"),
                    _ => "processing prompt".into(),
                };
                if let Some(cached) = p.cached_tokens.filter(|n| *n > 0) {
                    label.push_str(&format!(" · {} cached", human(cached)));
                }
                push_eta(&mut label, p.eta_secs);
                label
            }
            InferencePhase::Generating => {
                let prefix = if p.estimated { "~" } else { "" };
                let mut label = p.output_tokens.map_or_else(
                    || "generating tokens".into(),
                    |n| format!("generating {prefix}{n} tokens"),
                );
                if let Some(rate) = p.tokens_per_second.filter(|r| r.is_finite() && *r > 0.0) {
                    label.push_str(&format!(" · {prefix}{rate:.1} tok/s"));
                }
                push_eta(&mut label, p.eta_secs);
                if self.activity == "reasoning"
                    || self.activity == "writing"
                    || self.activity.starts_with("preparing ")
                {
                    label = format!("{} · {label}", self.activity);
                }
                label
            }
        };
        if self.model_silence_secs >= 15 {
            label = format!("no progress for {}s · {label}", self.model_silence_secs);
        }
        // Invalid telemetry cannot introduce terminal control sequences.
        label = crate::ui::text::one_line(&label);
        Some(label)
    }

    pub fn live_output_tokens(&self) -> (u64, bool) {
        match &self.model_progress {
            Some(p) => (
                self.output_tokens
                    .saturating_add(p.output_tokens.unwrap_or(0)),
                p.estimated,
            ),
            None => (self.output_tokens, false),
        }
    }

    pub fn live_tokens_per_second(&self) -> Option<f64> {
        match &self.model_progress {
            Some(p) => p.tokens_per_second,
            None => self.tokens_per_second,
        }
    }

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

#[cfg(test)]
mod tests;
