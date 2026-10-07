//! Turn-local request policy. Settings are operator owned; observations never admit candidates.
use crate::{
    config::types::Config as AppConfig,
    learn::task_policy::Kind,
    llm::profiles::{ModelProfile, ReasoningApi, ReasoningEffort},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Auto,
    Investigation,
    Editing,
    Verification,
    Recovery,
    Exploration,
    General,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Investigation => "investigation",
            Self::Editing => "editing",
            Self::Verification => "verification",
            Self::Recovery => "recovery",
            Self::Exploration => "exploration",
            Self::General => "general",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub sampling: bool,
    pub reasoning: bool,
    pub output_limits: bool,
    pub pin_sampling: bool,
    pub phase: Phase,
    pub investigation_temperature: f64,
    pub editing_temperature: f64,
    pub verification_temperature: f64,
    pub recovery_temperature: f64,
    pub exploration_temperature: f64,
    pub investigation_tokens: u32,
    pub verification_tokens: u32,
    pub recovery_tokens: u32,
    pub exploration_tokens: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            sampling: false,
            reasoning: true,
            output_limits: false,
            pin_sampling: false,
            phase: Phase::Auto,
            investigation_temperature: 0.2,
            editing_temperature: 0.15,
            verification_temperature: 0.05,
            recovery_temperature: 0.1,
            exploration_temperature: 0.5,
            investigation_tokens: 2048,
            verification_tokens: 1536,
            recovery_tokens: 2048,
            exploration_tokens: 4096,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        for t in [
            self.investigation_temperature,
            self.editing_temperature,
            self.verification_temperature,
            self.recovery_temperature,
            self.exploration_temperature,
        ] {
            ensure!(
                t.is_finite() && (0.0..=2.0).contains(&t),
                "adaptive temperatures must be finite in 0..2"
            );
        }
        for n in [
            self.investigation_tokens,
            self.verification_tokens,
            self.recovery_tokens,
            self.exploration_tokens,
        ] {
            ensure!(
                (256..=32768).contains(&n),
                "adaptive output budgets must be 256..32768"
            );
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct State {
    pub phase: Phase,
    pub reason: String,
    mutated: bool,
}
impl State {
    pub fn begin(kind: Kind, objective: &str) -> Self {
        let words = objective.to_lowercase();
        let exploration = words
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| matches!(w, "brainstorm" | "alternatives" | "compare" | "design"));
        let phase = if exploration {
            Phase::Exploration
        } else if kind == Kind::General {
            Phase::General
        } else {
            Phase::Investigation
        };
        Self {
            phase,
            reason: format!("initial {} task", kind.label()),
            mutated: false,
        }
    }
    pub fn recover(&mut self, reason: &str) {
        self.phase = Phase::Recovery;
        self.reason = reason.into();
    }
    pub fn observe(&mut self, name: &str, success: bool, check: bool) {
        if !success {
            self.recover("last tool failed; correct the observed failure");
            return;
        }
        if check {
            self.phase = Phase::Verification;
            self.reason = "a recognized check ran; inspect its evidence".into();
        } else if matches!(name, "write" | "edit" | "ast_edit" | "solve") {
            self.mutated = true;
            self.phase = Phase::Editing;
            self.reason = "a source mutation succeeded; continue focused implementation".into();
        } else if matches!(name, "read" | "grep" | "glob" | "find" | "ast_search") {
            self.phase = if self.mutated {
                Phase::Editing
            } else {
                Phase::Investigation
            };
            self.reason = if self.mutated {
                "inspected implementation evidence after an edit"
            } else {
                "a source observation succeeded"
            }
            .into();
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Selection {
    pub enabled: bool,
    pub task: String,
    pub phase: Phase,
    pub reason: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub sampling_source: String,
    pub max_tokens: u32,
    pub pre_action_secs: u64,
    pub context_bytes: usize,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_source: String,
    #[serde(skip)]
    pub profile: Option<ModelProfile>,
}
/// Baseline request controls and execution context before adaptive policy is applied.
pub struct Request<'a> {
    pub kind: Kind,
    pub temperature: f64,
    pub tokens: u32,
    pub pre_action_secs: u64,
    pub base_profile: Option<&'a ModelProfile>,
    pub recovering_output: bool,
    pub hermetic: bool,
}

pub fn select(config: &AppConfig, state: &State, request: Request<'_>) -> Selection {
    let Request {
        kind,
        temperature,
        tokens,
        pre_action_secs,
        base_profile,
        recovering_output,
        hermetic,
    } = request;
    let settings = &config.adaptive;
    let enabled = settings.enabled && !hermetic;
    let phase = if enabled && settings.phase != Phase::Auto {
        settings.phase
    } else {
        state.phase
    };
    let local = config.endpoint.is_none() && config.provider.as_deref().is_none_or(|p| p == "mlx");
    let mut profile = base_profile.cloned();
    if enabled && profile.is_none() && local {
        profile = Some(ModelProfile {
            provider: "mlx".into(),
            model: config.model.clone(),
            capabilities: crate::llm::profiles::ModelCapabilities {
                temperature: true,
                top_p: true,
                seed: true,
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let temp_supported = profile
        .as_ref()
        .is_some_and(|p| p.capabilities.temperature && p.thinking != Some(true));
    let mut effective_temp = if profile.is_none() {
        if local { Some(temperature) } else { None }
    } else if temp_supported {
        Some(
            profile
                .as_ref()
                .and_then(|p| p.temperature)
                .unwrap_or(temperature),
        )
    } else {
        None
    };
    let mut top_p = profile.as_ref().and_then(|p| p.top_p);
    let mut sampling_source = if !temp_supported {
        "unchanged; sampling capability unverified or unsupported"
    } else if settings.pin_sampling {
        "operator pinned baseline sampling"
    } else {
        "baseline sampling"
    }
    .to_owned();
    if enabled && settings.sampling && !settings.pin_sampling && temp_supported {
        if profile.as_ref().is_some_and(|p| p.temperature.is_some()) {
            sampling_source = "explicit model-profile temperature".into();
        } else {
            effective_temp = Some(match phase {
                Phase::Investigation => settings.investigation_temperature,
                Phase::Editing => settings.editing_temperature,
                Phase::Verification => settings.verification_temperature,
                Phase::Recovery => settings.recovery_temperature,
                Phase::Exploration => settings.exploration_temperature,
                _ => temperature,
            });
            if let Some(p) = profile.as_mut() {
                p.temperature = effective_temp;
            }
            sampling_source = "experimental operator-enabled phase preset".into();
        }
        if let Some(p) = profile.as_mut()
            && p.capabilities.top_p
            && p.top_p.is_none()
        {
            p.top_p = Some(match phase {
                Phase::Editing | Phase::Recovery => 0.9,
                Phase::Verification => 0.8,
                Phase::Exploration => 1.0,
                _ => 0.95,
            });
            top_p = p.top_p;
        }
    }
    let mut reasoning_source = "provider default or explicit profile".to_owned();
    if enabled
        && settings.reasoning
        && let Some(p) = profile.as_mut()
        && p.reasoning_effort.is_none()
        && p.thinking.is_none()
        && matches!(
            p.capabilities.reasoning,
            ReasoningApi::OpenaiEffort | ReasoningApi::OpenaiResponses
        )
    {
        let preferences = match phase {
            Phase::Editing | Phase::Verification | Phase::Recovery | Phase::General => [
                ReasoningEffort::Low,
                ReasoningEffort::Minimal,
                ReasoningEffort::Medium,
            ],
            Phase::Exploration => [
                ReasoningEffort::High,
                ReasoningEffort::Medium,
                ReasoningEffort::Low,
            ],
            _ => [
                ReasoningEffort::Medium,
                ReasoningEffort::Low,
                ReasoningEffort::High,
            ],
        };
        p.reasoning_effort = preferences
            .into_iter()
            .find(|e| p.capabilities.reasoning_efforts.contains(e));
        if p.reasoning_effort.is_some() {
            reasoning_source = "phase preference within declared model capabilities".into();
        }
    }
    let explicit_output = base_profile.is_some_and(|p| p.max_tokens.is_some());
    let reasoning_model = profile.as_ref().is_some_and(|p| {
        p.capabilities.reasoning != ReasoningApi::None && p.thinking != Some(false)
    });
    let phase_tokens = match phase {
        Phase::Investigation => settings.investigation_tokens,
        Phase::Verification => settings.verification_tokens,
        Phase::Recovery => settings.recovery_tokens,
        Phase::Exploration => settings.exploration_tokens,
        _ => tokens,
    };
    let max_tokens = if enabled
        && settings.output_limits
        && !recovering_output
        && !explicit_output
        && !reasoning_model
    {
        tokens.min(phase_tokens)
    } else {
        tokens
    };
    let secs = match phase {
        Phase::Editing => 25,
        Phase::Verification => 15,
        Phase::Recovery => 20,
        Phase::Exploration => 60,
        _ => 40,
    };
    let pre_action_secs = if enabled && settings.reasoning {
        pre_action_secs.min(secs)
    } else {
        pre_action_secs
    };
    let context_bytes = if enabled {
        match phase {
            Phase::Verification => 2048,
            Phase::Recovery => 3072,
            _ => 4096,
        }
    } else {
        4096
    };
    Selection {
        enabled,
        task: kind.label().into(),
        phase,
        reason: if enabled && settings.phase != Phase::Auto {
            "operator pinned phase".into()
        } else {
            state.reason.clone()
        },
        temperature: effective_temp,
        top_p,
        sampling_source,
        max_tokens,
        pre_action_secs,
        context_bytes,
        reasoning_effort: profile.as_ref().and_then(|p| p.reasoning_effort),
        reasoning_source,
        profile,
    }
}
pub fn report(config: &AppConfig, control: &Value) -> Value {
    json!({"config":config.adaptive,"last_request":control["adaptive_policy"],"history":control["adaptive_history"],"scope":"request settings and observed execution metrics; no automatic sampler promotion or accuracy certification"})
}
pub fn render(value: &Value) -> String {
    let last = &value["last_request"];
    let s = &last["selection"];
    if !s.is_object() {
        return format!(
            "Adaptive policy: enabled {} · phase sampling {} · pinned baseline {}\nNo request policy recorded yet. /settings → Model → Adaptive policy.",
            value["config"]["enabled"],
            value["config"]["sampling"],
            value["config"]["pin_sampling"]
        );
    }
    format!(
        "Adaptive enabled {} · phase sampling {} · baseline pinned {}\nLast request: {} → {}\n{}\nTemperature {} · top_p {} · output {} tokens · pre-action {}s\nSampling: {}\nReasoning: {} · {}\nLast execution: {}\n/settings → Model → Adaptive policy: enable, sampling, pin baseline, phase and budgets.\nExperimental phase presets; observed outcomes do not automatically promote sampler settings.",
        value["config"]["enabled"],
        value["config"]["sampling"],
        value["config"]["pin_sampling"],
        s["task"].as_str().unwrap_or("unknown"),
        s["phase"].as_str().unwrap_or("unknown"),
        s["reason"].as_str().unwrap_or(""),
        s["temperature"]
            .as_f64()
            .map(|x| x.to_string())
            .unwrap_or("unverified/provider-controlled".into()),
        s["top_p"]
            .as_f64()
            .map(|x| x.to_string())
            .unwrap_or("provider default".into()),
        s["max_tokens"],
        s["pre_action_secs"],
        s["sampling_source"].as_str().unwrap_or("unknown"),
        s["reasoning_effort"].as_str().unwrap_or("provider default"),
        s["reasoning_source"].as_str().unwrap_or("unknown"),
        last["result"]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> AppConfig {
        AppConfig {
            model: "test".into(),
            ..Default::default()
        }
    }
    fn choose(
        c: &AppConfig,
        s: &State,
        p: Option<&ModelProfile>,
        recover: bool,
        hermetic: bool,
    ) -> Selection {
        select(
            c,
            s,
            Request {
                kind: Kind::Editing,
                temperature: 0.4,
                tokens: 8192,
                pre_action_secs: 60,
                base_profile: p,
                recovering_output: recover,
                hermetic,
            },
        )
    }
    #[test]
    fn task_phase_follows_observed_work_and_recovery_is_not_a_quality_label() {
        let mut s = State::begin(Kind::Editing, "Implement change");
        assert_eq!(s.phase, Phase::Investigation);
        s.observe("write", true, false);
        assert_eq!(s.phase, Phase::Editing);
        s.observe("bash", true, true);
        assert_eq!(s.phase, Phase::Verification);
        s.observe("read", false, false);
        assert_eq!(s.phase, Phase::Recovery);
        s.observe("todo", true, false);
        assert_eq!(s.phase, Phase::Recovery);
        s.observe("read", true, false);
        assert_eq!(s.phase, Phase::Editing);
    }
    #[test]
    fn default_output_budget_is_not_reduced_by_phase() {
        let c = config();
        assert!(!c.adaptive.output_limits);
        for phase in [
            Phase::Investigation,
            Phase::Editing,
            Phase::Verification,
            Phase::Recovery,
            Phase::Exploration,
        ] {
            let mut s = State::begin(Kind::Editing, "Implement change");
            s.phase = phase;
            assert_eq!(choose(&c, &s, None, false, false).max_tokens, 8192);
        }
    }
    #[test]
    fn budgets_respect_operator_limits_and_length_recovery_escapes_phase_cap() {
        let mut c = config();
        c.adaptive.output_limits = true;
        let mut s = State::begin(Kind::Investigation, "Locate symbol");
        let first = choose(&c, &s, None, false, false);
        assert_eq!(first.max_tokens, 2048);
        assert_eq!(first.temperature, Some(0.4));
        let smaller = select(
            &c,
            &s,
            Request {
                kind: Kind::Editing,
                temperature: 0.4,
                tokens: 512,
                pre_action_secs: 3,
                base_profile: None,
                recovering_output: false,
                hermetic: false,
            },
        );
        assert_eq!(smaller.max_tokens, 512);
        assert_eq!(smaller.pre_action_secs, 3);
        s.recover("truncated");
        assert_eq!(choose(&c, &s, None, true, false).max_tokens, 8192);
        assert_eq!(choose(&c, &s, None, false, true).max_tokens, 8192);
    }
    #[test]
    fn phase_sampling_is_opt_in_and_pins_and_unknown_capabilities_win() {
        let mut c = config();
        let mut s = State::begin(Kind::Editing, "Change");
        s.observe("write", true, false);
        assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.4));
        c.adaptive.sampling = true;
        assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.15));
        c.adaptive.pin_sampling = true;
        assert_eq!(choose(&c, &s, None, false, false).temperature, Some(0.4));
        c.adaptive.pin_sampling = false;
        let profile = ModelProfile {
            provider: "mlx".into(),
            model: "test".into(),
            temperature: Some(0.7),
            top_p: Some(0.99),
            max_tokens: Some(8192),
            capabilities: crate::llm::profiles::ModelCapabilities {
                temperature: true,
                top_p: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let pinned = choose(&c, &s, Some(&profile), false, false);
        assert_eq!(pinned.temperature, Some(0.7));
        assert_eq!(pinned.top_p, Some(0.99));
        assert_eq!(pinned.max_tokens, 8192);
        let unsupported = ModelProfile {
            temperature: None,
            capabilities: Default::default(),
            ..profile
        };
        assert!(
            choose(&c, &s, Some(&unsupported), false, false)
                .temperature
                .is_none()
        );
    }
    #[test]
    fn reasoning_only_uses_declared_values_and_never_clips_thinking_budget() {
        let c = config();
        let mut s = State::begin(Kind::Editing, "Implement");
        s.observe("write", true, false);
        let mut p = ModelProfile {
            provider: "test".into(),
            model: "test".into(),
            capabilities: crate::llm::profiles::ModelCapabilities {
                reasoning: ReasoningApi::OpenaiEffort,
                reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                ..Default::default()
            },
            ..Default::default()
        };
        let chosen = choose(&c, &s, Some(&p), false, false);
        assert_eq!(chosen.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(chosen.max_tokens, 8192);
        p.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(
            choose(&c, &s, Some(&p), false, false).reasoning_effort,
            Some(ReasoningEffort::High)
        );
        p.capabilities.reasoning = ReasoningApi::AnthropicBudget;
        p.capabilities.reasoning_efforts.clear();
        p.reasoning_effort = None;
        p.thinking = Some(true);
        p.thinking_budget = Some(4096);
        let chosen = choose(&c, &s, Some(&p), false, false);
        assert_eq!(chosen.max_tokens, 8192);
        assert!(chosen.temperature.is_none());
        assert!(chosen.profile.unwrap().thinking_budget == Some(4096));
    }
    #[test]
    fn phase_pin_and_invalid_configuration_are_bounded() {
        let mut c = config();
        c.adaptive.phase = Phase::Verification;
        let s = State::begin(Kind::General, "hello");
        assert_eq!(
            choose(&c, &s, None, false, false).phase,
            Phase::Verification
        );
        c.adaptive.verification_tokens = 0;
        assert!(c.adaptive.validate().is_err());
        c.adaptive.verification_tokens = 1536;
        c.adaptive.editing_temperature = f64::INFINITY;
        assert!(c.adaptive.validate().is_err());
    }
}
