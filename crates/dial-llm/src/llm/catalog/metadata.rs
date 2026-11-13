//! Model metadata types and resolution: kinds, prices, reasoning declarations and context windows.

use super::bundled;
use super::cache::{PARSED, cached, same_model, selection_cache_key};
use super::local_models::{local_model_metadata, read_json, resolve_local_model};
use super::local_servers::declared_kind;
use super::probe::context;
use super::{ThinkingMode, bundled_model};
use crate::config::paths::Layout;
use crate::llm::local_format::LocalFormat;
use crate::llm::model_config::{ApiProtocol, ModelSelection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// What a model is for, taken from structured provider or checkpoint metadata, never from its
/// display name or source text. `Unknown` means no source said; it stays chat-eligible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    #[default]
    Unknown,
    Chat,
    Embedding,
    Rerank,
    Image,
    Audio,
}
impl ModelKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Chat => "chat",
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
            Self::Image => "image",
            Self::Audio => "audio",
        }
    }
    pub fn supports_chat(self) -> bool {
        matches!(self, Self::Chat | Self::Unknown)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelEntry {
    pub selection: ModelSelection,
    pub label: String,
    pub source: String,
    pub available: bool,
    #[serde(default)]
    pub metadata: ModelMetadata,
}
impl ModelEntry {
    pub fn is_embedding(&self) -> bool {
        self.metadata.kind == ModelKind::Embedding
    }
    pub fn supports_chat(&self) -> bool {
        self.metadata.kind.supports_chat()
    }
    /// Image generation, independent of image input/vision capability.
    pub fn supports_images(&self) -> bool {
        match self.metadata.image_generation {
            Some(crate::llm::profiles::ImageGenerationApi::Disabled) => false,
            Some(_) => true,
            None => self.metadata.kind == ModelKind::Image,
        }
    }
    /// Whether the image role may select this model. Besides declared image output, ChatGPT plan
    /// models qualify through the hosted Responses image tool, which their catalogs can omit; the
    /// approved generation itself verifies account access. An explicit `Disabled` still refuses.
    pub fn accepts_image_role(&self) -> bool {
        self.supports_images()
            || (self.selection.provider == crate::llm::chatgpt::PROVIDER
                && self.metadata.image_generation.is_none()
                && self.supports_chat())
    }
    /// The provider shown before the label. Dial's own local route keeps the id `mlx` (configs
    /// select it as `provider = "mlx"`) but serves GGUF files on llama.cpp, so those read `gguf`.
    pub fn display_provider(&self) -> &str {
        if self.metadata.format == Some(LocalFormat::Gguf) {
            "gguf"
        } else {
            &self.selection.provider
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub vision: Option<bool>,
    /// Declared image-output transport; image-input support does not imply this capability.
    #[serde(default)]
    pub image_generation: Option<crate::llm::profiles::ImageGenerationApi>,
    #[serde(default)]
    pub tools: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// Positive provider-reported maximum output budget; absent when not advertised.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<TokenPricing>,
    /// Preliminary architecture availability, not verified LoRA compatibility. A true value still
    /// needs Dial's capability probe to check adapter targets. `None` means unknown.
    #[serde(default)]
    pub lora: Option<bool>,
    #[serde(default)]
    pub lora_verified: bool,
    /// Automatic local verification progress; no model weights are loaded during discovery itself.
    #[serde(default)]
    pub lora_check: Option<LoraCheck>,
    /// What the model is for. Set from structured provider or checkpoint metadata only.
    #[serde(default)]
    pub kind: ModelKind,
    /// On-disk format of a downloaded local model; `None` for served and hosted models.
    #[serde(default)]
    pub format: Option<LocalFormat>,
    /// Thinking mode and effort levels the live listing declares for this exact model.
    #[serde(default)]
    pub declared_reasoning: Option<DeclaredReasoning>,
}
/// Reasoning controls a live model listing declares for one exact model (Anthropic Models API
/// `capabilities.thinking.types` and `capabilities.effort`). A model released after this build
/// gets its thinking and effort controls from here instead of waiting for a bundled record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredReasoning {
    pub mode: ThinkingMode,
    /// Declared levels as a bit set over [`DECLARABLE_EFFORTS`].
    efforts: u8,
}
/// Effort levels a listing can declare, weakest first.
const DECLARABLE_EFFORTS: [crate::llm::profiles::ReasoningEffort; 5] = {
    use crate::llm::profiles::ReasoningEffort::*;
    [Low, Medium, High, Xhigh, Max]
};
impl DeclaredReasoning {
    /// Declared effort levels, weakest first.
    pub fn efforts(self) -> Vec<crate::llm::profiles::ReasoningEffort> {
        DECLARABLE_EFFORTS
            .iter()
            .enumerate()
            .filter(|(bit, _)| self.efforts & (1 << bit) != 0)
            .map(|(_, effort)| *effort)
            .collect()
    }

    /// Anthropic `/v1/models` row: only capabilities reported as `{"supported": true}` count.
    fn from_anthropic_row(row: &Value) -> Option<Self> {
        let supported = |path: &str| {
            row.pointer(&format!("/capabilities/{path}/supported"))
                .and_then(Value::as_bool)
                == Some(true)
        };
        if !supported("thinking") {
            return None;
        }
        let efforts = if supported("effort") {
            DECLARABLE_EFFORTS
                .iter()
                .enumerate()
                .filter(|(_, effort)| supported(&format!("effort/{}", effort.as_str())))
                .fold(0u8, |bits, (bit, _)| bits | (1 << bit))
        } else {
            0
        };
        let mode = if supported("thinking/types/adaptive") {
            ThinkingMode::AnthropicAdaptive
        } else if supported("thinking/types/enabled") {
            if efforts == 0 {
                ThinkingMode::Budget
            } else {
                ThinkingMode::AnthropicBudgetEffort
            }
        } else {
            return None;
        };
        Some(Self { mode, efforts })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoraCheck {
    Waiting,
    Checking,
    Failed,
}
/// USD per token, as supplied by provider metadata; no guessed price tables.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TokenPricing {
    pub prompt: f64,
    pub completion: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}
impl TokenPricing {
    pub fn cost(self, prompt: u64, completion: u64) -> Option<f64> {
        if ![self.prompt, self.completion]
            .iter()
            .all(|n| n.is_finite() && *n >= 0.0)
        {
            return None;
        }
        let cost = prompt as f64 * self.prompt + completion as f64 * self.completion;
        cost.is_finite().then_some(cost)
    }
    pub fn reply_cost(self, reply: &crate::llm::Reply) -> Option<f64> {
        let prompt = reply.prompt_tokens?;
        let completion = reply.completion_tokens?;
        let read = reply.cached_prompt_tokens.unwrap_or(0);
        let write = reply.cache_creation_tokens.unwrap_or(0);
        let uncached = prompt.checked_sub(read.checked_add(write)?)?;
        let mut cost = self.cost(uncached, completion)?;
        if read > 0 {
            let rate = self.cache_read.filter(|n| n.is_finite() && *n >= 0.0)?;
            cost += read as f64 * rate;
        }
        let split = CacheWriteSplit::of(reply)?;
        let standard_writes = split.five_minute.checked_add(split.unbucketed)?;
        if standard_writes > 0 {
            let rate = self.cache_write.filter(|n| n.is_finite() && *n >= 0.0)?;
            cost += standard_writes as f64 * rate;
        }
        if split.one_hour > 0 {
            // The 1h write rate is derived from the input rate; see
            // `ONE_HOUR_WRITE_INPUT_MULTIPLIER`.
            cost += split.one_hour as f64 * self.prompt * ONE_HOUR_WRITE_INPUT_MULTIPLIER;
        }
        cost.is_finite().then_some(cost)
    }
}
/// Anthropic's published 1-hour cache-write price is 2x the base input rate. The catalog carries
/// only the 5-minute write rate (`cache_write`), so the 1h rate is derived here exactly as
/// the usage cost calculation does (model-independent `input * 2`); no catalog field needed.
pub(crate) const ONE_HOUR_WRITE_INPUT_MULTIPLIER: f64 = 2.0;
/// Prompt-cache writes split by Anthropic TTL bucket for pricing. `unbucketed` is the part of the
/// total that no bucket accounts for; it is priced at the default (5-minute) write rate, which is
/// also what happens when the provider reported no split (`five_minute`/`one_hour` are zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CacheWriteSplit {
    pub five_minute: u64,
    pub one_hour: u64,
    pub unbucketed: u64,
}
impl CacheWriteSplit {
    pub(crate) fn of(reply: &crate::llm::Reply) -> Option<Self> {
        let total = reply.cache_creation_tokens.unwrap_or(0);
        let five_minute = reply.cache_creation_5m_tokens.unwrap_or(0);
        let one_hour = reply.cache_creation_1h_tokens.unwrap_or(0);
        // Contradictory or overflowing buckets cannot be priced as observed usage.
        // Saturation would charge more writes than the total removed from prompt input.
        let bucketed = five_minute.checked_add(one_hour)?;
        Some(Self {
            five_minute,
            one_hour,
            unbucketed: total.checked_sub(bucketed)?,
        })
    }
}
/// Image output declared by provider metadata, never inferred from a model name or vision.
pub fn image_generation_api(row: &Value) -> Option<crate::llm::profiles::ImageGenerationApi> {
    use crate::llm::profiles::ImageGenerationApi;
    // MLX-Serve explicitly advertises native image generation, not vision input.
    if row["owned_by"] == "mlx-serve"
        && row["capabilities"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "image"))
    {
        return Some(ImageGenerationApi::MlxServe);
    }
    let hosted = row
        .get("experimental_supported_tools")
        .or_else(|| row.get("supported_tools"))
        .and_then(Value::as_array);
    if hosted.is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool == "image_generation" || tool["type"] == "image_generation")
    }) {
        return Some(ImageGenerationApi::Responses);
    }
    // A meta-router (OpenRouter `openrouter/auto*`) advertises the union of its routing pool's
    // modalities but serves no image-output endpoint itself, so its list declares nothing.
    if row.pointer("/architecture/tokenizer") == Some(&Value::from("Router")) {
        return None;
    }
    row.pointer("/architecture/output_modalities")
        .or_else(|| row.get("output_modalities"))
        .and_then(Value::as_array)
        .map(|items| {
            if items.iter().any(|item| item == "image") {
                ImageGenerationApi::ChatCompletions
            } else {
                ImageGenerationApi::Disabled
            }
        })
}
/// Image-generation model ids a provider documents for its direct API, used only when the row
/// declares no image output. Exact ids on the built-in endpoint only: no name, family or vision
/// inference, and an explicit row value (including `Disabled`) always wins.
pub(super) fn documented_image_generation(
    provider: &str,
    base_url: &str,
    model: &str,
) -> Option<crate::llm::profiles::ImageGenerationApi> {
    use crate::llm::profiles::ImageGenerationApi;
    let builtin = bundled::builtin_provider(provider)?;
    if builtin.base_url.trim_end_matches('/') != base_url.trim_end_matches('/') {
        return None;
    }
    match (provider, model) {
        // OpenAI Images API (`/v1/images/generations`) for the GPT Image models.
        (
            "openai",
            "gpt-image-1"
            | "gpt-image-1-mini"
            | "gpt-image-1.5"
            | "gpt-image-2"
            | "gpt-image-2.5-flare"
            | "gpt-image-2.5-sunburst"
            | "gpt-image-2.6-flare"
            | "gpt-image-2.6-sunburst",
        ) => Some(ImageGenerationApi::Images),
        // xAI Grok Imagine on `/v1/images/generations` (OpenAI-compatible request shape).
        ("xai", "grok-imagine-image-2.0") => Some(ImageGenerationApi::Images),
        // Gemini native `generateContent` with image output (the "Nano Banana" family). These are
        // the ids whose OpenRouter rows (`google/<id>`) declare image output, plus Google's
        // `nano-banana-pro-preview` alias of `gemini-3-pro-image-preview`.
        (
            "gemini",
            "gemini-2.5-flash-image"
            | "gemini-3-pro-image"
            | "gemini-3-pro-image-preview"
            | "nano-banana-pro-preview"
            | "gemini-3.1-flash-image"
            | "gemini-3.1-flash-image-preview"
            | "gemini-3.1-flash-lite-image"
            | "gemini-nano-banana-2.1",
        ) => Some(ImageGenerationApi::Gemini),
        _ => None,
    }
}
pub(super) fn metadata(row: &Value) -> ModelMetadata {
    let modalities = row
        .pointer("/architecture/input_modalities")
        .or_else(|| row.get("input_modalities"))
        .and_then(Value::as_array);
    let vision = modalities
        .map(|items| items.iter().any(|item| item == "image"))
        .or_else(|| row.pointer("/capabilities/vision").and_then(Value::as_bool))
        .or_else(|| row.get("vision").and_then(Value::as_bool))
        .or_else(|| {
            row.get("capabilities")
                .and_then(Value::as_array)
                .map(|items| items.iter().any(|item| item == "vision"))
        })
        .or_else(|| {
            row.get("vision_config")
                .filter(|config| config.is_object())
                .map(|_| true)
        })
        // Anthropic `/v1/models`: `capabilities` is an object of `{ "supported": bool }` records.
        .or_else(|| {
            row.pointer("/capabilities/image_input/supported")
                .and_then(Value::as_bool)
        });
    let capability = |name: &str, parameters: &[&str]| {
        row.pointer(&format!("/capabilities/{name}"))
            .and_then(Value::as_bool)
            .or_else(|| row.get(name).and_then(Value::as_bool))
            .or_else(|| {
                row.get("capabilities")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .any(|item| item == name || (name == "reasoning" && item == "thinking"))
                    })
            })
            .or_else(|| {
                row.get("supported_parameters")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .any(|item| parameters.iter().any(|parameter| item == parameter))
                    })
            })
    };
    let tools = capability("tools", &["tools", "tool_choice"]).or_else(|| {
        row["supports_parallel_tool_calls"]
            .as_bool()
            .filter(|supported| *supported)
    });
    let reasoning = capability("reasoning", &["reasoning", "reasoning_effort"])
        // Anthropic `capabilities.thinking.supported`; Gemini's top-level `thinking` boolean.
        .or_else(|| {
            row.pointer("/capabilities/thinking/supported")
                .and_then(Value::as_bool)
        })
        .or_else(|| row.get("thinking").and_then(Value::as_bool))
        .or_else(|| {
            row.get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|items| !items.is_empty())
        });
    let price = |key| {
        row.pointer(key)
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let pricing = price("/pricing/prompt")
        .zip(price("/pricing/completion"))
        .map(|(prompt, completion)| TokenPricing {
            prompt,
            completion,
            cache_read: price("/pricing/input_cache_read"),
            cache_write: price("/pricing/input_cache_write"),
        });
    ModelMetadata {
        vision,
        image_generation: image_generation_api(row),
        tools,
        reasoning,
        max_output_tokens: [
            "/max_output_tokens",
            "/outputTokenLimit",
            "/max_tokens",
            "/top_provider/max_completion_tokens",
        ]
        .iter()
        .filter_map(|path| row.pointer(path))
        .find_map(|value| {
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
        }),
        pricing,
        kind: declared_kind(row),
        declared_reasoning: DeclaredReasoning::from_anthropic_row(row),
        ..Default::default()
    }
}
/// Cache-only lookup: never make a metadata request in the inference hot path.
pub fn model_metadata(layout: &Layout, selection: &ModelSelection) -> ModelMetadata {
    if selection.provider == crate::llm::chatgpt::PROVIDER {
        let profile = crate::llm::chatgpt::scoped_model_profile(
            &layout.user_dir,
            selection.account_id.as_deref(),
            &selection.model,
        )
        .ok()
        .flatten();
        return ModelMetadata {
            vision: profile.as_ref().and_then(|p| p.capabilities.vision),
            image_generation: profile.and_then(|p| p.capabilities.image_generation),
            ..Default::default()
        };
    }
    if selection.provider == crate::llm::apple::PROVIDER {
        return crate::llm::apple::metadata();
    }
    if selection.provider == "mlx" && selection.base_url.is_none() {
        let model =
            resolve_local_model(&selection.model).unwrap_or_else(|_| selection.model.clone());
        return local_model_metadata(Path::new(&model));
    }
    let metadata = cached(&layout.user_dir)
        .get(&selection_cache_key(selection))
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| same_model(&entry.selection, selection))
        })
        .map(|entry| entry.metadata)
        .or_else(|| {
            bundled_model(&selection.provider, &selection.model).map(|model| model.metadata())
        })
        .unwrap_or_default();
    bundled::complete_metadata(selection, metadata)
}
/// The published list price of `selection`'s exact bundled card, matched by endpoint origin so a
/// custom proxy never inherits a first-party card. An estimate, not a provider-reported price:
/// callers label it as such. Local endpoints and models without a card have none.
pub fn list_pricing(selection: &ModelSelection) -> Option<TokenPricing> {
    bundled::selection_model(selection)?.pricing
}
/// Reasoning controls the saved live listing declares for `selection`'s exact model at its
/// endpoint, for models the bundled catalog does not describe yet. No network access.
pub fn declared_reasoning(
    user_dir: &Path,
    selection: &ModelSelection,
) -> Option<DeclaredReasoning> {
    find_declared_reasoning(
        &cached(user_dir),
        &selection.provider,
        &selection.model,
        |entry| {
            let trim = |url: Option<&str>| url.map(|url| url.trim_end_matches('/').to_owned());
            selection.base_url.is_none()
                || trim(entry.base_url.as_deref()) == trim(selection.base_url.as_deref())
        },
    )
}
/// Like [`declared_reasoning`], from the listing this process last read. Request builders that
/// know only the model id use it; backend construction reads the listing first.
pub fn remembered_declared_reasoning(provider: &str, model: &str) -> Option<DeclaredReasoning> {
    let parsed = PARSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, parsed)| parsed.clone())?;
    find_declared_reasoning(&parsed, provider, model, |_| true)
}
fn find_declared_reasoning(
    cache: &BTreeMap<String, Vec<ModelEntry>>,
    provider: &str,
    model: &str,
    endpoint: impl Fn(&ModelSelection) -> bool,
) -> Option<DeclaredReasoning> {
    cache
        .values()
        .flatten()
        .filter(|entry| {
            entry.selection.provider == provider
                && entry.selection.model == model
                && endpoint(&entry.selection)
        })
        .find_map(|entry| entry.metadata.declared_reasoning)
}
pub(super) fn documented_context_window(selection: &ModelSelection) -> Option<u32> {
    if selection.provider == "anthropic"
        && selection
            .base_url
            .as_deref()
            .is_none_or(|url| url.trim_end_matches('/') == crate::llm::anthropic::BASE_URL)
    {
        crate::llm::anthropic::documented_context_window(&selection.model)
    } else if selection.provider != crate::llm::chatgpt::PROVIDER {
        bundled::selection_model(selection).and_then(|model| model.context_window)
    } else {
        None
    }
}
/// Metadata-only resolution for configuration load and model changes. No provider requests.
pub(crate) fn model_context_window(user_dir: &Path, selection: &ModelSelection) -> Option<u32> {
    selection
        .context_window
        .or_else(|| {
            cached(user_dir)
                .get(&selection_cache_key(selection))?
                .iter()
                .find(|entry| {
                    entry.selection.provider == selection.provider
                        && entry.selection.model == selection.model
                        && entry
                            .selection
                            .protocol
                            .unwrap_or_else(|| ApiProtocol::builtin(&entry.selection.provider))
                            == selection
                                .protocol
                                .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider))
                        && entry.selection.account_id == selection.account_id
                        && entry.selection.headers.as_deref().unwrap_or_default()
                            == selection.headers.as_deref().unwrap_or_default()
                })
                .and_then(|entry| entry.selection.context_window)
        })
        .or_else(|| {
            if selection.provider == crate::llm::chatgpt::PROVIDER {
                crate::llm::chatgpt::scoped_model_context_window(
                    user_dir,
                    selection.account_id.as_deref(),
                    &selection.model,
                )
            } else if selection.provider == crate::llm::apple::PROVIDER {
                crate::llm::apple::supported_build()
                    .then(|| crate::llm::apple::availability().context_size)
            } else if selection.provider == "mlx" && selection.base_url.is_none() {
                let model = resolve_local_model(&selection.model).ok()?;
                context(&read_json(&Path::new(&model).join("config.json"))?)
            } else {
                None
            }
        })
        .or_else(|| documented_context_window(selection))
}
