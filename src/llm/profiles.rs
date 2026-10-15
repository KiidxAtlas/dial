//! Operator-declared, exact-model capabilities. Unknown models are never classified by name.
use super::model_config::ModelSelection;
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
/// Thinking a Claude request uses when its profile leaves `thinking` unset: on, in the mode the
/// catalog documents for that exact model. Models the catalog does not know, or that document no
/// thinking support, stay at the provider default. Explicit profiles always win.
pub fn default_anthropic_thinking(model: &str) -> Option<ReasoningApi> {
    use super::catalog::ThinkingMode;
    match super::catalog::model_capabilities(model)?.thinking? {
        ThinkingMode::AnthropicAdaptive => Some(ReasoningApi::AnthropicAdaptive),
        ThinkingMode::AnthropicBudgetEffort | ThinkingMode::Budget => {
            Some(ReasoningApi::AnthropicBudget)
        }
        ThinkingMode::Effort | ThinkingMode::GoogleLevel => None,
    }
}

/// Budget for default budget-mode thinking: half the output allowance, between 1,024 tokens and
/// 16,000 tokens, and always below `max_tokens`. `None` when the output allowance is too small.
pub fn default_thinking_budget(max_tokens: u32) -> Option<u32> {
    let budget = (max_tokens / 2).clamp(1_024, 16_000);
    (budget < max_tokens).then_some(budget)
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
    GeminiBudget,
    GeminiLevel,
    OllamaThink,
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
    /// Endpoint-enforced JSON output for the constrained tool repair retry. Unset uses the
    /// provider default (lm-studio: json_schema, ollama: ollama_format); `none` disables it.
    pub structured_output: Option<StructuredOutput>,
}

/// Structured-output mechanism an endpoint enforces. Never inferred from model names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    #[serde(rename = "none")]
    Disabled,
    /// OpenAI-compatible `response_format` with a json_schema (LM Studio).
    JsonSchema,
    /// Ollama native `format` field carrying a JSON schema.
    OllamaFormat,
}
/// Anthropic cache lifetime. Auto uses 1h for official subscription OAuth and 5m otherwise.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheRetention {
    #[default]
    Auto,
    Short,
    Long,
}
/// Account service tier and context options from config. Set by the Backend at the request
/// boundary and never persisted, so it is skipped by serde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTiers {
    /// `none` sends nothing; other values are gated per provider.
    pub service_tier: String,
    /// Anthropic API-key only: request the 1M-token context beta.
    pub extended_context: bool,
}
impl Default for RequestTiers {
    fn default() -> Self {
        Self {
            service_tier: "none".into(),
            extended_context: false,
        }
    }
}
impl RequestTiers {
    /// The `service_tier` to send to `provider`, or `None`. Applies the rule
    /// for a model target: `ultrafast` is first-party `openai` only, and the advertised-tier
    /// list is unavailable here, so the provider-level answer stands.
    pub fn service_tier_for(&self, provider: &str) -> Option<&str> {
        let tier = self.service_tier.as_str();
        super::policy::service_tier(provider, tier)
    }
    /// Direct Anthropic fast mode: `priority` maps to body `speed:"fast"` plus its beta.
    pub fn anthropic_fast(&self, provider: &str) -> bool {
        super::policy::fast_mode(provider, &self.service_tier)
    }
}

/// Attach request tiers to the model profile for `selection`. With no declared profile, a
/// non-default tier or extended context still needs a request profile, so one is built from the
/// selection; the default tier with no profile needs nothing and returns `None`.
pub fn request_tiers_for(
    profile: Option<ModelProfile>,
    selection: &ModelSelection,
    service_tier: &str,
    extended_context: bool,
) -> Result<Option<ModelProfile>> {
    let non_default = !matches!(service_tier, "" | "none" | "auto") || extended_context;
    let mut profile = match profile {
        Some(profile) => profile,
        None if non_default => ModelProfile::minimal(&selection.provider, &selection.model),
        None => return Ok(None),
    };
    profile.request_tiers = RequestTiers {
        service_tier: service_tier.to_owned(),
        extended_context,
    };
    Ok(Some(profile))
}

