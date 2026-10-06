//! Operator-declared, exact-model capabilities. Unknown models are never classified by name.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}
impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningApi {
    #[default]
    None,
    OpenaiEffort,
    OpenaiResponses,
    AnthropicBudget,
    AnthropicAdaptive,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OutputLimit {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilities {
    pub temperature: bool,
    pub top_p: bool,
    pub seed: bool,
    pub tools: Option<bool>,
    /// Explicit endpoint support for strict JSON tool generation; never inferred by model name.
    pub strict_tools: bool,
    pub parallel_tool_calls: Option<bool>,
    pub vision: Option<bool>,
    pub reasoning: ReasoningApi,
    /// Only values documented by this provider for this exact model.
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub output_limit: OutputLimit,
    pub prompt_cache: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub capabilities: ModelCapabilities,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking: Option<bool>,
    pub thinking_budget: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub edit_mode: Option<crate::config::types::EditModeConfig>,
    /// An explicit allow-list of existing harness/integration tools.
    pub tools: Option<Vec<String>>,
    /// Enable Anthropic stable-prefix cache breakpoints. OpenAI caching is automatic.
    pub prompt_cache: bool,
}
impl ModelProfile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.provider.trim().is_empty() && !self.model.trim().is_empty(),
            "model profile requires exact provider and model identifiers"
        );
        if let Some(t) = self.temperature {
            ensure!(
                self.capabilities.temperature && t.is_finite() && (0.0..=2.0).contains(&t),
                "profile temperature unsupported or outside 0..2"
            );
        }
        if let Some(p) = self.top_p {
            ensure!(
                self.capabilities.top_p && p.is_finite() && (0.0..=1.0).contains(&p),
                "profile top_p unsupported or outside 0..1"
            );
        }
        if let Some(n) = self.max_tokens {
            ensure!(
                n > 0 && n <= 1_048_576,
                "profile max_tokens must be 1..1048576"
            );
        }
        ensure!(
            !self.prompt_cache || self.capabilities.prompt_cache,
            "profile prompt caching is not declared supported"
        );
        ensure!(
            self.capabilities.tools != Some(false) || self.tools.as_ref().is_none_or(Vec::is_empty),
            "tool profile conflicts with unsupported tools"
        );
        if let Some(budget) = self.thinking_budget {
            if self.thinking == Some(true) {
                if let Some(limit) = self.max_tokens {
                    ensure!(budget < limit, "thinking_budget must be below max_tokens");
                }
            }
        }
        if let Some(effort) = self.reasoning_effort {
            ensure!(
                self.capabilities.reasoning != ReasoningApi::None
                    && self.capabilities.reasoning_efforts.contains(&effort),
                "reasoning effort is not declared supported for this exact model"
            );
            if self.capabilities.reasoning == ReasoningApi::OpenaiEffort {
                ensure!(
                    !matches!(effort, ReasoningEffort::Max | ReasoningEffort::Ultra),
                    "this OpenAI chat profile does not accept max or ultra"
                );
            } else if self.capabilities.reasoning != ReasoningApi::OpenaiResponses {
                ensure!(
                    !matches!(
                        effort,
                        ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Ultra
                    ),
                    "Anthropic effort must be low, medium, high, xhigh or max"
                );
            }
        }
        match self.capabilities.reasoning {
            ReasoningApi::None => ensure!(
                self.thinking.is_none() && self.thinking_budget.is_none(),
                "thinking is not supported by this profile"
            ),
            ReasoningApi::OpenaiEffort | ReasoningApi::OpenaiResponses => {
                ensure!(
                    self.thinking_budget.is_none(),
                    "OpenAI effort does not accept a thinking budget"
                );
                if self.thinking == Some(false) {
                    ensure!(
                        self.capabilities
                            .reasoning_efforts
                            .contains(&ReasoningEffort::None)
                            && self
                                .reasoning_effort
                                .is_none_or(|e| e == ReasoningEffort::None),
                        "thinking off requires supported none effort and cannot conflict with reasoning_effort"
                    );
                }
                if self.thinking == Some(true) {
                    ensure!(
                        self.reasoning_effort
                            .is_some_and(|e| e != ReasoningEffort::None),
                        "thinking on requires an explicit supported non-none effort"
                    );
                }
            }
            ReasoningApi::AnthropicBudget => {
                ensure!(
                    self.thinking != Some(true) || self.thinking_budget.is_some(),
                    "enabled Anthropic thinking requires thinking_budget"
                );
                if let Some(n) = self.thinking_budget {
                    ensure!(
                        n >= 1024,
                        "Anthropic thinking budget requires at least 1024 tokens"
                    );
                }
                ensure!(
                    self.thinking != Some(true)
                        || self.temperature.is_none() && self.top_p.is_none(),
                    "budgeted thinking cannot be combined with sampling controls"
                );
            }
            ReasoningApi::AnthropicAdaptive => {
                ensure!(
                    self.thinking_budget.is_none(),
                    "adaptive thinking does not accept a budget"
                );
                ensure!(
                    self.thinking != Some(true)
                        || self.temperature.is_none() && self.top_p.is_none(),
                    "adaptive thinking cannot be combined with sampling controls"
                );
            }
        }
        Ok(())
    }
    pub fn validate_request(&self, req: &super::Chat, anthropic: bool) -> Result<()> {
        self.validate()?;
        ensure!(
            anthropic
                == matches!(
                    self.capabilities.reasoning,
                    ReasoningApi::AnthropicBudget | ReasoningApi::AnthropicAdaptive
                )
                || self.capabilities.reasoning == ReasoningApi::None,
            "profile reasoning API does not match provider protocol"
        );
        ensure!(
            !anthropic || self.capabilities.output_limit == OutputLimit::MaxTokens,
            "Anthropic uses max_tokens, not max_completion_tokens"
        );
        ensure!(
            self.capabilities.tools != Some(false) || req.tools.is_empty(),
            "this model profile declares tools unsupported"
        );
        ensure!(
            anthropic || !self.prompt_cache,
            "OpenAI caching is automatic; prompt_cache controls native Anthropic breakpoints only"
        );
        if let Some(budget) = self.thinking_budget {
            if self.thinking == Some(true) {
                ensure!(
                    budget < req.max_tokens,
                    "thinking_budget must be below request max_tokens"
                );
            }
        }
        Ok(())
    }
    pub fn apply_openai(&self, body: &mut serde_json::Value) {
        use serde_json::json;
        crate::reliability::constrain_openai(body, self);
        let object = body.as_object_mut().expect("request body object");
        if !self.capabilities.temperature {
            object.remove("temperature");
        }
        if !self.capabilities.seed {
            object.remove("seed");
        }
        if let Some(t) = self.temperature {
            object.insert("temperature".into(), json!(t));
        }
        if let Some(p) = self.top_p {
            object.insert("top_p".into(), json!(p));
        }
        if self.capabilities.output_limit == OutputLimit::MaxCompletionTokens {
            if let Some(n) = object.remove("max_tokens") {
                object.insert("max_completion_tokens".into(), n);
            }
        }
        if let Some(e) = self
            .reasoning_effort
            .or_else(|| (self.thinking == Some(false)).then_some(ReasoningEffort::None))
        {
            object.insert("reasoning_effort".into(), json!(e.as_str()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_capabilities_do_not_enable_controls() {
        let p = ModelProfile {
            provider: "custom".into(),
            model: "o3-not-a-known-model".into(),
            temperature: Some(0.2),
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }
    #[test]
    fn explicit_reasoning_profile_omits_unsupported_sampling() {
        let p = ModelProfile {
            provider: "openai".into(),
            model: "explicit".into(),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::OpenaiEffort,
                reasoning_efforts: vec![ReasoningEffort::High],
                output_limit: OutputLimit::MaxCompletionTokens,
                ..Default::default()
            },
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        p.validate().unwrap();
        let mut body = serde_json::json!({"temperature":0.2,"max_tokens":8192,"seed":1});
        p.apply_openai(&mut body);
        assert_eq!(
            body,
            serde_json::json!({"max_completion_tokens":8192,"reasoning_effort":"high"})
        );
    }
    #[test]
    fn named_profiles_are_explicit_and_migrate_with_model_selection() {
        use crate::config::types::{Config, EditModeConfig};
        let mut config = Config {
            provider: Some("custom".into()),
            model: "exact".into(),
            ..Default::default()
        };
        let fast = ModelProfile {
            provider: "custom".into(),
            model: "exact".into(),
            max_tokens: Some(16000),
            edit_mode: Some(EditModeConfig::Hashline),
            tools: Some(vec!["read".into()]),
            ..Default::default()
        };
        config.model_profiles.insert("fast".into(), fast.clone());
        config.model_profiles.insert(
            "deep".into(),
            ModelProfile {
                max_tokens: Some(32000),
                ..fast
            },
        );
        assert!(
            config.active_model_profile().is_none(),
            "ambiguous profiles never select by map order"
        );
        config.model_profile = Some("fast".into());
        let harness = config.effective_harness(&config.harness).unwrap();
        assert_eq!(harness.max_tokens, 16000);
        assert_eq!(harness.edit_mode, Some(EditModeConfig::Hashline));
        assert_eq!(harness.tools, vec!["read"]);
        let switched = config.with_selection(&super::super::model_config::ModelSelection {
            provider: "custom".into(),
            model: "different".into(),
            ..Default::default()
        });
        assert!(switched.model_profile.is_none());
    }
    #[test]
    fn thinking_toggle_preserves_budget_but_rejects_unsupported_effort() {
        let mut profile = ModelProfile {
            provider: "anthropic".into(),
            model: "exact".into(),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::AnthropicBudget,
                ..Default::default()
            },
            thinking: Some(false),
            thinking_budget: Some(1024),
            max_tokens: Some(4096),
            ..Default::default()
        };
        profile.validate().unwrap();
        profile.thinking = Some(true);
        profile.validate().unwrap();
        profile.reasoning_effort = Some(ReasoningEffort::High);
        assert!(profile.validate().is_err());
        profile.capabilities.reasoning_efforts = vec![ReasoningEffort::High];
        profile.validate().unwrap();
    }
    #[test]
    fn unsupported_tools_can_be_removed_without_claiming_model_capabilities() {
        let mut config = crate::config::types::Config {
            provider: Some("p".into()),
            model: "m".into(),
            ..Default::default()
        };
        config.model_profiles.insert(
            "plain".into(),
            ModelProfile {
                provider: "p".into(),
                model: "m".into(),
                capabilities: ModelCapabilities {
                    tools: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(
            config
                .effective_harness(&config.harness)
                .unwrap()
                .tools
                .is_empty()
        );
    }
}
