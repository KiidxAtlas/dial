//! Operator-declared, exact-model capabilities. Unknown models are never classified by name.
use super::model_config::ModelSelection;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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
    /// Image-output protocol for this exact model; independent of image input (`vision`).
    pub image_generation: Option<ImageGenerationApi>,
    pub reasoning: ReasoningApi,
    /// Only values documented by this provider for this exact model.
    pub reasoning_efforts: Vec<ReasoningEffort>,
    /// Provider wire value for a declared effort when it differs from the Dial name (e.g.
    /// Inception `minimal` → `instant`). Keys must be in `reasoning_efforts`; applied on the
    /// chat-completions `reasoning_effort` field only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reasoning_effort_wire: BTreeMap<ReasoningEffort, String>,
    pub output_limit: OutputLimit,
    pub prompt_cache: bool,
    /// Endpoint-enforced JSON output for the constrained tool repair retry. Unset uses the
    /// provider default (lm-studio: json_schema, ollama: ollama_format); `none` disables it.
    pub structured_output: Option<StructuredOutput>,
}

/// Provider protocol used to generate images with the selected model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageGenerationApi {
    /// Explicitly disable selected-model image generation.
    Disabled,
    /// OpenAI-compatible images/generations.
    Images,
    /// MLX-Serve native Images API; PNG output only.
    MlxServe,
    /// Responses hosted image_generation tool.
    Responses,
    /// Chat completions returning inline image data URLs (for example OpenRouter).
    ChatCompletions,
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
    /// Effective structured-output mechanism: explicit capability, else the detected endpoint, else
    /// the provider default.
    pub fn structured_output(&self) -> StructuredOutput {
        resolve_structured_output(
            &self.provider,
            &self.model,
            &self.capabilities,
            self.endpoint_structured_output,
        )
    }
}

/// Explicit capability, else the mechanism detected on the live endpoint, else the provider default.
pub fn resolve_structured_output(
    provider: &str,
    model: &str,
    capabilities: &ModelCapabilities,
    endpoint: Option<StructuredOutput>,
) -> StructuredOutput {
    capabilities
        .structured_output
        .or(endpoint)
        .unwrap_or_else(|| structured_output_for(provider, model, capabilities))
}

/// Effective structured-output mechanism for a provider, model and its declared capabilities.
/// A local GGUF model runs on a Dial-launched `llama-server`, which enforces `json_schema`.
pub fn structured_output_for(
    provider: &str,
    model: &str,
    capabilities: &ModelCapabilities,
) -> StructuredOutput {
    capabilities.structured_output.unwrap_or_else(|| {
        if provider == "mlx" && super::local_format::serves_with_llama_cpp(model) {
            StructuredOutput::JsonSchema
        } else {
            super::policy::structured_output(provider)
        }
    })
}

/// Ollama `keep_alive` values: a signed integer of seconds (negative keeps the model loaded) or a
/// Go duration made of number+unit pairs (`ms`, `s`, `m`, `h`).
fn valid_keep_alive(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.len() > 32 {
        return false;
    }
    if value.parse::<i64>().is_ok() {
        return true;
    }
    let mut rest = value.strip_prefix('-').unwrap_or(value);
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if digits == 0 || rest[..digits].parse::<f64>().is_err() {
            return false;
        }
        rest = &rest[digits..];
        let unit = ["ms", "s", "m", "h"]
            .into_iter()
            .find(|unit| rest.starts_with(unit));
        let Some(unit) = unit else { return false };
        rest = &rest[unit.len()..];
    }
    true
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
    /// Ollama only: how long the server keeps the model loaded after a request (`"30m"`, `"-1"` for
    /// always). Unset leaves Ollama's own default, which unloads idle models after a few minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    /// Request-scoped config tiers; applied by body builders, never persisted.
    #[serde(skip)]
    pub request_tiers: RequestTiers,
    /// Set only on the single constrained repair retry; never persisted.
    #[serde(skip)]
    pub tool_repair: bool,
    /// Structured output detected from the live endpoint (a llama.cpp `llama-server` is recognised
    /// by `catalog::is_llama_server`). Set at the request boundary; never persisted. An explicit
    /// `capabilities.structured_output` still wins.
    #[serde(skip)]
    pub endpoint_structured_output: Option<StructuredOutput>,
    /// Request-scoped local hints from the backend (persistent base merged with the next request's
    /// hints); set only at the request boundary and never persisted. Precedence: explicit profile
    /// fields (`thinking`, `reasoning_effort`, `keep_alive`, `local_sampling`) win over these.
    #[serde(skip)]
    pub local_hints: super::local_hints::LocalHints,
    /// The local server behind this request, detected at the request boundary. `None` for hosted
    /// routes, which never receive `local_hints`.
    #[serde(skip)]
    pub local_server: Option<super::local_hints::LocalServer>,
}
impl ModelProfile {
    /// `local_hints` minus whatever an explicit profile setting already decides.
    pub fn effective_local_hints(&self, thinking_decided: bool) -> super::local_hints::LocalHints {
        self.local_hints
            .clone()
            .without_explicit(thinking_decided, self.keep_alive.is_some())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.provider.trim().is_empty() && !self.model.trim().is_empty(),
            "model profile requires exact provider and model identifiers"
        );
        if let Some(local) = &self.local_sampling {
            ensure!(
                matches!(self.provider.as_str(), "mlx" | "lm-studio" | "ollama"),
                "local sampling controls require the mlx, lm-studio or ollama provider"
            );
            match self.provider.as_str() {
                "lm-studio" => local.validate_lmstudio()?,
                "ollama" => local.validate_ollama()?,
                _ => local.validate()?,
            }
        }
        if let Some(keep_alive) = &self.keep_alive {
            ensure!(
                self.provider == "ollama",
                "keep_alive requires the ollama provider"
            );
            ensure!(
                valid_keep_alive(keep_alive),
                "keep_alive must be a duration such as 30m, 2h or 300s, a number of seconds, or -1"
            );
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
    /// Provider wire string for an effort: the declared `reasoning_effort_wire` mapping, else the
    /// Dial name. Every request builder that sends an effort value goes through this.
    pub(crate) fn reasoning_effort_wire(&self, effort: ReasoningEffort) -> &str {
        self.capabilities
            .reasoning_effort_wire
            .get(&effort)
            .map_or(effort.as_str(), String::as_str)
    }
    pub fn apply_openai(&self, body: &mut serde_json::Value) {
        use serde_json::json;
        if let Some(local) = &self.local_sampling {
            match self.provider.as_str() {
                "mlx" if super::local_format::serves_with_llama_cpp(&self.model) => {
                    local.apply_llamacpp(body)
                }
                "mlx" => local.apply(body),
                "lm-studio" => local.apply_lmstudio(body),
                _ => {}
            }
        }
        crate::llm::reliability::constrain_openai(body, self);
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
            let wire = self.reasoning_effort_wire(e);
            object.insert("reasoning_effort".into(), json!(wire));
        }
        if let Some(tier) = self.request_tiers.service_tier_for(&self.provider) {
            object.insert("service_tier".into(), json!(tier));
        }
        // Request-scoped local hints go last: a field the profile or body already wrote stays.
        if let Some(server) = self.local_server {
            let thinking_decided = self.thinking.is_some() || self.reasoning_effort.is_some();
            self.effective_local_hints(thinking_decided)
                .apply_openai(server, body);
        }
    }
}

#[cfg(test)]
mod tests;