impl ModelProfile {
    /// Request-only profile for a selected model without a declared profile. It keeps the sampling
    /// fields the unprofiled request already sends, so the only change on the wire is the tiers.
    pub fn minimal(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            capabilities: ModelCapabilities {
                temperature: true,
                seed: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    /// Effective structured-output mechanism: explicit capability, else the provider default.
    pub fn structured_output(&self) -> StructuredOutput {
        structured_output_for(&self.provider, &self.capabilities)
    }
}

/// Effective structured-output mechanism for a provider and its declared capabilities.
pub fn structured_output_for(provider: &str, capabilities: &ModelCapabilities) -> StructuredOutput {
    capabilities
        .structured_output
        .unwrap_or_else(|| super::policy::structured_output(provider))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking: Option<bool>,
    pub thinking_budget: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u32>,
    pub edit_mode: Option<crate::config::types::EditMode>,
    /// An explicit allow-list of existing harness/integration tools.
    pub tools: Option<Vec<String>>,
    /// Declared native caching: Anthropic cache anchors or Responses session routing key.
    pub prompt_cache: Option<bool>,
    pub cache_retention: CacheRetention,
    /// Local request controls. Never forwarded to cloud providers.
    pub local_sampling: Option<super::local_sampling::LocalSampling>,
    /// Speculative-decoding and KV-cache overrides for this MLX model; replaces the global `local_runtime` when set.
    pub local_runtime: Option<super::local_runtime::LocalRuntime>,
    /// Request-scoped config tiers; applied by body builders, never persisted.
    #[serde(skip)]
    pub request_tiers: RequestTiers,
    /// Set only on the single constrained repair retry; never persisted.
    #[serde(skip)]
    pub tool_repair: bool,
}
impl ModelProfile {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.provider.trim().is_empty() && !self.model.trim().is_empty(),
            "model profile requires exact provider and model identifiers"
        );
        if let Some(local) = &self.local_sampling {
            ensure!(
                matches!(self.provider.as_str(), "mlx" | "lm-studio"),
                "local sampling controls require the mlx or lm-studio provider"
            );
            if self.provider == "lm-studio" {
                local.validate_lmstudio()?;
            } else {
                local.validate()?;
            }
        }
        if let Some(runtime) = &self.local_runtime {
            ensure!(
                self.provider == "mlx",
                "local runtime controls require the mlx provider"
            );
            runtime.validate()?;
        }
        match self.capabilities.structured_output {
            Some(StructuredOutput::JsonSchema) => ensure!(
                self.provider == "lm-studio",
                "structured_output = json_schema requires the lm-studio provider"
            ),
            Some(StructuredOutput::OllamaFormat) => ensure!(
                self.provider == "ollama",
                "structured_output = ollama_format requires the ollama provider"
            ),
            _ => {}
        }
        if let Some(window) = self.context_window {
            ensure!(
                (2048..=super::model_config::MAX_CONTEXT_WINDOW).contains(&window),
                "context window must be 2048..2097152 tokens"
            );
        }
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
                n <= 1_048_576,
                "profile max_tokens must be 0 (unlimited) or 1..1048576"
            );
        }
        ensure!(
            self.prompt_cache != Some(true) || self.capabilities.prompt_cache,
            "profile prompt caching is not declared supported"
        );
        ensure!(
            self.capabilities.tools != Some(false) || self.tools.as_ref().is_none_or(Vec::is_empty),
            "tool profile conflicts with unsupported tools"
        );
        if let Some(budget) = self.thinking_budget
            && self.thinking == Some(true)
            && let Some(limit) = self.max_tokens
        {
            ensure!(budget < limit, "thinking_budget must be below max_tokens");
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
            } else if matches!(
                self.capabilities.reasoning,
                ReasoningApi::AnthropicBudget | ReasoningApi::AnthropicAdaptive
            ) {
                ensure!(
                    !matches!(
                        effort,
                        ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Ultra
                    ),
                    "Anthropic effort must be low, medium, high, xhigh or max"
                );
            } else if self.capabilities.reasoning == ReasoningApi::GeminiLevel {
                ensure!(
                    matches!(
                        effort,
                        ReasoningEffort::None
                            | ReasoningEffort::Minimal
                            | ReasoningEffort::Low
                            | ReasoningEffort::Medium
                            | ReasoningEffort::High
                    ),
                    "Gemini thinking level must be none, minimal, low, medium or high"
                );
            } else if self.capabilities.reasoning == ReasoningApi::OllamaThink {
                ensure!(
                    matches!(
                        effort,
                        ReasoningEffort::None
                            | ReasoningEffort::Low
                            | ReasoningEffort::Medium
                            | ReasoningEffort::High
                            | ReasoningEffort::Max
                    ),
                    "Ollama think level must be none, low, medium, high or max"
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
            ReasoningApi::GeminiBudget => {
                ensure!(
                    self.reasoning_effort.is_none(),
                    "Gemini budgeted thinking does not accept a reasoning effort"
                );
                ensure!(
                    self.thinking != Some(false)
                        || self.thinking_budget.is_none_or(|budget| budget == 0),
                    "disabled Gemini thinking cannot have a nonzero budget"
                );
                ensure!(
                    self.thinking != Some(true) || self.thinking_budget != Some(0),
                    "enabled Gemini thinking cannot have a zero budget"
                );
            }
            ReasoningApi::GeminiLevel | ReasoningApi::OllamaThink => {
                ensure!(
                    self.thinking_budget.is_none(),
                    "this native thinking API does not accept a thinking budget"
                );
                ensure!(
                    self.thinking != Some(false)
                        || self
                            .reasoning_effort
                            .is_none_or(|effort| effort == ReasoningEffort::None),
                    "disabled native thinking conflicts with a non-none effort"
                );
                ensure!(
                    self.thinking != Some(true)
                        || self.reasoning_effort != Some(ReasoningEffort::None),
                    "enabled native thinking conflicts with none effort"
                );
                if self.capabilities.reasoning == ReasoningApi::GeminiLevel {
                    ensure!(
                        self.thinking != Some(false)
                            && self.reasoning_effort != Some(ReasoningEffort::None)
                            || self
                                .capabilities
                                .reasoning_efforts
                                .contains(&ReasoningEffort::None),
                        "disabling Gemini thinking requires exact-model support for none"
                    );
                }
            }
        }
        Ok(())
    }
    pub fn validate_request(&self, req: &super::Chat, anthropic: bool) -> Result<()> {
        self.validate_request_inner(req, anthropic, false)
    }
    pub fn validate_responses_request(&self, req: &super::Chat) -> Result<()> {
        self.validate_request_inner(req, false, true)
    }
    fn validate_request_inner(
        &self,
        req: &super::Chat,
        anthropic: bool,
        responses: bool,
    ) -> Result<()> {
        self.validate()?;
        ensure!(
            if anthropic {
                matches!(
                    self.capabilities.reasoning,
                    ReasoningApi::None
                        | ReasoningApi::AnthropicBudget
                        | ReasoningApi::AnthropicAdaptive
                )
            } else {
                matches!(
                    self.capabilities.reasoning,
                    ReasoningApi::None | ReasoningApi::OpenaiEffort | ReasoningApi::OpenaiResponses
                )
            },
            "profile reasoning API does not match provider protocol"
        );
        ensure!(
            !anthropic || self.capabilities.output_limit == OutputLimit::MaxTokens,
            "Anthropic uses max_tokens, not max_completion_tokens"
        );
        ensure!(
            !anthropic || self.thinking != Some(true) || self.top_p.is_none(),
            "Anthropic thinking cannot be combined with top_p"
        );
        ensure!(
            self.capabilities.tools != Some(false) || req.tools.is_empty(),
            "this model profile declares tools unsupported"
        );
        let responses_cache =
            responses && self.capabilities.prompt_cache && self.prompt_cache != Some(false);
        ensure!(
            anthropic
                || ((self.prompt_cache != Some(true)
                    || responses && self.capabilities.prompt_cache)
                    && (self.cache_retention == CacheRetention::Auto || responses_cache)),
            "prompt_cache needs declared Responses support or Anthropic; cache_retention needs Anthropic or declared Responses caching"
        );
        if let Some(budget) = self.thinking_budget
            && self.thinking == Some(true)
        {
            ensure!(
                budget < req.max_tokens,
                "thinking_budget must be below request max_tokens"
            );
        }
        Ok(())
    }
    pub fn apply_openai(&self, body: &mut serde_json::Value) {
        use serde_json::json;
        if let Some(local) = &self.local_sampling {
            match self.provider.as_str() {
                "mlx" => local.apply(body),
                "lm-studio" => local.apply_lmstudio(body),
                _ => {}
            }
        }
        crate::reliability::constrain_openai(body, self);
        if self.tool_repair
            && self.structured_output() == StructuredOutput::JsonSchema
            && let Some(tools) = body.get("tools").and_then(|tools| tools.as_array())
            && let Ok(schema) = super::tool_repair::schema_for_tools(tools)
        {
            let response_format = super::tool_repair::lmstudio_response_format(schema);
            let fields = body.as_object_mut().expect("request body object");
            fields.remove("tools");
            fields.remove("tool_choice");
            fields.remove("parallel_tool_calls");
            fields.insert("response_format".into(), response_format);
        }
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
        if self.capabilities.output_limit == OutputLimit::MaxCompletionTokens
            && let Some(n) = object.remove("max_tokens")
        {
            object.insert("max_completion_tokens".into(), n);
        }
        if let Some(e) = self
            .reasoning_effort
            .or_else(|| (self.thinking == Some(false)).then_some(ReasoningEffort::None))
        {
            object.insert("reasoning_effort".into(), json!(e.as_str()));
        }
        if let Some(tier) = self.request_tiers.service_tier_for(&self.provider) {
            object.insert("service_tier".into(), json!(tier));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_output_defaults_by_provider_and_explicit_none_disables() {
        let caps = ModelCapabilities::default();
        assert_eq!(
            structured_output_for("lm-studio", &caps),
            StructuredOutput::JsonSchema
        );
        assert_eq!(
            structured_output_for("ollama", &caps),
            StructuredOutput::OllamaFormat
        );
        assert_eq!(
            structured_output_for("mlx", &caps),
            StructuredOutput::Disabled
        );
        let off = ModelCapabilities {
            structured_output: Some(StructuredOutput::Disabled),
            ..Default::default()
        };
        assert_eq!(
            structured_output_for("lm-studio", &off),
            StructuredOutput::Disabled
        );
    }

    #[test]
    fn repair_flag_adds_response_format_only_for_the_retry() {
        let base = serde_json::json!({
            "model": "m",
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {
                "type": "object", "properties": {"path": {"type": "string"}}}}}],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });
        let mut body = base.clone();
        ModelProfile::minimal("lm-studio", "m").apply_openai(&mut body);
        assert!(body.get("response_format").is_none());
        assert!(body.get("tools").is_some());

        let mut repair = ModelProfile::minimal("lm-studio", "m");
        repair.tool_repair = true;
        let mut body = base;
        repair.apply_openai(&mut body);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn legacy_cache_booleans_and_retention_round_trip_without_affecting_other_providers() {
        for value in [true, false] {
            let text = format!(
                "provider = 'anthropic'\nmodel = 'exact'\nprompt_cache = {value}\n[capabilities]\nprompt_cache = true\n"
            );
            let profile: ModelProfile = toml::from_str(&text).unwrap();
            profile.validate().unwrap();
            assert_eq!(profile.prompt_cache, Some(value));
            let restored: ModelProfile =
                toml::from_str(&toml::to_string(&profile).unwrap()).unwrap();
            assert_eq!(restored.prompt_cache, Some(value));
        }
        let profile: ModelProfile =
            toml::from_str("provider = 'anthropic'\nmodel = 'exact'\ncache_retention = 'long'\n")
                .unwrap();
        assert_eq!(profile.prompt_cache, None);
        assert_eq!(profile.cache_retention, CacheRetention::Long);
        let restored: ModelProfile = toml::from_str(&toml::to_string(&profile).unwrap()).unwrap();
        assert_eq!(restored.prompt_cache, None);
        assert_eq!(restored.cache_retention, CacheRetention::Long);
        let req = super::super::Chat {
            messages: vec![],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 8192,
            seed: None,
        };
        profile.validate_request(&req, true).unwrap();
        assert!(profile.validate_request(&req, false).is_err());
        assert!(toml::from_str::<ModelProfile>("cache_retention = 'forever'").is_err());
    }
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
        use crate::config::types::{Config, EditMode};
        let mut config = Config {
            provider: Some("custom".into()),
            model: "exact".into(),
            ..Default::default()
        };
        let fast = ModelProfile {
            provider: "custom".into(),
            model: "exact".into(),
            max_tokens: Some(16000),
            edit_mode: Some(EditMode::Hashline),
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
        assert_eq!(harness.edit_mode, Some(EditMode::Hashline));
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

    #[test]
    fn non_default_tier_without_declared_profile_reaches_the_request_body() {
        use crate::config::types::Config;
        use crate::llm::{Chat, Control, Endpoint, chat_at_controlled, fake};
        use serde_json::json;

        let server = fake::serve(vec![fake::json(
            200,
            &json!({"choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}}]}),
        )]);
        let mut config = Config::default();
        config.provider = Some("openai".into());
        config.model = "gpt-test".into();
        config.service_tier = "priority".into();
        let selection = config.selection();
        let profile =
            request_tiers_for(None, &selection, config.effective_service_tier(), false).unwrap();
        assert!(
            profile.is_some(),
            "a non-default tier needs a request profile"
        );
        let req = Chat {
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: Vec::new(),
            temperature: 0.2,
            max_tokens: 64,
            seed: None,
        };
        chat_at_controlled(
            Endpoint {
                url: &server.url,
                auth: None,
                model: "gpt-test",
                headers: &[],
            },
            &req,
            5,
            Control {
                interrupt: None,
                profile: profile.as_ref(),
                reasoning_guard: None,
                on_event: &mut |_| true,
            },
        )
        .unwrap();
        let requests = server.requests();
        let body = fake::body(&requests[0]);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["temperature"], 0.2);
    }
}
