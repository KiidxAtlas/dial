//! Turn-local request policy. Settings are operator owned; observations never admit candidates.
use crate::config::adaptive::Phase;

use crate::{
    config::types::Config as AppConfig,
    learn::task_policy::Kind,
    llm::profiles::{ModelProfile, ReasoningApi, ReasoningEffort},
};
use serde::Serialize;
use serde_json::{Value, json};

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
fn on_off(value: &Value) -> &'static str {
    if value.as_bool() == Some(true) {
        "on"
    } else {
        "off"
    }
}
fn yes_no(value: &Value) -> &'static str {
    if value.as_bool() == Some(true) {
        "yes"
    } else {
        "no"
    }
}
/// Request-owned local controls only; never expose arbitrary profile/template data or credentials.
pub fn local_request_settings(
    request: &crate::llm::Chat,
    profile: Option<&ModelProfile>,
    local: &Value,
    source: &str,
) -> Value {
    let temperature = if profile.is_none_or(|p| p.capabilities.temperature) {
        profile
            .and_then(|p| p.temperature)
            .or(Some(request.temperature))
    } else {
        None
    };
    let sampling = profile.and_then(|p| p.local_sampling.as_ref());
    let thinking = local["thinking"].as_bool().or_else(|| {
        profile.and_then(|p| p.thinking).or_else(|| {
            sampling
                .and_then(|s| s.chat_template_kwargs.as_ref())
                .and_then(|kwargs| kwargs.get("enable_thinking"))
                .and_then(Value::as_bool)
        })
    });
    json!({
        "temperature": temperature,
        "top_p": profile.and_then(|p| p.top_p),
        "top_k": sampling.and_then(|s| s.top_k),
        "min_p": sampling.and_then(|s| s.min_p),
        "thinking": thinking,
        "reasoning_effort": profile.and_then(|p| p.reasoning_effort),
        "thinking_budget": profile.and_then(|p| p.thinking_budget),
        "max_tokens": request.max_tokens,
        "loop_penalty": local["loop_penalty"],
        "context_window": profile.and_then(|p| p.context_window),
        "sampling_source": source,
    })
}

/// Compact live label or detailed change notice. Null means delegated, not a measured default.
pub fn render_request_settings(settings: &Value, compact: bool) -> Option<String> {
    if !settings.is_object() {
        return None;
    }
    let number = |key: &str| {
        settings[key]
            .as_number()
            .map(ToString::to_string)
            .unwrap_or_else(|| "server default".into())
    };
    let thinking = match settings["thinking"].as_bool() {
        Some(true) => "on",
        Some(false) => "off",
        None => "model default",
    };
    let short = format!(
        "temp {} · think {thinking} · out {}",
        number("temperature"),
        number("max_tokens")
    );
    if compact {
        return Some(short);
    }
    Some(format!(
        "{short}\nTop-p {} · top-k {} · min-p {}\nSampling: {}\nReasoning effort {} · thinking budget {} · loop penalty {}\nReasoning guard: {} bytes · pre-action {}s (0 = unlimited)\nRequested overrides; server defaults are not measured. /policy shows the last request.",
        number("top_p"),
        number("top_k"),
        number("min_p"),
        settings["sampling_source"].as_str().unwrap_or("unknown"),
        settings["reasoning_effort"]
            .as_str()
            .unwrap_or("model default"),
        number("thinking_budget"),
        if settings["loop_penalty"].as_bool() == Some(true) {
            "on"
        } else {
            "off"
        },
        number("max_reasoning_bytes"),
        number("max_pre_action_secs"),
    ))
}

pub fn render(value: &Value) -> String {
    let last = &value["last_request"];
    let s = &last["selection"];
    if !s.is_object() {
        return format!(
            "Adaptive policy {} · phase sampling {} · baseline pinned {}\nNo request policy recorded yet. /settings → Model → Adaptive policy.",
            on_off(&value["config"]["enabled"]),
            on_off(&value["config"]["sampling"]),
            yes_no(&value["config"]["pin_sampling"])
        );
    }
    let effective = render_request_settings(&s["effective"], false)
        .map(|text| format!("\nLocal request settings:\n{text}"))
        .unwrap_or_default();
    format!(
        "Adaptive policy {} · phase sampling {} · baseline pinned {}\nLast request: {} → {}\n{}\nTemperature {} · top_p {} · output {} tokens · pre-action {}s\nSampling: {}\nReasoning: {} · {}\nLast execution: {}{effective}\n/settings → Model → Adaptive policy: enable, sampling, pin baseline, phase and budgets.\nExperimental phase presets; observed outcomes do not automatically promote sampler settings.",
        on_off(&value["config"]["enabled"]),
        on_off(&value["config"]["sampling"]),
        yes_no(&value["config"]["pin_sampling"]),
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
mod tests;
