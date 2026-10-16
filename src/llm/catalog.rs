//! Metadata-only discovery: no subnet scan, inference, or model downloads.
//! Saved credentials are plaintext JSON, protected by owner-only directories/files,
//! not an OS keychain. They are URL-scoped and written only on explicit user action.
use super::model_config::{ApiProtocol, ModelSelection, ProviderSpec};
use crate::storage::{
    paths::Layout,
    settings_store::{locked, read_private, write_private},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
#[path = "catalog/bundled.rs"]
mod bundled;
pub use bundled::{
    ThinkingMode, builtin_provider, bundled_model, bundled_models, known_context_window,
    model_capabilities, provider_endpoint,
};

const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
const MAX_MODELS: usize = 512;
const MAX_PROVIDERS: usize = 64;
const CONCURRENCY: usize = 8;
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_VALIDATE_URL: &str = "https://openrouter.ai/api/v1/auth/key";
const KNOWN: &[(&str, &str, &str, &str)] = &[
    (
        "gemini",
        "Google Gemini",
        super::gemini::BASE_URL,
        "GEMINI_API_KEY",
    ),
    (
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        "OPENAI_API_KEY",
    ),
    (
        "anthropic",
        "Anthropic",
        "https://api.anthropic.com/v1",
        "ANTHROPIC_API_KEY",
    ),
    (
        "openrouter",
        "OpenRouter",
        OPENROUTER_BASE_URL,
        "OPENROUTER_API_KEY",
    ),
    (
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        "GROQ_API_KEY",
    ),
    (
        "mistral",
        "Mistral",
        "https://api.mistral.ai/v1",
        "MISTRAL_API_KEY",
    ),
    (
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_API_KEY",
    ),
];
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
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub vision: Option<bool>,
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
    pub fn reply_cost(self, reply: &super::Reply) -> Option<f64> {
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
        let split = CacheWriteSplit::of(reply);
        let standard_writes = split.five_minute.saturating_add(split.unbucketed);
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
    pub(crate) fn of(reply: &super::Reply) -> Self {
        let total = reply.cache_creation_tokens.unwrap_or(0);
        let five_minute = reply.cache_creation_5m_tokens.unwrap_or(0);
        let one_hour = reply.cache_creation_1h_tokens.unwrap_or(0);
        Self {
            five_minute,
            one_hour,
            unbucketed: total.saturating_sub(five_minute.saturating_add(one_hour)),
        }
    }
}

fn metadata(row: &Value) -> ModelMetadata {
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
    let reasoning = capability("reasoning", &["reasoning", "reasoning_effort"]).or_else(|| {
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
        ..Default::default()
    }
}

/// Preliminary MLX architecture availability; this does not verify adapter training targets.
pub fn local_lora_support(user_dir: &Path, model: &Path) -> Option<bool> {
    lora_support(mlx_architectures(user_dir).as_deref(), model)
}

/// Read local capabilities without importing Python or loading model weights. GGUF models are read
/// from their header (chat template) and a sibling `mmproj` projector, which `llama-server` loads.
pub fn local_model_metadata(model: &Path) -> ModelMetadata {
    if !model.join("config.json").is_file()
        && let Some(info) = crate::llm::local_format::gguf_info(model)
    {
        let mut metadata = ModelMetadata {
            vision: Some(crate::llm::local_format::gguf_projector(model).is_some()),
            ..Default::default()
        };
        if let Some(template) = &info.chat_template {
            classify_template(&mut metadata, template);
        }
        return metadata;
    }
    read_json(&model.join("config.json"))
        .map(|value| {
            let mut info = metadata(&value);
            if is_embedding_checkpoint(model) {
                info.kind = ModelKind::Embedding;
            }
            // A downloaded text-only configuration has no vision encoder.
            info.vision = Some(info.vision.unwrap_or(false));
            // These are template declarations, not inference or quality checks. Missing
            // template markers remain unknown rather than being classified by model name.
            let tokenizer = read_json(&model.join("tokenizer_config.json"));
            let template = tokenizer
                .as_ref()
                .and_then(|value| {
                    let templates = match &value["chat_template"] {
                        Value::String(template) => vec![template.as_str()],
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|item| item["template"].as_str())
                            .collect(),
                        Value::Object(items) => items.values().filter_map(Value::as_str).collect(),
                        _ => Vec::new(),
                    };
                    (!templates.is_empty()).then(|| templates.join("\n"))
                })
                .or_else(|| {
                    let file = fs::File::open(model.join("chat_template.jinja")).ok()?;
                    let mut template = String::new();
                    file.take(MAX_RESPONSE + 1)
                        .read_to_string(&mut template)
                        .ok()?;
                    (template.len() as u64 <= MAX_RESPONSE).then_some(template)
                });
            if let Some(template) = template {
                classify_template(&mut info, &template);
            }
            info
        })
        .unwrap_or_default()
}

/// Tool and reasoning support declared by a chat template. These are template declarations, not
/// inference or quality checks; values already known are kept.
fn classify_template(info: &mut ModelMetadata, template: &str) {
    info.tools = info.tools.or_else(|| {
        (template.contains("tools")
            && ["tool_call", "function_call", "[TOOL_CALLS]"]
                .iter()
                .any(|marker| template.contains(marker)))
        .then_some(true)
    });
    info.reasoning = info.reasoning.or_else(|| {
        (template.contains("enable_thinking")
            || (template.contains("<think>") && template.contains("</think>")))
        .then_some(true)
    });
}

/// Cache-only lookup: never make a metadata request in the inference hot path.
pub fn model_metadata(layout: &Layout, selection: &ModelSelection) -> ModelMetadata {
    if selection.provider == super::chatgpt::PROVIDER {
        return ModelMetadata {
            vision: super::chatgpt::scoped_model_profile(
                &layout.user_dir,
                selection.account_id.as_deref(),
                &selection.model,
            )
            .ok()
            .flatten()
            .and_then(|profile| profile.capabilities.vision),
            ..Default::default()
        };
    }
    if selection.provider == "mlx" && selection.base_url.is_none() {
        let model =
            resolve_local_model(&selection.model).unwrap_or_else(|_| selection.model.clone());
        return local_model_metadata(Path::new(&model));
    }
    let metadata = cached(&layout.user_dir)
        .remove(&selection_cache_key(selection))
        .and_then(|entries| {
            entries
                .into_iter()
                .find(|entry| same_model(&entry.selection, selection))
        })
        .map(|entry| entry.metadata)
        .or_else(|| {
            bundled_model(&selection.provider, &selection.model).map(|model| model.metadata())
        })
        .unwrap_or_default();
    bundled::complete_metadata(selection, metadata)
}
#[derive(Clone, Debug)]
pub struct ProviderStatus {
    pub id: String,
    pub label: String,
    pub base_url: Option<String>,
    pub state: String,
    pub detail: String,
    pub models: usize,
}
#[derive(Clone, Default)]
pub struct Discovery {
    pub models: Vec<ModelEntry>,
    pub providers: Vec<ProviderStatus>,
}

fn validated_url(base: &str, allow_insecure: bool) -> Result<ureq::http::Uri> {
    ensure!(
        !base.contains('#'),
        "endpoint URL must not contain a fragment"
    );
    // Endpoint validation deliberately does not echo the input, which could be a secret.
    crate::llm::provider::endpoint_url(base, allow_insecure).map_err(|_| {
        anyhow::anyhow!("invalid or insecure endpoint URL; credentials and queries are forbidden")
    })?;
    base.parse()
        .map_err(|_| anyhow::anyhow!("invalid endpoint URL"))
}
fn valid_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
        "credential must be nonempty printable ASCII without whitespace"
    );
    Ok(())
}
fn credential_map(user_dir: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = read_private(&user_dir.join("credentials.json"))?;
    match bytes {
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("credential file is invalid; no changes saved")),
        None => Ok(BTreeMap::new()),
    }
}
pub fn credential_token(user_dir: &Path, base_url: &str) -> Result<Option<String>> {
    validated_url(base_url, true)?;
    let path = user_dir.join("credentials.json");
    let _lock = locked(&path)?;
    let token = credential_map(user_dir)?.remove(base_url);
    if let Some(token) = &token {
        valid_token(token)?;
    }
    Ok(token)
}

/// The environment variable Dial reads for a built-in provider's base URL, if the URL is one of its known endpoints.
pub fn known_key_env(base_url: &str) -> Option<&'static str> {
    let uri = base_url.parse::<ureq::http::Uri>().ok()?;
    KNOWN
        .iter()
        .find(|(_, _, url, _)| {
            url.parse::<ureq::http::Uri>()
                .is_ok_and(|known| origin(&known) == origin(&uri))
        })
        .map(|(_, _, _, key)| *key)
}
pub fn save_credential(user_dir: &Path, base_url: &str, token: &str) -> Result<()> {
    validated_url(base_url, true)?;
    valid_token(token)?;
    let path = user_dir.join("credentials.json");
    let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.insert(base_url.to_owned(), token.to_owned());
    write_private(&path, &serde_json::to_vec(&credentials)?)
}
pub fn remove_credential(user_dir: &Path, base_url: &str) -> Result<()> {
    validated_url(base_url, true)?;
    let path = user_dir.join("credentials.json");
    let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.remove(base_url);
    write_private(&path, &serde_json::to_vec(&credentials)?)
}

// Never derive Debug: this transient object can contain a bearer credential.
struct Probe {
    id: String,
    spec: ProviderSpec,
    token: Option<String>,
    blocked: Option<(&'static str, &'static str)>,
}
fn origin(uri: &ureq::http::Uri) -> (String, String, u16) {
    (
        uri.scheme_str().unwrap_or("").to_ascii_lowercase(),
        uri.host().unwrap_or("").to_ascii_lowercase(),
        uri.port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            }),
    )
}
/// `--api-key` for one endpoint origin. Process memory only; never written to disk or logs.
static CLI_API_KEY: std::sync::LazyLock<
    parking_lot::RwLock<Option<((String, String, u16), String)>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(None));

/// Sets the process-only API key for `base_url`'s origin. Applies ahead of env and saved keys.
pub fn set_cli_api_key(base_url: &str, key: String) -> Result<()> {
    let uri = validated_url(base_url, true)?;
    valid_token(&key)?;
    *CLI_API_KEY.write() = Some((origin(&uri), key));
    Ok(())
}

/// The `--api-key` for this exact origin, if one was set.
fn cli_token(uri: &ureq::http::Uri) -> Option<String> {
    CLI_API_KEY
        .read()
        .as_ref()
        .filter(|(scope, _)| *scope == origin(uri))
        .map(|(_, key)| key.clone())
}
fn env_token(name: &str, uri: &ureq::http::Uri) -> Result<Option<String>> {
    if let Some((_, _, url, _)) = KNOWN.iter().find(|(_, _, _, key)| *key == name) {
        let known: ureq::http::Uri = url.parse()?;
        ensure!(
            origin(uri) == origin(&known),
            "provider credential environment variable does not match endpoint origin"
        );
    }
    let token = std::env::var(name).ok().filter(|s| !s.is_empty());
    if let Some(token) = &token {
        valid_token(token)?;
    }
    Ok(token)
}

/// Account and gateway settings that fill templated built-in base URLs (`<account>`, `<gateway>`).
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointSettings<'a> {
    pub snowflake_account: Option<&'a str>,
    pub cloudflare_account_id: Option<&'a str>,
    pub cloudflare_gateway_id: Option<&'a str>,
}

/// Substitutes the account/gateway placeholders of a built-in base URL. A missing setting leaves its
/// placeholder in place; `provider::endpoint_url` then refuses the URL and names the setting.
pub fn fill_endpoint_template(
    provider_id: &str,
    base: &str,
    settings: EndpointSettings<'_>,
) -> String {
    let (account, gateway) = match provider_id {
        "snowflake" => (settings.snowflake_account, None),
        "cloudflare-ai-gateway" => (
            settings.cloudflare_account_id,
            settings.cloudflare_gateway_id,
        ),
        _ => return base.to_owned(),
    };
    let mut filled = base.to_owned();
    if let Some(account) = account.filter(|value| !value.is_empty()) {
        filled = filled.replace("<account>", account);
    }
    if let Some(gateway) = gateway.filter(|value| !value.is_empty()) {
        filled = filled.replace("<gateway>", gateway);
    }
    filled
}

/// Shared inference/discovery credential precedence, with provider-origin checks for known env keys.
pub fn resolve_credential(
    user_dir: &Path,
    base_url: &str,
    variable: Option<&str>,
) -> Result<Option<String>> {
    let uri = validated_url(base_url, true)?;
    if let Some(token) = cli_token(&uri) {
        return Ok(Some(token));
    }
    let variable = variable.or_else(|| {
        KNOWN
            .iter()
            .find(|(_, _, url, _)| {
                url.parse::<ureq::http::Uri>()
                    .is_ok_and(|known| origin(&known) == origin(&uri))
            })
            .map(|(_, _, _, key)| *key)
    });
    if let Some(variable) = variable
        && let Some(token) = env_token(variable, &uri)?
    {
        return Ok(Some(token));
    }
    let token = if user_dir.as_os_str().is_empty() {
        None
    } else {
        credential_token(user_dir, base_url)?
    };
    ensure!(
        variable.is_none() || token.is_some(),
        "provider credentials are missing; configure an API key in /providers or set its credential environment variable"
    );
    Ok(token)
}
fn prepare(id: String, spec: ProviderSpec, user_dir: &Path) -> Probe {
    let mut probe = Probe {
        id,
        spec,
        token: None,
        blocked: None,
    };
    if !probe.spec.enabled {
        probe.blocked = Some(("disabled", "Disabled in configuration; not probed"));
        return probe;
    }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) {
        Ok(uri) => uri,
        Err(_) => {
            probe.blocked = Some(("unavailable", "Invalid or insecure endpoint URL"));
            return probe;
        }
    };
    let env = probe.spec.api_key_env.as_deref().or_else(|| {
        KNOWN
            .iter()
            .find(|(_, _, base, _)| {
                base.parse::<ureq::http::Uri>()
                    .is_ok_and(|known| origin(&known) == origin(&uri))
            })
            .map(|(_, _, _, key)| *key)
    });
    let env_result = match cli_token(&uri) {
        Some(token) => Ok(Some(token)),
        None => env
            .map(|name| env_token(name, &uri))
            .transpose()
            .map(Option::flatten),
    };
    let token = match env_result {
        Ok(token) => token,
        Err(_) => {
            probe.blocked = Some((
                "auth-required",
                "Credential environment variable is invalid or belongs to a different provider origin",
            ));
            return probe;
        }
    };
    probe.token = match token {
        Some(token) => Some(token),
        None => match credential_token(user_dir, &probe.spec.base_url) {
            Ok(token) => token,
            Err(_) => {
                probe.blocked = Some(("auth-required", "Saved credentials cannot be read safely"));
                return probe;
            }
        },
    };
    if env.is_some() && probe.token.is_none() {
        probe.blocked = Some((
            "auth-required",
            "Set the configured credential environment variable or save a URL-scoped token",
        ));
    }
    probe
}
fn apply_anthropic_oauth_token(probe: &mut Probe, token: &super::auth::SecretToken) {
    probe.token = Some(token.as_str().to_owned());
    probe.blocked = None;
}
fn status(probe: &Probe, state: &str, detail: &str, models: usize) -> ProviderStatus {
    ProviderStatus {
        id: probe.id.clone(),
        label: if probe.spec.label.is_empty() {
            probe.id.clone()
        } else {
            probe.spec.label.clone()
        },
        base_url: validated_url(&probe.spec.base_url, true)
            .ok()
            .map(|_| probe.spec.base_url.clone()),
        state: state.into(),
        detail: detail.into(),
        models,
    }
}
fn context(value: &Value) -> Option<u32> {
    [
        "/inputTokenLimit",
        "/context_window",
        "/max_input_tokens",
        "/context_length",
        "/max_context_length",
        "/max_position_embeddings",
        "/text_config/max_position_embeddings",
        "/top_provider/context_length",
        "/meta/n_ctx_train",
        "/details/context_length",
    ]
    .iter()
    .filter_map(|key| value.pointer(key))
    .find_map(|v| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    })
}
fn parse_models(value: &Value, probe: &Probe) -> Result<Vec<ModelEntry>> {
    let rows = value
        .get(
            if matches!(
                probe
                    .spec
                    .protocol
                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id)),
                ApiProtocol::Ollama | ApiProtocol::Gemini
            ) {
                "models"
            } else {
                "data"
            },
        )
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("model response has no model array"))?;
    let mut seen = BTreeSet::new();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let model = row
                .get("id")
                .or_else(|| row.get("model"))
                .or_else(|| row.get("name"))?
                .as_str()?
                .trim();
            let gemini = probe
                .spec
                .protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                == ApiProtocol::Gemini;
            let methods = row["supportedGenerationMethods"].as_array();
            let declares = |method: &str| methods.is_some_and(|m| m.iter().any(|x| x == method));
            // Gemini states purpose only through its generation methods; rows with neither are dropped.
            let declared = if !gemini {
                ModelKind::Unknown
            } else if declares("generateContent") {
                ModelKind::Chat
            } else if declares("embedContent") {
                ModelKind::Embedding
            } else {
                return None;
            };
            let model = if gemini {
                model.strip_prefix("models/").unwrap_or(model)
            } else {
                model
            };
            if model.is_empty()
                || model.len() > 4096
                || model.chars().any(char::is_control)
                || !seen.insert(model.to_owned())
            {
                return None;
            }
            let mut selection = ModelSelection {
                provider: probe.id.clone(),
                model: model.into(),
                base_url: Some(probe.spec.base_url.clone()),
                api_key_env: probe.spec.api_key_env.clone(),
                allow_insecure: probe.spec.allow_insecure,
                protocol: Some(
                    probe
                        .spec
                        .protocol
                        .unwrap_or_else(|| ApiProtocol::builtin(&probe.id)),
                ),
                headers: probe.spec.headers.clone(),
                account_id: None,
                context_window: context(row),
            };
            selection.context_window = selection
                .context_window
                .or_else(|| documented_context_window(&selection));
            let reported = if declared == ModelKind::Unknown {
                declared_kind(row)
            } else {
                declared
            };
            let kind = bundled::known_kind(&selection, reported);
            Some(ModelEntry {
                selection,
                label: model.into(),
                source: probe.spec.label.clone(),
                available: true,
                metadata: ModelMetadata {
                    kind,
                    ..metadata(row)
                },
            })
        })
        .take(MAX_MODELS)
        .collect())
}
/// Detail of the status for an endpoint that refused the connection or did not answer in time. The
/// settings panel shows it as "not running" for a local server, rather than as a failure.
pub const UNREACHABLE_DETAIL: &str = "Connection failed or metadata probe timed out";
fn probe_models(probe: &Probe) -> (ProviderStatus, Vec<ModelEntry>) {
    if super::model_config::validate_headers(probe.spec.headers.as_deref().unwrap_or_default())
        .is_err()
    {
        return (
            status(probe, "unavailable", "Invalid request headers", 0),
            Vec::new(),
        );
    }
    if probe
        .token
        .as_deref()
        .is_some_and(super::anthropic::subscription_token)
        && probe.spec.headers.as_ref().is_some_and(|h| !h.is_empty())
    {
        return (
            status(
                probe,
                "unavailable",
                "Subscription does not permit request headers",
                0,
            ),
            Vec::new(),
        );
    }
    if let Some((state, detail)) = probe.blocked {
        return (status(probe, state, detail, 0), Vec::new());
    }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) {
        Ok(uri) => uri,
        Err(_) => {
            return (
                status(probe, "unavailable", "Invalid endpoint URL", 0),
                Vec::new(),
            );
        }
    };
    let local = crate::llm::is_local_host(uri.host().unwrap_or(""));
    let builder = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(if local { 1 } else { 5 })))
        .http_status_as_error(false)
        .max_redirects(0);
    let builder = if local { builder.proxy(None) } else { builder };
    let agent = ureq::Agent::new_with_config(builder.build());
    let protocol = probe
        .spec
        .protocol
        .unwrap_or_else(|| ApiProtocol::builtin(&probe.id));
    let url = if protocol == ApiProtocol::Gemini {
        format!("{}/models", probe.spec.base_url.trim_end_matches('/'))
    } else if protocol == ApiProtocol::Ollama {
        let base = probe
            .spec
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches("/api");
        format!("{base}/api/tags")
    } else {
        let chat = match crate::llm::provider::endpoint_url(
            &probe.spec.base_url,
            probe.spec.allow_insecure,
        ) {
            Ok(url) => url,
            Err(_) => {
                return (
                    status(probe, "unavailable", "Invalid endpoint URL", 0),
                    Vec::new(),
                );
            }
        };
        format!(
            "{}models",
            chat.strip_suffix("chat/completions").unwrap_or(&chat)
        )
    };
    let mut request = agent.get(&url).header("Accept", "application/json");
    let resolved_headers = match super::model_config::resolve_validated_headers(
        probe.spec.headers.as_deref().unwrap_or_default(),
    ) {
        Ok(headers) => headers,
        Err(_) => {
            return (
                status(
                    probe,
                    "unavailable",
                    "Request header environment is unavailable or invalid",
                    0,
                ),
                Vec::new(),
            );
        }
    };
    for (name, value) in resolved_headers.pairs() {
        let field = match super::model_config::private_header_value(value) {
            Ok(field) => field,
            Err(_) => {
                return (
                    status(probe, "unavailable", "Invalid request header value", 0),
                    Vec::new(),
                );
            }
        };
        request = request.header(name, field);
    }
    if let Some(token) = &probe.token {
        if protocol == ApiProtocol::Gemini {
            request = request.header("x-goog-api-key", token);
        } else if protocol == ApiProtocol::Anthropic {
            let headers = match super::anthropic::auth_headers(&url, token) {
                Ok(headers) => headers,
                Err(_) => {
                    return (
                        status(
                            probe,
                            "auth-required",
                            "Subscription credentials require the official Anthropic endpoint",
                            0,
                        ),
                        Vec::new(),
                    );
                }
            };
            for (name, value) in headers {
                request = request.header(name, value);
            }
        } else {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(_) => {
            return (
                status(
                    probe,
                    "unavailable",
                    UNREACHABLE_DETAIL,
                    0,
                ),
                Vec::new(),
            );
        }
    };
    let code = response.status().as_u16();
    if code == 401 || code == 403 {
        return (
            status(
                probe,
                "auth-required",
                "Endpoint requires valid credentials",
                0,
            ),
            Vec::new(),
        );
    }
    if !(200..300).contains(&code) {
        return (
            status(
                probe,
                "unavailable",
                &format!("Endpoint returned HTTP {code}; redirects are not followed"),
                0,
            ),
            Vec::new(),
        );
    }
    let mut bytes = Vec::new();
    if response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_RESPONSE
    {
        return (
            status(
                probe,
                "unavailable",
                "Metadata response exceeded limits or could not be read",
                0,
            ),
            Vec::new(),
        );
    }
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| parse_models(&value, probe).ok())
    {
        Some(mut models) => {
            if local
                && probe.id != "ollama"
                && let Some(info) = lmstudio_models_with_headers(
                    &probe.spec.base_url,
                    probe.token.as_deref(),
                    &resolved_headers,
                )
            {
                annotate_lmstudio(&mut models, &info);
            }
            (
                status(probe, "reachable", "Live model metadata", models.len()),
                models,
            )
        }
        None => (
            status(
                probe,
                "unavailable",
                "Endpoint returned malformed model metadata",
                0,
            ),
            Vec::new(),
        ),
    }
}

/// Verify credentials with a provider endpoint before persisting them.
/// OpenRouter's `GET /api/v1/auth/key` is an auth check; `/models` is public and cannot validate keys.
pub fn verify_and_save_credential(
    user_dir: &Path,
    provider: &str,
    base_url: &str,
    token: &mut String,
) -> Result<()> {
    verify_and_save_credential_at(user_dir, provider, base_url, token, OPENROUTER_VALIDATE_URL)
}

fn verify_and_save_credential_at(
    user_dir: &Path,
    provider: &str,
    base_url: &str,
    token: &mut String,
    openrouter_validate_url: &str,
) -> Result<()> {
    let result = (|| {
        valid_token(token)?;
        match provider {
            "openai" | "anthropic" | "gemini" => {
                let spec = ProviderSpec {
                    label: provider.into(),
                    base_url: base_url.into(),
                    ..ProviderSpec::default()
                };
                let mut probe = Probe {
                    id: provider.into(),
                    spec,
                    token: Some(token.clone()),
                    blocked: None,
                };
                let (status, _) = probe_models(&probe);
                let verification = match status.state.as_str() {
                    "reachable" => Ok(()),
                    "auth-required" => Err(anyhow::anyhow!(
                        "{provider} rejected this credential; check the token and endpoint"
                    )),
                    _ => Err(anyhow::anyhow!(
                        "could not verify {provider} credential: {}",
                        status.detail
                    )),
                };
                if let Some(secret) = probe.token.as_mut() {
                    wipe_token(secret);
                }
                verification?;
            }
            "openrouter" => {
                ensure!(
                    base_url == OPENROUTER_BASE_URL,
                    "OpenRouter sign-in is only valid for its official endpoint"
                );
                verify_openrouter_key_at(openrouter_validate_url, token)?;
            }
            _ => {}
        }
        save_credential(user_dir, base_url, token)
    })();
    wipe_token(token);
    result
}

fn verify_openrouter_key_at(url: &str, token: &str) -> Result<()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    );
    let mut response = agent
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|_| anyhow::anyhow!("could not reach OpenRouter's key-validation endpoint"))?;
    match response.status().as_u16() {
        200..=299 => {
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| {
                    anyhow::anyhow!("could not read OpenRouter key-validation response")
                })?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "OpenRouter key-validation response exceeded limits"
            );
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| anyhow::anyhow!("OpenRouter key-validation response was invalid"))?;
            Ok(())
        }
        401 | 403 => anyhow::bail!("OpenRouter rejected this API key"),
        status => anyhow::bail!("OpenRouter key validation returned HTTP {status}"),
    }
}

fn wipe_token(token: &mut String) {
    let mut bytes = std::mem::take(token).into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

/// What LM Studio's native REST API knows that its OpenAI-compatible model list omits: whether a
/// model is an embedder, and whether it is loaded and with how much context. A model that is not
/// loaded gets loaded on first use, using whatever context LM Studio picks; that, not the model's
/// maximum, is the budget Dial has to live within.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmStudioModel {
    pub id: String,
    pub kind: String,
    pub loaded: bool,
    pub loaded_context: Option<u32>,
}

fn parse_lmstudio(value: &Value) -> Vec<LmStudioModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?.to_owned();
                    let loaded = row.get("state").and_then(Value::as_str) == Some("loaded");
                    let loaded_context = row
                        .get("loaded_context_length")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| loaded && *n > 0);
                    Some(LmStudioModel {
                        id,
                        kind: row
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("llm")
                            .to_owned(),
                        loaded,
                        loaded_context,
                    })
                })
                .take(MAX_MODELS)
                .collect()
        })
        .unwrap_or_default()
}

/// GET a native (non-OpenAI) endpoint of a local server. Only ever asked of loopback/private hosts; a
/// hosted API is never sent these probes.
fn local_get(
    base_url: &str,
    path: &str,
    token: Option<&str>,
    headers: &super::model_config::ResolvedHeaders<'_>,
) -> Option<Value> {
    local_request(base_url, path, None, token, headers)
}

/// [`local_get`], or a JSON POST when `body` is set (Ollama's `/api/show` is POST-only).
fn local_request(
    base_url: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
    headers: &super::model_config::ResolvedHeaders<'_>,
) -> Option<Value> {
    let uri = validated_url(base_url, true).ok()?;
    if !crate::llm::is_local_host(uri.host()?) {
        return None;
    }
    let origin = format!("{}://{}", uri.scheme_str()?, uri.authority()?);
    let builder = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(1)))
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None);
    let agent = ureq::Agent::new_with_config(builder.build());
    let url = format!("{origin}{path}");
    let mut response = match body {
        None => {
            let mut request = agent.get(&url);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            for (name, value) in headers.pairs() {
                request =
                    request.header(name, super::model_config::private_header_value(value).ok()?);
            }
            request.call().ok()?
        }
        Some(body) => {
            let mut request = agent.post(&url);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            for (name, value) in headers.pairs() {
                request =
                    request.header(name, super::model_config::private_header_value(value).ok()?);
            }
            request.send_json(body).ok()?
        }
    };
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn lmstudio_models_with_headers(
    base_url: &str,
    token: Option<&str>,
    headers: &super::model_config::ResolvedHeaders<'_>,
) -> Option<Vec<LmStudioModel>> {
    let found = parse_lmstudio(&local_get(base_url, "/api/v0/models", token, headers)?);
    (!found.is_empty()).then_some(found)
}

/// Whether the OpenAI-compatible server at `base_url` is llama.cpp's `llama-server`, recognised by
/// the `default_generation_settings` of its native `/props` document. Only loopback/private hosts
/// are asked (`local_request` refuses others). An answer is cached per base URL; a transport
/// failure is not, so a server that is still starting is probed again on the next request.
pub fn is_llama_server(
    base_url: &str,
    token: Option<&str>,
    headers: &[super::model_config::RequestHeader],
) -> bool {
    static KNOWN: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, bool>>> =
        std::sync::LazyLock::new(Default::default);
    let key = base_url.trim_end_matches('/').to_owned();
    if let Some(known) = KNOWN.lock().ok().and_then(|known| known.get(&key).copied()) {
        return known;
    }
    let Ok(resolved) = super::model_config::resolve_headers(headers) else {
        return false;
    };
    let Some(props) = local_get(base_url, "/props", token, &resolved) else {
        return false;
    };
    let llama = props.get("default_generation_settings").is_some();
    if let Ok(mut known) = KNOWN.lock() {
        known.insert(key, llama);
    }
    llama
}

/// Context window the local server is serving `model` with right now: LM Studio's loaded size,
/// llama.cpp's per-slot `n_ctx` (`-c` divided by `--parallel`), or the `context_length` Ollama loaded
/// the model with (its default is far below Dial's, and it truncates silently rather than failing).
/// `None` for hosted APIs, other servers, or a model that is not loaded yet.
pub fn loaded_context(
    base_url: &str,
    model: &str,
    token: Option<&str>,
    headers: &[super::model_config::RequestHeader],
) -> Option<u32> {
    let resolved = super::model_config::resolve_headers(headers).ok()?;
    if model.is_empty()
        || model.len() > 512
        || model
            .chars()
            .any(|c| c.is_control() || c == '?' || c == '#' || c == '%')
    {
        return None;
    }
    let positive = |value: Option<&Value>| {
        value?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let listed = if model.contains('/') {
        local_get(base_url, "/api/v0/models", token, &resolved)
            .map(|list| parse_lmstudio(&list))
            .and_then(|found| found.into_iter().find(|m| m.id == model))
            .map(|m| m.loaded_context)
    } else {
        local_get(
            base_url,
            &format!("/api/v0/models/{model}"),
            token,
            &resolved,
        )
        .map(|value| {
            parse_lmstudio(&serde_json::json!({"data":[value]}))
                .into_iter()
                .find(|m| m.id == model)
                .and_then(|m| m.loaded_context)
        })
    };
    if let Some(loaded) = listed {
        return loaded;
    }
    if let Some(props) = local_get(base_url, "/props", token, &resolved) {
        return positive(props.pointer("/default_generation_settings/n_ctx"));
    }
    if let Some(running) = local_get(base_url, "/api/ps", token, &resolved) {
        let bare = |name: &str| name.strip_suffix(":latest").unwrap_or(name).to_owned();
        let loaded = running["models"].as_array().and_then(|models| {
            models
                .iter()
                .find(|m| {
                    ["name", "model"].iter().any(|key| {
                        m[*key]
                            .as_str()
                            .is_some_and(|name| bare(name) == bare(model))
                    })
                })
                .and_then(|m| positive(m.get("context_length")))
        });
        if loaded.is_some() {
            return loaded;
        }
        // Not loaded yet: the model's trained context bounds the window it can usefully serve once
        // Dial's first request loads it with the planned window as `num_ctx`.
        let shown = local_request(
            base_url,
            "/api/show",
            Some(&serde_json::json!({ "model": model })),
            token,
            &resolved,
        )?;
        return shown["model_info"].as_object().and_then(|info| {
            info.iter()
                .find(|(key, _)| key.ends_with(".context_length"))
                .and_then(|(_, value)| positive(Some(value)))
        });
    }
    None
}

/// Explicit purpose tokens a row declares (`type`, `pipeline_tag`, `task`). Matching is exact: an
/// unfamiliar or differently spelled value stays `Unknown` rather than being guessed from its words.
fn kind_from_declared(token: &str) -> ModelKind {
    match token {
        "llm" | "vlm" | "chat" | "text-generation" | "conversational" | "image-text-to-text" => {
            ModelKind::Chat
        }
        "embeddings" | "embedding" | "feature-extraction" | "sentence-similarity" => {
            ModelKind::Embedding
        }
        "reranker" | "rerank" | "text-ranking" => ModelKind::Rerank,
        "image" | "text-to-image" => ModelKind::Image,
        "audio" | "text-to-speech" | "automatic-speech-recognition" => ModelKind::Audio,
        _ => ModelKind::Unknown,
    }
}

/// The purpose a row declares through an explicit field; see [`kind_from_declared`].
fn declared_kind(row: &Value) -> ModelKind {
    ["type", "pipeline_tag", "task"]
        .iter()
        .filter_map(|key| row.get(*key).and_then(Value::as_str))
        .map(kind_from_declared)
        .find(|kind| *kind != ModelKind::Unknown)
        .unwrap_or_default()
}

/// LM Studio's native `type` is the purpose source for its served models. A served embedder keeps
/// its availability: it is selectable for embedding roles and excluded from chat by its kind.
fn annotate_lmstudio(models: &mut [ModelEntry], info: &[LmStudioModel]) {
    for entry in models {
        let Some(known) = info.iter().find(|m| m.id == entry.selection.model) else {
            continue;
        };
        if entry.metadata.kind == ModelKind::Unknown {
            entry.metadata.kind = kind_from_declared(&known.kind);
        }
        if entry.metadata.kind == ModelKind::Embedding {
            entry.source = format!("{} · embedding model", entry.source);
        } else if known.loaded {
            entry.selection.context_window = known.loaded_context;
            entry.source = format!("{} · loaded", entry.source);
        } else {
            entry.source = format!("{} · loads on first use", entry.source);
        }
    }
}

/// Encoder architectures and model types that embed text without a language-model head.
const EMBEDDING_ARCHITECTURES: [&str; 11] = [
    "BertModel",
    "RobertaModel",
    "XLMRobertaModel",
    "DistilBertModel",
    "MPNetModel",
    "ModernBertModel",
    "NomicBertModel",
    "ElectraModel",
    "AlbertModel",
    "T5EncoderModel",
    "Qwen3ForTextEmbedding",
];
const EMBEDDING_MODEL_TYPES: [&str; 9] = [
    "bert",
    "roberta",
    "xlm-roberta",
    "distilbert",
    "mpnet",
    "modernbert",
    "nomic_bert",
    "electra",
    "albert",
];

/// Whether a local checkpoint is an embedding model, from its files: the sentence-transformers layout,
/// or an encoder `architectures`/`model_type` (causal and conditional generation models never qualify).
pub fn is_embedding_checkpoint(model: &Path) -> bool {
    if ["sentence_bert_config.json", "modules.json", "1_Pooling"]
        .iter()
        .any(|name| model.join(name).exists())
    {
        return true;
    }
    let Some(config) = read_json(&model.join("config.json")) else {
        return false;
    };
    let architectures: Vec<&str> = config
        .get("architectures")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if architectures
        .iter()
        .any(|name| name.ends_with("ForCausalLM") || name.ends_with("ForConditionalGeneration"))
    {
        return false;
    }
    if architectures
        .iter()
        .any(|name| EMBEDDING_ARCHITECTURES.contains(name))
    {
        return true;
    }
    config
        .get("model_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| EMBEDDING_MODEL_TYPES.contains(&kind))
}

/// Local embedding checkpoints cannot chat and Dial cannot run them directly, so they stay
/// unavailable until an embeddings endpoint serves them. Classified from checkpoint metadata only.
fn annotate_embedding_checkpoints(models: &mut [ModelEntry]) {
    for entry in models {
        if entry.selection.provider == "mlx"
            && entry.selection.base_url.is_none()
            && entry.is_embedding()
        {
            entry.available = false;
            entry.source = format!(
                "{} · embedding checkpoint, not served: serve it from LM Studio or an embeddings endpoint to use it",
                entry.source
            );
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_RESPONSE {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_RESPONSE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn snapshot(path: &Path) -> Option<Option<u32>> {
    let config = read_json(&path.join("config.json"))?;
    config.as_object()?;
    if path.join("model.safetensors.index.json").exists() {
        let index = read_json(&path.join("model.safetensors.index.json"))?;
        let weights = index.get("weight_map")?.as_object()?;
        if weights.is_empty()
            || !weights.values().all(|v| {
                v.as_str().is_some_and(|name| {
                    Path::new(name).components().count() == 1
                        && fs::metadata(path.join(name))
                            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
                })
            })
        {
            return None;
        }
    } else if !fs::read_dir(path)
        .ok()?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "safetensors")
                && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        })
    {
        return None;
    }
    Some(context(&config))
}
fn local_models(cache: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    let Ok(repos) = fs::read_dir(cache) else {
        return models;
    };
    let mut repos: Vec<_> = repos
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("models--"))
        .take(4096)
        .collect();
    repos.sort_by_key(|entry| entry.file_name());
    for repo in repos {
        if models.len() >= MAX_MODELS {
            break;
        }
        let root = repo.path();
        let main = fs::File::open(root.join("refs/main"))
            .ok()
            .and_then(|file| {
                let mut text = String::new();
                file.take(256).read_to_string(&mut text).ok()?;
                Some(text.trim().to_owned())
            })
            .filter(|s| {
                !s.is_empty()
                    && s.len() < 256
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        let mut candidates = Vec::new();
        if let Some(main) = main {
            candidates.push(root.join("snapshots").join(main));
        }
        if let Ok(entries) = fs::read_dir(root.join("snapshots")) {
            let mut revisions: Vec<_> = entries.filter_map(Result::ok).take(1024).collect();
            revisions.sort_by_key(|entry| {
                std::cmp::Reverse(entry.metadata().and_then(|m| m.modified()).ok())
            });
            candidates.extend(revisions.into_iter().map(|e| e.path()));
        }
        for path in candidates {
            let (window, gguf) = match snapshot(&path) {
                Some(window) => (window, false),
                None if super::local_format::gguf_file(&path).is_some() => (None, true),
                None => continue,
            };
            let label = repo
                .file_name()
                .to_string_lossy()
                .trim_start_matches("models--")
                .replace("--", "/");
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: path.to_string_lossy().into_owned(),
                    context_window: window,
                    ..ModelSelection::default()
                },
                label,
                source: if gguf {
                    "Hugging Face cache (GGUF)"
                } else {
                    "Hugging Face cache (MLX)"
                }
                .into(),
                available: true,
                metadata: ModelMetadata {
                    lora: lora_support(trainable, &path),
                    ..local_model_metadata(&path)
                },
            });
            break;
        }
    }
    models
}
/// Resolve local inference without asking Hugging Face to look up a revision.
pub fn resolve_local_model(model: &str) -> Result<String> {
    resolve_local_model_in(model, hf_cache().as_deref(), lmstudio_folder().as_deref())
}

fn resolve_local_model_in(
    model: &str,
    cache: Option<&Path>,
    studio: Option<&Path>,
) -> Result<String> {
    let path = Path::new(model);
    if snapshot(path).is_some() || super::local_format::gguf_file(path).is_some() {
        return Ok(fs::canonicalize(path)?.to_string_lossy().into_owned());
    }
    if super::local_format::detect(path) == Some(super::local_format::LocalFormat::Splash) {
        anyhow::bail!(
            "{model:?} is a Splash bundle, which only LM Studio can run: load it in LM Studio and select it under the LM Studio provider"
        );
    }
    let mut entries = cache
        .map(|cache| local_models(cache, None))
        .unwrap_or_default();
    if let Some(studio) = studio {
        entries.extend(lmstudio_mlx_models(studio, None));
        entries.extend(lmstudio_gguf_models(studio));
    }
    if let Some(entry) = entries.into_iter().find(|entry| entry.label == model) {
        return Ok(fs::canonicalize(entry.selection.model)?
            .to_string_lossy()
            .into_owned());
    }
    anyhow::bail!(
        "local model {model:?} has no complete downloaded weights (MLX safetensors or a GGUF file); inference is offline. Download this model first or select a downloaded local model in /models (for architecture maintenance, change the Architecture model in Settings → Model → Project understanding)"
    )
}

pub(crate) fn hf_cache() -> Option<PathBuf> {
    std::env::var_os("HF_HUB_CACHE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HF_HOME")
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(s).join("hub"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|s| PathBuf::from(s).join(".cache/huggingface/hub"))
        })
}
/// The `mlx_lm/models` directory of dial's Python environment (`dial setup` creates it under the user dir).
fn mlx_architectures(user_dir: &Path) -> Option<PathBuf> {
    let lib = user_dir.join("python/.venv/lib");
    fs::read_dir(&lib)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
        .map(|entry| entry.path().join("site-packages/mlx_lm/models"))
        .find(|dir| dir.is_dir())
}
/// Whether mlx-lm implements the architecture in `model/config.json` (`model_type`, or the text model's
/// for multimodal configs). A static check: no weights are loaded.
fn lora_support(architectures: Option<&Path>, model: &Path) -> Option<bool> {
    let architectures = architectures?;
    let config = read_json(&model.join("config.json"))?;
    let kind = config
        .get("model_type")
        .or_else(|| config.pointer("/text_config/model_type"))
        .and_then(Value::as_str)?;
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Some(false);
    }
    // MLX also resolves architecture aliases (for example gemma4_unified).
    // Absence of a same-named module cannot prove incompatibility.
    architectures
        .join(format!("{kind}.py"))
        .is_file()
        .then_some(true)
}
/// LM Studio's download folder: its `downloadsFolder` setting, else `~/.lmstudio/models`.
fn lmstudio_folder() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|s| !s.is_empty())?);
    let configured = read_json(&home.join(".lmstudio/settings.json")).and_then(|settings| {
        settings["downloadsFolder"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    Some(configured.unwrap_or_else(|| home.join(".lmstudio/models")))
}
/// MLX models LM Studio downloaded, laid out as `<folder>/<publisher>/<model>/`. Only folders that
/// pass the same check as Hugging Face snapshots (config plus complete safetensors) are listed, so GGUF
/// downloads and LM Studio-specific bundles are skipped. Entries are labelled `publisher/model`.
fn lmstudio_mlx_models(folder: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    let sorted = |dir: &Path| -> Vec<fs::DirEntry> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|it| it.filter_map(Result::ok).take(4096).collect())
            .unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        entries
    };
    let mut models = Vec::new();
    for publisher in sorted(folder) {
        for repo in sorted(&publisher.path()) {
            if models.len() >= MAX_MODELS {
                return models;
            }
            let path = repo.path();
            let Some(window) = snapshot(&path) else {
                continue;
            };
            let label = format!(
                "{}/{}",
                publisher.file_name().to_string_lossy(),
                repo.file_name().to_string_lossy()
            );
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: path.to_string_lossy().into_owned(),
                    context_window: window,
                    ..ModelSelection::default()
                },
                label,
                source: "LM Studio download (MLX)".into(),
                available: true,
                metadata: ModelMetadata {
                    lora: lora_support(trainable, &path),
                    ..local_model_metadata(&path)
                },
            });
        }
    }
    models
}
/// GGUF models LM Studio downloaded (`<folder>/<publisher>/<model>/*.gguf`), listed by the file
/// `llama-server` would load. Labelled `publisher/model`.
fn lmstudio_gguf_models(folder: &Path) -> Vec<ModelEntry> {
    let sorted = |dir: &Path| -> Vec<fs::DirEntry> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|it| it.filter_map(Result::ok).take(4096).collect())
            .unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        entries
    };
    let mut models = Vec::new();
    for publisher in sorted(folder) {
        for repo in sorted(&publisher.path()) {
            if models.len() >= MAX_MODELS {
                return models;
            }
            let Some(file) = super::local_format::gguf_file(&repo.path()) else {
                continue;
            };
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: file.to_string_lossy().into_owned(),
                    ..ModelSelection::default()
                },
                label: format!(
                    "{}/{}",
                    publisher.file_name().to_string_lossy(),
                    repo.file_name().to_string_lossy()
                ),
                source: "LM Studio download (GGUF)".into(),
                available: true,
                metadata: ModelMetadata {
                    vision: Some(false),
                    ..ModelMetadata::default()
                },
            });
        }
    }
    models
}
fn selection_cache_key(selection: &ModelSelection) -> String {
    use sha2::{Digest, Sha256};
    let scope = serde_json::json!([
        "dial-model-catalog-v2",
        selection.provider,
        selection
            .base_url
            .as_deref()
            .map(|s| s.trim_end_matches('/')),
        selection
            .protocol
            .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider)),
        selection.api_key_env,
        selection.headers.as_deref().unwrap_or_default()
    ]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}
fn probe_cache_key(probe: &Probe) -> String {
    selection_cache_key(&ModelSelection {
        provider: probe.id.clone(),
        base_url: Some(probe.spec.base_url.clone()),
        protocol: probe.spec.protocol,
        api_key_env: probe.spec.api_key_env.clone(),
        headers: probe.spec.headers.clone(),
        ..Default::default()
    })
}
fn cached(user_dir: &Path) -> BTreeMap<String, Vec<ModelEntry>> {
    let mut cache: BTreeMap<String, Vec<ModelEntry>> =
        read_private(&user_dir.join("model-catalog.json"))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    // Repair missing windows in older cached Claude rows without a network request or a rewrite.
    for entries in cache.values_mut() {
        for entry in entries {
            if entry.selection.context_window.is_none() {
                entry.selection.context_window = documented_context_window(&entry.selection);
            }
        }
    }
    // Migrate legacy URL-only caches by the declared policy of each row.
    let mut scoped: BTreeMap<String, Vec<ModelEntry>> = BTreeMap::new();
    for entry in cache.into_values().flatten() {
        scoped
            .entry(selection_cache_key(&entry.selection))
            .or_default()
            .push(entry);
    }
    scoped
}

fn documented_context_window(selection: &ModelSelection) -> Option<u32> {
    if selection.provider == "anthropic"
        && selection
            .base_url
            .as_deref()
            .is_none_or(|url| url.trim_end_matches('/') == super::anthropic::BASE_URL)
    {
        super::anthropic::documented_context_window(&selection.model)
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
                .remove(&selection_cache_key(selection))?
                .into_iter()
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
            if selection.provider == super::chatgpt::PROVIDER {
                super::chatgpt::scoped_model_context_window(
                    user_dir,
                    selection.account_id.as_deref(),
                    &selection.model,
                )
            } else if selection.provider == "mlx" && selection.base_url.is_none() {
                let model = resolve_local_model(&selection.model).ok()?;
                context(&read_json(&Path::new(&model).join("config.json"))?)
            } else {
                None
            }
        })
        .or_else(|| documented_context_window(selection))
}
const ANTHROPIC_SUBSCRIPTION_SEEDS: &[(&str, &str)] = &[
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5", "Claude Sonnet 5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-fable-5", "Claude Fable 5"),
    ("claude-mythos-5", "Claude Mythos 5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
    ("claude-mythos-5-1", "Claude Mythos 5.1"),
];

fn anthropic_subscription_catalog(
    probe: &Probe,
    status: &mut ProviderStatus,
    stale: Option<&[ModelEntry]>,
) -> Option<Vec<ModelEntry>> {
    if probe.id != "anthropic"
        || !probe
            .token
            .as_deref()
            .is_some_and(super::anthropic::subscription_token)
    {
        return None;
    }
    let mut models = stale
        .unwrap_or_default()
        .iter()
        .filter(|entry| {
            entry
                .selection
                .protocol
                .is_none_or(|p| p == ApiProtocol::Anthropic)
        })
        .take(MAX_MODELS)
        .cloned()
        .collect::<Vec<_>>();
    if models.is_empty() {
        models = ANTHROPIC_SUBSCRIPTION_SEEDS
            .iter()
            .map(|(id, label)| ModelEntry {
                selection: ModelSelection {
                    provider: "anthropic".into(),
                    model: (*id).into(),
                    base_url: Some(super::anthropic::BASE_URL.into()),
                    context_window: Some(1_000_000),
                    ..Default::default()
                },
                label: (*label).into(),
                source: "Claude subscription catalog (inference verifies model access)".into(),
                available: true,
                metadata: ModelMetadata {
                    vision: Some(true),
                    ..Default::default()
                },
            })
            .collect();
    }
    for model in &mut models {
        model.selection.provider = "anthropic".into();
        model.selection.base_url = Some(super::anthropic::BASE_URL.into());
        model.available = true;
        model.source =
            "Claude subscription catalog (cached; inference verifies model access)".into();
    }
    status.state = "cached".into();
    status.detail = format!(
        "Claude OAuth is signed in; live model discovery failed ({}). Cached or bundled models remain selectable; inference verifies model access.",
        status.detail
    );
    status.models = models.len();
    Some(models)
}
fn persist_cache(user_dir: &Path, updates: BTreeMap<String, Vec<ModelEntry>>) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let path = user_dir.join("model-catalog.json");
    let _lock = locked(&path)?;
    let mut cache = cached(user_dir);
    let fresh: Vec<String> = updates.keys().cloned().collect();
    cache.extend(updates);
    while cache.len() > MAX_PROVIDERS {
        let victim = cache
            .keys()
            .find(|key| !fresh.contains(key))
            .or_else(|| cache.keys().next())
            .cloned();
        match victim {
            Some(key) => {
                cache.remove(&key);
            }
            None => break,
        }
    }
    write_private(&path, &serde_json::to_vec(&cache)?)
}
fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider
        && a.model == b.model
        && a.base_url == b.base_url
        && a.protocol
            .unwrap_or_else(|| ApiProtocol::builtin(&a.provider))
            == b.protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&b.provider))
        && a.account_id == b.account_id
        && a.headers.as_deref().unwrap_or_default() == b.headers.as_deref().unwrap_or_default()
        && a.api_key_env == b.api_key_env
}

/// Downloaded local models (MLX checkpoints and GGUF files) from the Hugging Face cache and LM
/// Studio. Makes no network calls and loads no models; `discover` and `dial models recommend` share
/// this listing.
pub fn downloaded_local_models(user_dir: &Path) -> Vec<ModelEntry> {
    let trainable = mlx_architectures(user_dir);
    let mut models = Vec::new();
    if let Some(cache) = hf_cache() {
        models = local_models(&cache, trainable.as_deref());
    }
    if let Some(folder) = lmstudio_folder() {
        let gguf = lmstudio_gguf_models(&folder);
        // The same repository downloaded by both keeps its Hugging Face entry.
        for entry in lmstudio_mlx_models(&folder, trainable.as_deref())
            .into_iter()
            .chain(gguf)
        {
            if models.len() < MAX_MODELS && !models.iter().any(|known| known.label == entry.label) {
                models.push(entry);
            }
        }
    }
    models
}

/// Subscription OAuth providers with a bundled catalog row. A signed-in grant lists its bundled
/// models without a network probe (the grant only reaches the endpoint at inference); a signed-out
/// grant stays visible as `auth-required` so /providers can start sign-in.
fn oauth_catalog(
    user_dir: &Path,
    probed: &[(String, ProviderSpec)],
) -> Vec<(ProviderStatus, Vec<ModelEntry>)> {
    super::oauth::flows()
        .filter(|flow| !probed.iter().any(|(id, _)| id == flow.id))
        .filter_map(|flow| {
            let provider = builtin_provider(flow.id)?;
            let signed = super::oauth::signed_in(user_dir, flow.id);
            let base = provider.base_url.clone();
            let models: Vec<ModelEntry> = if signed {
                bundled_models(flow.id)
                    .into_iter()
                    .map(|model| ModelEntry {
                        selection: ModelSelection {
                            provider: flow.id.into(),
                            model: model.id.clone(),
                            base_url: Some(base.clone()),
                            protocol: Some(provider.protocol),
                            context_window: model.context_window,
                            ..Default::default()
                        },
                        label: model.id.clone(),
                        source: format!("{} bundled catalog", flow.name),
                        available: true,
                        metadata: model.metadata(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let status = ProviderStatus {
                id: flow.id.into(),
                label: provider.label.clone(),
                base_url: Some(base),
                state: if signed { "cached" } else { "auth-required" }.into(),
                detail: if signed {
                    "Signed in; models from the bundled catalog, verified at first request".into()
                } else {
                    "Sign in to use this subscription".into()
                },
                models: models.len(),
            };
            Some((status, models))
        })
        .collect()
}

pub fn discover(
    layout: &Layout,
    configured: &BTreeMap<String, ProviderSpec>,
    selections: &[ModelSelection],
) -> Discovery {
    let trainable = mlx_architectures(&layout.user_dir);
    let mut discovery = Discovery {
        models: downloaded_local_models(&layout.user_dir),
        ..Discovery::default()
    };
    discovery.providers.push(ProviderStatus {
        id: "mlx".into(),
        label: "Local models".into(),
        base_url: None,
        state: if discovery.models.is_empty() {
            "unavailable"
        } else {
            "cached"
        }
        .into(),
        detail: "Downloaded MLX and GGUF models, loaded on demand; nothing is downloaded".into(),
        models: discovery.models.len(),
    });
    let mut endpoints = BTreeMap::new();
    for (id, label, variable, default) in [
        (
            "lm-studio",
            "LM Studio",
            "LM_STUDIO_BASE_URL",
            "http://127.0.0.1:1234/v1",
        ),
        (
            "ollama",
            "Ollama",
            "OLLAMA_BASE_URL",
            super::ollama::BASE_URL,
        ),
        (
            "llama-cpp",
            "llama.cpp",
            "LLAMA_CPP_BASE_URL",
            "http://127.0.0.1:8080/v1",
        ),
    ] {
        let override_url = std::env::var(variable)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if id == "ollama" {
                    std::env::var("OLLAMA_HOST")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map(|s| {
                            if s.contains("://") {
                                s
                            } else {
                                format!("http://{s}")
                            }
                        })
                } else {
                    None
                }
            });
        endpoints.insert(
            id.to_owned(),
            ProviderSpec {
                label: label.into(),
                base_url: override_url.unwrap_or_else(|| default.into()),
                protocol: Some(ApiProtocol::builtin(id)),
                ..ProviderSpec::default()
            },
        );
    }
    for &(id, label, base, env) in KNOWN {
        endpoints.insert(
            id.into(),
            ProviderSpec {
                label: label.into(),
                base_url: base.into(),
                api_key_env: Some(env.into()),
                protocol: Some(ApiProtocol::builtin(id)),
                ..ProviderSpec::default()
            },
        );
    }
    endpoints.extend(
        configured
            .iter()
            .map(|(id, spec)| (id.clone(), spec.clone())),
    );
    let mut endpoints: Vec<_> = endpoints.into_iter().collect();
    for selection in selections {
        if let Some(base) = &selection.base_url {
            if configured
                .get(&selection.provider)
                .is_some_and(|spec| !spec.enabled)
            {
                continue;
            }
            let inherited = configured
                .get(&selection.provider)
                .filter(|spec| spec.base_url.trim_end_matches('/') == base.trim_end_matches('/'));
            let headers = selection
                .headers
                .as_ref()
                .or_else(|| inherited.and_then(|spec| spec.headers.as_ref()));
            let api_key_env = selection
                .api_key_env
                .as_ref()
                .or_else(|| inherited.and_then(|spec| spec.api_key_env.as_ref()));
            if endpoints.iter().any(|(id, spec)| {
                id == &selection.provider
                    && &spec.base_url == base
                    && spec.protocol.unwrap_or_else(|| ApiProtocol::builtin(id))
                        == selection
                            .protocol
                            .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider))
                    && spec.headers.as_deref().unwrap_or_default()
                        == headers.map(Vec::as_slice).unwrap_or_default()
                    && spec.api_key_env.as_ref() == api_key_env
            }) {
                continue;
            }
            endpoints.push((
                selection.provider.clone(),
                ProviderSpec {
                    label: selection.provider.clone(),
                    base_url: base.clone(),
                    api_key_env: api_key_env.cloned(),
                    allow_insecure: selection.allow_insecure,
                    protocol: selection.protocol,
                    headers: headers.cloned(),
                    ..ProviderSpec::default()
                },
            ));
        }
    }
    let storage_safe = layout.ensure_project_outside_storage().is_ok();
    if !storage_safe {
        discovery.providers.push(ProviderStatus {
            id: "storage".into(),
            label: "Private storage".into(),
            base_url: None,
            state: "unavailable".into(),
            detail: "Project must not be inside private storage; endpoint discovery skipped".into(),
            models: 0,
        });
    } else {
        if configured
            .get(super::chatgpt::PROVIDER)
            .is_some_and(|spec| !spec.enabled)
        {
            discovery.providers.push(ProviderStatus {
                id: super::chatgpt::PROVIDER.into(),
                label: "ChatGPT subscription".into(),
                base_url: Some(super::chatgpt::BASE_URL.into()),
                state: "disabled".into(),
                detail: "Disabled in configuration".into(),
                models: 0,
            });
        } else if configured
            .get(super::chatgpt::PROVIDER)
            .is_some_and(|spec| spec.headers.as_ref().is_some_and(|h| !h.is_empty()))
            || selections.iter().any(|selection| {
                selection.provider == super::chatgpt::PROVIDER
                    && selection.headers.as_ref().is_some_and(|h| !h.is_empty())
            })
        {
            discovery.providers.push(ProviderStatus {
                id: super::chatgpt::PROVIDER.into(),
                label: "ChatGPT subscription".into(),
                base_url: Some(super::chatgpt::BASE_URL.into()),
                state: "unavailable".into(),
                detail: "Subscription does not permit custom request headers".into(),
                models: 0,
            });
        } else {
            let (chatgpt, models) = super::chatgpt::discover(&layout.user_dir);
            discovery.providers.push(chatgpt);
            discovery.models.extend(models);
        }
        for (status, models) in oauth_catalog(&layout.user_dir, &endpoints) {
            discovery.providers.push(status);
            discovery.models.extend(models);
        }
        let cache = cached(&layout.user_dir);
        let probes: Vec<_> = endpoints
            .into_iter()
            .filter(|(id, _)| id != super::chatgpt::PROVIDER)
            .enumerate()
            .map(|(index, (id, spec))| {
                if index >= MAX_PROVIDERS {
                    Probe {
                        id,
                        spec,
                        token: None,
                        blocked: Some(("unavailable", "Endpoint probe limit reached")),
                    }
                } else {
                    let mut probe = prepare(id.clone(), spec, &layout.user_dir);
                    if id == "anthropic"
                        && probe
                            .spec
                            .protocol
                            .is_none_or(|p| p == ApiProtocol::Anthropic)
                        && super::anthropic_oauth::signed_in(&layout.user_dir)
                        && let Ok(token) = super::anthropic_oauth::access_token(
                            &layout.user_dir,
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        )
                    {
                        apply_anthropic_oauth_token(&mut probe, &token);
                    }
                    probe
                }
            })
            .collect();
        let mut updates = BTreeMap::new();
        // At most eight simultaneous probes, each with a total request deadline.
        for batch in probes.chunks(CONCURRENCY) {
            let results = std::thread::scope(|scope| {
                let jobs: Vec<_> = batch
                    .iter()
                    .map(|probe| scope.spawn(move || probe_models(probe)))
                    .collect();
                jobs.into_iter()
                    .zip(batch)
                    .map(|(job, probe)| {
                        job.join().unwrap_or_else(|_| {
                            (
                                status(probe, "unavailable", "Metadata worker failed", 0),
                                Vec::new(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            });
            for ((mut status, mut models), probe) in results.into_iter().zip(batch) {
                if status.state == "reachable" {
                    updates.insert(probe_cache_key(probe), models.clone());
                } else if status.state == "unavailable" {
                    let saved = cache.get(&probe_cache_key(probe)).map(Vec::as_slice);
                    if let Some(subscription_models) =
                        anthropic_subscription_catalog(probe, &mut status, saved)
                    {
                        models = subscription_models;
                    } else if status.state != "disabled"
                        && let Some(stale) = cache.get(&probe_cache_key(probe))
                    {
                        models = stale
                            .iter()
                            .filter(|entry| {
                                entry.selection.protocol.unwrap_or_else(|| {
                                    ApiProtocol::builtin(&entry.selection.provider)
                                }) == probe
                                    .spec
                                    .protocol
                                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                            })
                            .take(MAX_MODELS)
                            .cloned()
                            .map(|mut entry| {
                                entry.available = false;
                                entry.source = format!("{} (cached, unverified)", probe.spec.label);
                                entry.selection.provider = probe.id.clone();
                                entry.selection.api_key_env = probe.spec.api_key_env.clone();
                                entry.selection.allow_insecure = probe.spec.allow_insecure;
                                entry.selection.headers = probe.spec.headers.clone();
                                entry
                            })
                            .collect();
                        if !models.is_empty() {
                            status
                                .detail
                                .push_str("; showing cached metadata as unverified");
                            status.models = models.len();
                        }
                    }
                } else if status.state != "disabled"
                    && let Some(stale) = cache.get(&probe_cache_key(probe))
                {
                    models =
                        stale
                            .iter()
                            .filter(|entry| {
                                entry.selection.protocol.unwrap_or_else(|| {
                                    ApiProtocol::builtin(&entry.selection.provider)
                                }) == probe
                                    .spec
                                    .protocol
                                    .unwrap_or_else(|| ApiProtocol::builtin(&probe.id))
                            })
                            .take(MAX_MODELS)
                            .cloned()
                            .map(|mut entry| {
                                entry.available = false;
                                entry.source = format!("{} (cached, unverified)", probe.spec.label);
                                entry.selection.provider = probe.id.clone();
                                entry.selection.api_key_env = probe.spec.api_key_env.clone();
                                entry.selection.allow_insecure = probe.spec.allow_insecure;
                                entry.selection.headers = probe.spec.headers.clone();
                                entry
                            })
                            .collect();
                    if !models.is_empty() {
                        status
                            .detail
                            .push_str("; showing cached metadata as unverified");
                        status.models = models.len();
                    }
                }
                discovery.providers.push(status);
                discovery.models.extend(models);
            }
        }
        if persist_cache(&layout.user_dir, updates).is_err() {
            discovery.providers.push(ProviderStatus {
                id: "catalog-cache".into(),
                label: "Metadata cache".into(),
                base_url: None,
                state: "unavailable".into(),
                detail: "Live metadata could not be saved privately".into(),
                models: 0,
            });
        }
    }
    if storage_safe {
        let mut accounts = std::collections::BTreeSet::new();
        for selection in selections {
            if selection.provider == super::chatgpt::PROVIDER
                && let Some(id) = selection.account_id.as_deref()
                && accounts.insert(id)
                && let Ok(models) = super::chatgpt::scoped_cached_models(&layout.user_dir, id)
            {
                for entry in models {
                    if !discovery
                        .models
                        .iter()
                        .any(|m| same_model(&m.selection, &entry.selection))
                    {
                        discovery.models.push(entry);
                    }
                }
            }
        }
    }
    for selection in selections {
        if selection.model.trim().is_empty() {
            continue;
        }
        let mut selection = selection.clone();
        if let Some(spec) = configured.get(&selection.provider).filter(|spec| {
            selection.base_url.as_deref().is_some_and(|base| {
                base.trim_end_matches('/') == spec.base_url.trim_end_matches('/')
            })
        }) {
            if selection.headers.is_none() {
                selection.headers = spec.headers.clone();
            }
            if selection.api_key_env.is_none() {
                selection.api_key_env = spec.api_key_env.clone();
            }
        }
        if discovery
            .models
            .iter()
            .any(|m| same_model(&m.selection, &selection))
        {
            continue;
        }
        if selection
            .base_url
            .as_deref()
            .is_some_and(|url| validated_url(url, true).is_err())
        {
            selection.base_url = None;
        }
        let local = selection.provider == "mlx" && selection.base_url.is_none();
        let available = local
            && (snapshot(Path::new(&selection.model)).is_some()
                || super::local_format::gguf_file(Path::new(&selection.model)).is_some());
        let lora = if local {
            lora_support(trainable.as_deref(), Path::new(&selection.model))
        } else {
            None
        };
        let capabilities = if local {
            local_model_metadata(Path::new(&selection.model))
        } else {
            ModelMetadata::default()
        };
        discovery.models.push(ModelEntry {
            label: selection.model.clone(),
            selection,
            source: if available {
                "Configured local model"
            } else {
                "Configured (unverified)"
            }
            .into(),
            available,
            metadata: ModelMetadata {
                lora,
                ..capabilities
            },
        });
    }
    annotate_embedding_checkpoints(&mut discovery.models);
    discovery
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_pricing_splits_cache_writes_by_ttl() {
        let pricing = TokenPricing {
            prompt: 1e-6,
            completion: 2e-6,
            cache_read: Some(1e-7),
            cache_write: Some(3.75e-6),
        };
        let mut split = crate::llm::Reply {
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(0),
            cache_creation_tokens: Some(1_000_000),
            ..Default::default()
        };
        split.cache_creation_5m_tokens = Some(400_000);
        split.cache_creation_1h_tokens = Some(600_000);
        // 400k at the 5m rate (3.75e-6) plus 600k at the 1h rate (2x prompt = 2e-6).
        assert!((pricing.reply_cost(&split).unwrap() - 2.7).abs() < 1e-9);
        // Without a split, the whole write prices at the default write rate.
        let flat = crate::llm::Reply {
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            ..split
        };
        assert!((pricing.reply_cost(&flat).unwrap() - 3.75).abs() < 1e-9);
    }
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
    };
    #[test]
    fn picker_capabilities_preserve_declarations_and_unknown_cached_fields() {
        let info = metadata(&serde_json::json!({"capabilities":["vision","tools","thinking"]}));
        assert_eq!(
            (info.vision, info.tools, info.reasoning),
            (Some(true), Some(true), Some(true))
        );
        let info = metadata(
            &serde_json::json!({"supported_parameters":["temperature","tools","reasoning_effort"]}),
        );
        assert_eq!((info.tools, info.reasoning), (Some(true), Some(true)));
        let info = metadata(&serde_json::json!({"tools":false,"supported_parameters":["tools"]}));
        assert_eq!(info.tools, Some(false));
        let info: ModelMetadata =
            serde_json::from_value(serde_json::json!({"vision":true})).unwrap();
        assert_eq!((info.tools, info.reasoning), (None, None));
        assert_eq!(
            metadata(&serde_json::json!({"id":"vision-reasoning-tools-model"})).tools,
            None
        );
    }

    #[test]
    fn model_kind_policy_is_shared_and_serialized_snake_case() {
        assert!(ModelKind::Chat.supports_chat());
        assert!(ModelKind::Unknown.supports_chat());
        for kind in [
            ModelKind::Embedding,
            ModelKind::Rerank,
            ModelKind::Image,
            ModelKind::Audio,
        ] {
            assert!(!kind.supports_chat(), "{}", kind.label());
        }
        assert_eq!(ModelKind::default(), ModelKind::Unknown);
        assert_eq!(ModelKind::Embedding.label(), "embedding");
        assert_eq!(
            serde_json::to_string(&ModelKind::Rerank).unwrap(),
            "\"rerank\""
        );
        let entry = |kind| ModelEntry {
            selection: ModelSelection::default(),
            label: "m".into(),
            source: "s".into(),
            available: true,
            metadata: ModelMetadata {
                kind,
                ..Default::default()
            },
        };
        assert!(entry(ModelKind::Embedding).is_embedding());
        assert!(!entry(ModelKind::Embedding).supports_chat());
        assert!(!entry(ModelKind::Chat).is_embedding());
        assert!(entry(ModelKind::Unknown).supports_chat());
        // Cache rows written before the field existed read back as Unknown.
        let legacy: ModelMetadata = serde_json::from_str(r#"{"vision":true}"#).unwrap();
        assert_eq!(legacy.kind, ModelKind::Unknown);
    }

    #[test]
    fn declared_kinds_match_only_exact_documented_tokens() {
        assert_eq!(kind_from_declared("embeddings"), ModelKind::Embedding);
        assert_eq!(
            kind_from_declared("feature-extraction"),
            ModelKind::Embedding
        );
        assert_eq!(kind_from_declared("llm"), ModelKind::Chat);
        assert_eq!(kind_from_declared("text-generation"), ModelKind::Chat);
        assert_eq!(kind_from_declared("text-to-image"), ModelKind::Image);
        assert_eq!(kind_from_declared("text-to-speech"), ModelKind::Audio);
        assert_eq!(kind_from_declared("reranker"), ModelKind::Rerank);
        // Names and near-misses are never classified by their words.
        for token in [
            "Embedding Model",
            "my-embeddings-llm",
            "text-embedding-3-small",
            "",
        ] {
            assert_eq!(kind_from_declared(token), ModelKind::Unknown, "{token}");
        }
        let kind_of = |json: Value| metadata(&json).kind;
        assert_eq!(
            kind_of(serde_json::json!({"type": "embeddings"})),
            ModelKind::Embedding
        );
        assert_eq!(
            kind_of(serde_json::json!({"task": "feature-extraction"})),
            ModelKind::Embedding
        );
        assert_eq!(
            kind_of(serde_json::json!({"pipeline_tag": "text-generation"})),
            ModelKind::Chat
        );
        assert_eq!(
            kind_of(serde_json::json!({"id": "text-embedding-3-small"})),
            ModelKind::Unknown
        );
    }

    #[test]
    fn lmstudio_served_embedders_stay_available_for_embedding_roles() {
        let served = |id: &str, available: bool| ModelEntry {
            selection: ModelSelection {
                provider: "lm-studio".into(),
                model: id.into(),
                base_url: Some("http://127.0.0.1:1234/v1".into()),
                ..Default::default()
            },
            label: id.into(),
            source: "LM Studio".into(),
            available,
            metadata: ModelMetadata::default(),
        };
        let listed = |id: &str, kind: &str| LmStudioModel {
            id: id.into(),
            kind: kind.into(),
            loaded: false,
            loaded_context: None,
        };
        let mut models = vec![
            served("nomic-embed", true),
            served("qwen-chat", true),
            served("cached-embed", false),
        ];
        annotate_lmstudio(
            &mut models,
            &[
                listed("nomic-embed", "embeddings"),
                listed("qwen-chat", "llm"),
                listed("cached-embed", "embeddings"),
            ],
        );
        assert!(models[0].available && models[0].is_embedding());
        assert!(!models[0].supports_chat());
        assert!(models[1].available && models[1].supports_chat());
        // Annotation never changes availability: an unverified row stays unavailable.
        assert!(!models[2].available && models[2].is_embedding());
    }

    #[test]
    fn local_embedding_checkpoints_are_classified_and_unavailable_until_served() {
        let root = tempfile::tempdir().unwrap();
        let encoder = root.path().join("encoder");
        fs::create_dir_all(&encoder).unwrap();
        fs::write(
            encoder.join("config.json"),
            r#"{"architectures":["BertModel"],"model_type":"bert"}"#,
        )
        .unwrap();
        let chat = root.path().join("chat");
        fs::create_dir_all(&chat).unwrap();
        fs::write(
            chat.join("config.json"),
            r#"{"architectures":["LlamaForCausalLM"],"model_type":"llama"}"#,
        )
        .unwrap();
        assert_eq!(local_model_metadata(&encoder).kind, ModelKind::Embedding);
        assert_eq!(local_model_metadata(&chat).kind, ModelKind::Unknown);
        let checkpoint = |path: &Path| ModelEntry {
            selection: ModelSelection {
                provider: "mlx".into(),
                model: path.to_string_lossy().into_owned(),
                ..Default::default()
            },
            label: "checkpoint".into(),
            source: "Hugging Face cache (MLX)".into(),
            available: true,
            metadata: local_model_metadata(path),
        };
        let mut models = vec![checkpoint(&encoder), checkpoint(&chat)];
        annotate_embedding_checkpoints(&mut models);
        assert!(!models[0].available && models[0].is_embedding());
        assert!(models[0].source.contains("not served"));
        assert!(models[1].available && models[1].supports_chat());
    }

    #[test]
    fn embedding_checkpoints_are_detected_from_metadata_not_names() {
        let root = tempfile::tempdir().unwrap();
        let sentence = root.path().join("plain-name");
        fs::create_dir_all(sentence.join("1_Pooling")).unwrap();
        fs::write(sentence.join("config.json"), r#"{"model_type":"qwen3"}"#).unwrap();
        assert!(is_embedding_checkpoint(&sentence));
        let encoder = root.path().join("encoder");
        fs::create_dir_all(&encoder).unwrap();
        fs::write(
            encoder.join("config.json"),
            r#"{"architectures":["XLMRobertaModel"],"model_type":"xlm-roberta"}"#,
        )
        .unwrap();
        assert!(is_embedding_checkpoint(&encoder));
        let chat = root.path().join("embed-named-chat");
        fs::create_dir_all(&chat).unwrap();
        fs::write(
            chat.join("config.json"),
            r#"{"architectures":["BertForCausalLM"],"model_type":"bert"}"#,
        )
        .unwrap();
        assert!(!is_embedding_checkpoint(&chat));
    }

    #[test]
    fn local_picker_capabilities_read_templates_without_loading_weights() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("config.json"), r#"{"vision_config":{}}"#).unwrap();
        fs::write(root.path().join("tokenizer_config.json"), serde_json::json!({
            "chat_template":[{"name":"tool_use","template":"{% if tools %}<tool_call>{% endif %}"},
                {"name":"default","template":"{% if enable_thinking %}<think></think>{% endif %}"}]
        }).to_string()).unwrap();
        let info = local_model_metadata(root.path());
        assert_eq!(
            (info.vision, info.tools, info.reasoning),
            (Some(true), Some(true), Some(true))
        );
        fs::write(
            root.path().join("config.json"),
            r#"{"tools":false,"reasoning":false}"#,
        )
        .unwrap();
        let info = local_model_metadata(root.path());
        assert_eq!(
            (info.vision, info.tools, info.reasoning),
            (Some(false), Some(false), Some(false))
        );
        fs::remove_file(root.path().join("tokenizer_config.json")).unwrap();
        fs::write(root.path().join("config.json"), "{}").unwrap();
        fs::write(
            root.path().join("chat_template.jinja"),
            "{% if tools %}<tool_call>{% endif %}",
        )
        .unwrap();
        assert_eq!(local_model_metadata(root.path()).tools, Some(true));
    }

    #[test]
    fn provider_metadata_preserves_supported_prices_and_vision() {
        let info = metadata(
            &serde_json::json!({"architecture":{"input_modalities":["text","image"]},"pricing":{"prompt":"0.000001","completion":"0.000003"}}),
        );
        assert_eq!(info.vision, Some(true));
        assert!((info.pricing.unwrap().cost(1000, 500).unwrap() - 0.0025).abs() < 1e-12);
        assert_eq!(
            metadata(&serde_json::json!({"input_modalities":["text"]})).vision,
            Some(false)
        );
        assert_eq!(metadata(&serde_json::json!({})).vision, None);
        for value in [
            serde_json::json!({"prompt":"NaN","completion":"1"}),
            serde_json::json!({"prompt":-1,"completion":1}),
            serde_json::json!({"prompt":"1"}),
        ] {
            assert!(
                metadata(&serde_json::json!({"pricing":value}))
                    .pricing
                    .is_none()
            );
        }
    }
    #[test]
    fn claude_subscription_keeps_cached_models_selectable_when_model_listing_is_unavailable() {
        let mut probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
        probe.token = Some("sk-ant-oat01-test".into());
        let mut provider_status = status(&probe, "unavailable", "Endpoint returned HTTP 404", 0);
        let models = anthropic_subscription_catalog(&probe, &mut provider_status, None).unwrap();
        assert_eq!(provider_status.state, "cached");
        assert!(
            provider_status
                .detail
                .contains("inference verifies model access")
        );
        assert!(
            models
                .iter()
                .any(|model| model.selection.model == "claude-sonnet-5-5" && model.available)
        );

        let mut cached = ModelEntry {
            selection: ModelSelection {
                provider: "anthropic".into(),
                model: "claude-sonnet-5-5".into(),
                base_url: Some(super::super::anthropic::BASE_URL.into()),
                ..Default::default()
            },
            label: "Sonnet".into(),
            source: "cached".into(),
            available: false,
            metadata: ModelMetadata::default(),
        };
        let mut provider_status = status(&probe, "unavailable", "endpoint offline", 0);
        let recovered = anthropic_subscription_catalog(
            &probe,
            &mut provider_status,
            Some(std::slice::from_mut(&mut cached)),
        )
        .unwrap();
        assert_eq!(provider_status.state, "cached");
        assert_eq!(
            recovered[0].source,
            "Claude subscription catalog (cached; inference verifies model access)"
        );

        probe.token = Some("sk-ant-api03-key".into());
        let mut provider_status = status(&probe, "unavailable", "endpoint offline", 0);
        assert!(
            anthropic_subscription_catalog(&probe, &mut provider_status, None).is_none(),
            "API-key discovery failures must not be treated as Claude subscription login"
        );
    }
    #[test]
    fn claude_oauth_token_replaces_missing_api_key_block() {
        let mut probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
        probe.blocked = Some((
            "auth-required",
            "Set the configured credential environment variable or save a URL-scoped token",
        ));
        apply_anthropic_oauth_token(
            &mut probe,
            &super::super::auth::SecretToken::new("sk-ant-oat01-test".into()),
        );
        assert_eq!(probe.token.as_deref(), Some("sk-ant-oat01-test"));
        assert!(probe.blocked.is_none());
    }
    fn server(code: u16, body: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let job = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            write!(stream, "HTTP/1.1 {code} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        });
        (format!("http://{address}/v1"), job)
    }
    fn test_probe(url: String, id: &str) -> Probe {
        Probe {
            id: id.into(),
            spec: ProviderSpec {
                label: id.into(),
                base_url: url,
                ..ProviderSpec::default()
            },
            token: None,
            blocked: None,
        }
    }
    #[test]
    fn probes_real_http_models_and_keeps_unknown_context_unknown() {
        let (url, job) = server(
            200,
            r#"{"data":[{"id":"one","context_length":12345},{"id":"two"},{"id":"one"}]}"#.into(),
        );
        let (status, models) = probe_models(&test_probe(url, "custom"));
        assert_eq!(status.state, "reachable");
        assert_eq!(
            models
                .iter()
                .map(|m| (&*m.selection.model, m.selection.context_window))
                .collect::<Vec<_>>(),
            [("one", Some(12345)), ("two", None)]
        );
        assert!(job.join().unwrap().starts_with("GET /v1/models "));
        let (url, job) = server(200, r#"{"models":[{"name":"llama:latest"}]}"#.into());
        let (_, models) = probe_models(&test_probe(url, "ollama"));
        assert_eq!(models[0].selection.model, "llama:latest");
        assert!(job.join().unwrap().starts_with("GET /api/tags "));
    }
    #[test]
    fn auth_redirect_invalid_and_oversized_responses_are_explicit_failures() {
        for (code, body, expected) in [
            (401, "secret must not appear".into(), "auth-required"),
            (302, "redirect".into(), "unavailable"),
            (200, "invalid".into(), "unavailable"),
            (200, "x".repeat(MAX_RESPONSE as usize + 1), "unavailable"),
        ] {
            let (url, job) = server(code, body);
            let (status, models) = probe_models(&test_probe(url, "custom"));
            assert_eq!(status.state, expected);
            assert!(models.is_empty());
            assert!(!status.detail.contains("secret"));
            job.join().unwrap();
        }
    }
    #[test]
    fn first_party_key_verification_uses_provider_auth_and_saves_only_after_success() {
        let home = tempfile::tempdir().unwrap();
        let (url, job) = server(200, r#"{"data":[{"id":"claude-test"}]}"#.into());
        let mut key = "test-secret".to_owned();
        verify_and_save_credential(home.path(), "anthropic", &url, &mut key).unwrap();
        assert!(key.is_empty());
        let request = job.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models "));
        assert!(
            request.contains("x-api-key: test-secret")
                && request.contains("anthropic-version: 2023-06-01")
        );
        assert!(!request.contains("authorization:"));
        assert_eq!(
            credential_token(home.path(), &url).unwrap().as_deref(),
            Some("test-secret")
        );

        let rejected_home = tempfile::tempdir().unwrap();
        let (url, job) = server(401, r#"{"error":"invalid key"}"#.into());
        let mut key = "bad-secret".to_owned();
        let error = verify_and_save_credential(rejected_home.path(), "openai", &url, &mut key)
            .unwrap_err()
            .to_string();
        let request = job.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer bad-secret"));
        assert!(error.contains("rejected") && !error.contains("bad-secret"));
        assert!(key.is_empty());
        assert_eq!(
            credential_token(rejected_home.path(), &url).unwrap(),
            None,
            "rejected keys are never persisted"
        );
    }
    #[test]
    fn openrouter_key_validation_precedes_persistence() {
        let home = tempfile::tempdir().unwrap();
        let (url, job) = server(200, r#"{"data":{"label":"Dial"}}"#.into());
        let validate = url.replace("/v1", "/api/v1/auth/key");
        let mut key = "sk-or-valid".to_owned();
        verify_and_save_credential_at(
            home.path(),
            "openrouter",
            OPENROUTER_BASE_URL,
            &mut key,
            &validate,
        )
        .unwrap();
        assert!(key.is_empty());
        let request = job.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/v1/auth/key "));
        assert!(request.contains("authorization: bearer sk-or-valid"));
        assert_eq!(
            credential_token(home.path(), OPENROUTER_BASE_URL)
                .unwrap()
                .as_deref(),
            Some("sk-or-valid")
        );

        let rejected_home = tempfile::tempdir().unwrap();
        let (url, job) = server(401, r#"{"error":"no"}"#.into());
        let mut key = "sk-or-invalid".to_owned();
        let validate = url.replace("/v1", "/api/v1/auth/key");
        let error = verify_and_save_credential_at(
            rejected_home.path(),
            "openrouter",
            OPENROUTER_BASE_URL,
            &mut key,
            &validate,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("rejected") && !error.contains("sk-or-invalid"));
        assert!(key.is_empty());
        job.join().unwrap();
        assert_eq!(
            credential_token(rejected_home.path(), OPENROUTER_BASE_URL).unwrap(),
            None
        );
    }
    #[test]
    fn key_verification_rejects_unreachable_metadata_without_persisting() {
        let home = tempfile::tempdir().unwrap();
        let (url, job) = server(500, r#"{"error":"upstream"}"#.into());
        let mut key = "test-secret".to_owned();
        let error = verify_and_save_credential(home.path(), "anthropic", &url, &mut key)
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 500") && !error.contains("test-secret"));
        assert!(
            key.is_empty(),
            "the caller's key buffer is cleared whether verification succeeds or fails"
        );
        job.join().unwrap();
        assert_eq!(credential_token(home.path(), &url).unwrap(), None);
    }
    #[test]
    fn credentials_are_url_scoped_private_and_preserve_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        save_credential(dir.path(), "https://first.test/v1", "first-secret").unwrap();
        save_credential(dir.path(), "https://second.test/v1", "second-secret").unwrap();
        remove_credential(dir.path(), "https://first.test/v1").unwrap();
        assert_eq!(
            credential_token(dir.path(), "https://first.test/v1").unwrap(),
            None
        );
        assert_eq!(
            credential_token(dir.path(), "https://second.test/v1")
                .unwrap()
                .as_deref(),
            Some("second-secret")
        );
        assert_eq!(
            credential_token(dir.path(), "https://second.test/other").unwrap(),
            None
        );
        for url in [
            "https://user:secret@first.test/v1",
            "https://first.test/v1?key=secret",
            "https://first.test/v1#secret",
        ] {
            assert!(save_credential(dir.path(), url, "ok").is_err());
        }
        assert_eq!(
            crate::platform::fs::mode(&fs::metadata(dir.path().join("credentials.json")).unwrap())
                & 0o777,
            0o600
        );
        let uri = "https://unrelated.test/v1".parse().unwrap();
        assert!(env_token("OPENAI_API_KEY", &uri).is_err());
    }
    #[test]
    fn hf_main_revision_wins_and_incomplete_snapshots_are_ignored() {
        let cache = tempfile::tempdir().unwrap();
        let repo = cache.path().join("models--mlx-community--example");
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(repo.join("refs/main"), "current\n").unwrap();
        for name in ["old", "current"] {
            let path = repo.join("snapshots").join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("config.json"),
                r#"{"max_position_embeddings":8192}"#,
            )
            .unwrap();
            fs::write(path.join("model.safetensors"), "weights").unwrap();
        }
        let entries = local_models(cache.path(), None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].selection.model.ends_with("/current"));
        assert_eq!(entries[0].selection.context_window, Some(8192));
        fs::write(
            repo.join("snapshots/current/model.safetensors.index.json"),
            r#"{"weight_map":{"w":"missing.safetensors"}}"#,
        )
        .unwrap();
        assert!(
            local_models(cache.path(), None)[0]
                .selection
                .model
                .ends_with("/old")
        );
    }
    #[test]
    fn local_resolution_uses_complete_snapshot_without_main_ref() {
        let cache = tempfile::tempdir().unwrap();
        assert!(
            resolve_local_model_in("mlx-community/missing", Some(cache.path()), None)
                .unwrap_err()
                .to_string()
                .contains("no complete downloaded weights")
        );
        let path = cache
            .path()
            .join("models--mlx-community--example/snapshots/cached");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("config.json"), "{}").unwrap();
        fs::write(path.join("model.safetensors"), "weights").unwrap();
        let resolved =
            resolve_local_model_in("mlx-community/example", Some(cache.path()), None).unwrap();
        assert_eq!(Path::new(&resolved), fs::canonicalize(&path).unwrap());
        assert_eq!(
            resolve_local_model_in(&resolved, None, None).unwrap(),
            resolved
        );
        fs::write(
            path.join("model.safetensors.index.json"),
            r#"{"weight_map":{"w":"missing.safetensors"}}"#,
        )
        .unwrap();
        let error =
            resolve_local_model_in("mlx-community/example", Some(cache.path()), None).unwrap_err();
        assert!(error.to_string().contains("no complete downloaded weights"));
        assert!(error.to_string().contains("Architecture model"));
    }

    #[test]
    fn lm_studio_mlx_downloads_are_listed_and_other_formats_skipped() {
        let folder = tempfile::tempdir().unwrap();
        let mlx = folder.path().join("lmstudio-community/Qwen-MLX-8bit");
        fs::create_dir_all(&mlx).unwrap();
        fs::write(
            mlx.join("config.json"),
            r#"{"model_type":"qwen3_5","vision_config":{},"text_config":{"max_position_embeddings":32768}}"#,
        )
        .unwrap();
        let gemma = folder.path().join("lmstudio-community/gemma-MLX");
        fs::create_dir_all(&gemma).unwrap();
        fs::write(
            gemma.join("config.json"),
            r#"{"text_config":{"model_type":"gemma4_unified"}}"#,
        )
        .unwrap();
        fs::write(gemma.join("model.safetensors"), "weights").unwrap();
        fs::write(mlx.join("model.safetensors"), "weights").unwrap();
        let gguf = folder.path().join("lmstudio-community/Qwen-GGUF");
        fs::create_dir_all(&gguf).unwrap();
        fs::write(gguf.join("model-Q4_K_M.gguf"), "weights").unwrap();
        fs::create_dir_all(folder.path().join("incoai/Bundle/target")).unwrap();
        let architectures = folder.path().join("mlx_lm/models");
        fs::create_dir_all(&architectures).unwrap();
        fs::write(architectures.join("qwen3_5.py"), "").unwrap();
        let entries = lmstudio_mlx_models(folder.path(), Some(&architectures));
        assert_eq!(entries[0].metadata.vision, Some(true));
        assert_eq!(entries[1].metadata.vision, Some(false));
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.label.as_str(), e.metadata.lora))
                .collect::<Vec<_>>(),
            [
                ("lmstudio-community/Qwen-MLX-8bit", Some(true)),
                ("lmstudio-community/gemma-MLX", None)
            ]
        );
        assert_eq!(
            lmstudio_mlx_models(folder.path(), None)[0].metadata.lora,
            None,
            "unknown without an mlx-lm install"
        );
        assert_eq!(entries[0].selection.model, mlx.to_string_lossy());
        assert_eq!(entries[0].selection.context_window, Some(32768));
        assert!(
            entries[0].selection.base_url.is_none(),
            "served by dial's MLX server, not LM Studio"
        );
    }
    #[test]
    fn disabled_endpoint_is_never_contacted() {
        let dir = tempfile::tempdir().unwrap();
        let probe = prepare(
            "custom".into(),
            ProviderSpec {
                base_url: "http://127.0.0.1:1".into(),
                enabled: false,
                ..ProviderSpec::default()
            },
            dir.path(),
        );
        let (status, models) = probe_models(&probe);
        assert_eq!(status.state, "disabled");
        assert!(models.is_empty());
    }
    #[test]
    fn failed_live_probe_retains_cached_and_configured_models_as_unverified() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let (url, job) = server(401, "{}".into());
        let selection = ModelSelection {
            provider: "custom".into(),
            model: "saved".into(),
            base_url: Some(url.clone()),
            ..ModelSelection::default()
        };
        persist_cache(
            &layout.user_dir,
            BTreeMap::from([(
                url.clone(),
                vec![ModelEntry {
                    selection: selection.clone(),
                    label: "saved".into(),
                    source: "prior live probe".into(),
                    available: true,
                    metadata: ModelMetadata::default(),
                }],
            )]),
        )
        .unwrap();
        let mut configured = BTreeMap::new();
        for id in ["lm-studio", "ollama", "llama-cpp"]
            .into_iter()
            .chain(KNOWN.iter().map(|row| row.0))
        {
            configured.insert(
                id.into(),
                ProviderSpec {
                    enabled: false,
                    ..ProviderSpec::default()
                },
            );
        }
        configured.insert(
            "custom".into(),
            ProviderSpec {
                base_url: url,
                ..ProviderSpec::default()
            },
        );
        let mut another = selection.clone();
        another.model = "not-in-cache".into();
        let result = discover(&layout, &configured, &[selection, another]);
        assert_eq!(
            result
                .providers
                .iter()
                .find(|s| s.id == "custom")
                .unwrap()
                .state,
            "auth-required"
        );
        let models: Vec<_> = result
            .models
            .iter()
            .filter(|m| m.selection.provider == "custom")
            .collect();
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| !m.available));
        assert!(
            models
                .iter()
                .any(|m| m.selection.model == "saved" && m.source.contains("cached"))
        );
        assert!(models.iter().any(|m| m.selection.model == "not-in-cache"));
        job.join().unwrap();
    }
    #[test]
    fn simultaneous_credential_saves_do_not_lose_other_urls() {
        let home = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let path = home.path();
                scope.spawn(move || {
                    save_credential(path, &format!("https://provider{n}.test/v1"), "private-key")
                        .unwrap()
                });
            }
        });
        for n in 0..8 {
            assert_eq!(
                credential_token(home.path(), &format!("https://provider{n}.test/v1"))
                    .unwrap()
                    .as_deref(),
                Some("private-key")
            );
        }
    }
    #[test]
    fn metadata_caps_models_and_rejects_invalid_context_values() {
        let probe = test_probe("http://127.0.0.1:1234/v1".into(), "custom");
        let rows: Vec<_> = (0..MAX_MODELS + 20).map(|i| serde_json::json!({"id":format!("model-{i}"), "context_length":if i == 0 { u64::MAX } else { 0 }})).collect();
        let models = parse_models(&serde_json::json!({"data":rows}), &probe).unwrap();
        assert_eq!(models.len(), MAX_MODELS);
        assert!(
            models
                .iter()
                .all(|entry| entry.selection.context_window.is_none())
        );
        assert!(
            models
                .iter()
                .all(|entry| entry.selection.model != format!("model-{MAX_MODELS}"))
        );
    }

    #[test]
    fn anthropic_catalog_reads_input_context_without_confusing_output_limits() {
        let probe = test_probe(super::super::anthropic::BASE_URL.into(), "anthropic");
        let entries = parse_models(
            &serde_json::json!({"data":[
                {"id":"claude-opus-5-5","max_input_tokens":1_000_000,"max_tokens":128_000},
                {"id":"claude-custom","max_input_tokens":750_000,"max_tokens":32_000},
                {"id":"claude-unknown","max_input_tokens":null,"max_tokens":128_000}
            ]}),
            &probe,
        )
        .unwrap();
        assert_eq!(entries[0].selection.context_window, Some(1_000_000));
        assert_eq!(entries[1].selection.context_window, Some(750_000));
        assert_eq!(entries[2].selection.context_window, None);
        assert_eq!(entries[0].metadata.max_output_tokens, Some(128_000));
        assert_eq!(entries[1].metadata.max_output_tokens, Some(32_000));
    }

    #[test]
    fn old_claude_cache_recovers_context_without_network_or_rewriting_saved_metadata() {
        let home = tempfile::tempdir().unwrap();
        let selection = ModelSelection {
            provider: "anthropic".into(),
            model: "claude-opus-5-5".into(),
            base_url: Some(super::super::anthropic::BASE_URL.into()),
            ..Default::default()
        };
        let mut row = ModelEntry {
            selection: selection.clone(),
            label: "Opus".into(),
            source: "cached".into(),
            available: false,
            metadata: Default::default(),
        };
        let path = home.path().join("model-catalog.json");
        let bytes = serde_json::to_vec(&BTreeMap::from([(
            super::super::anthropic::BASE_URL,
            vec![row.clone()],
        )]))
        .unwrap();
        write_private(&path, &bytes).unwrap();
        assert_eq!(
            model_context_window(home.path(), &selection),
            Some(1_000_000)
        );
        assert_eq!(
            cached(home.path())[&selection_cache_key(&selection)][0]
                .selection
                .context_window,
            Some(1_000_000)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        row.selection.context_window = Some(400_000);
        write_private(
            &path,
            &serde_json::to_vec(&BTreeMap::from([(
                super::super::anthropic::BASE_URL,
                vec![row],
            )]))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(model_context_window(home.path(), &selection), Some(400_000));
        let mut selected = selection.clone();
        selected.context_window = Some(64_000);
        assert_eq!(model_context_window(home.path(), &selected), Some(64_000));
        selected.context_window = None;
        selected.base_url = Some("https://proxy.example/v1".into());
        assert_eq!(model_context_window(home.path(), &selected), None);
        selected.base_url = selection.base_url;
        selected.model = "claude-future-99".into();
        assert_eq!(model_context_window(home.path(), &selected), None);
    }
    #[test]
    fn malformed_credentials_fail_closed_without_echoing_content() {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("credentials.json"),
            "secret-token malformed",
        )
        .unwrap();
        let error =
            save_credential(home.path(), "https://provider.test/v1", "new-token").unwrap_err();
        assert!(!format!("{error:#}").contains("secret-token"));
        assert_eq!(
            fs::read_to_string(home.path().join("credentials.json")).unwrap(),
            "secret-token malformed"
        );
    }
    /// Answers any number of requests by path; unknown paths get 404 like a server without the native API.
    fn routed(routes: Vec<(&'static str, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).ok();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (code, body) = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map(|(_, body)| (200, body.clone()))
                    .unwrap_or((404, "{}".into()));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{address}/v1")
    }
    fn lm_studio_routes() -> Vec<(&'static str, String)> {
        let native = serde_json::json!({"data":[
            {"id":"warm","type":"llm","state":"loaded","max_context_length":1_048_576,"loaded_context_length":8192},
            {"id":"cold","type":"vlm","state":"not-loaded","max_context_length":262_144},
            {"id":"embedder","type":"embeddings","state":"not-loaded","max_context_length":2048},
        ]});
        vec![
            ("/v1/models", r#"{"data":[{"id":"warm"},{"id":"cold"},{"id":"embedder"}]}"#.into()),
            ("/api/v0/models", native.to_string()),
            ("/api/v0/models/warm", serde_json::json!({"id":"warm","type":"llm","state":"loaded","max_context_length":1_048_576,"loaded_context_length":8192}).to_string()),
            ("/api/v0/models/cold", serde_json::json!({"id":"cold","type":"vlm","state":"not-loaded","max_context_length":262_144}).to_string()),
        ]
    }
    #[test]
    fn lm_studio_discovery_keeps_embedders_available_and_reports_only_loaded_context() {
        let url = routed(lm_studio_routes());
        let (status, models) = probe_models(&test_probe(url, "lm-studio"));
        assert_eq!(status.state, "reachable");
        let by = |id: &str| models.iter().find(|m| m.selection.model == id).unwrap();
        assert_eq!(
            by("warm").selection.context_window,
            Some(8192),
            "loaded size is the budget"
        );
        assert!(by("warm").source.contains("loaded") && by("warm").available);
        assert_eq!(
            by("cold").selection.context_window,
            None,
            "an unloaded model's maximum must never become its window"
        );
        assert!(by("cold").source.contains("loads on first use") && by("cold").available);
        assert!(
            by("embedder").available
                && by("embedder").is_embedding()
                && !by("embedder").supports_chat(),
            "served embedders are available for embedding roles, not chat"
        );
        assert!(
            models
                .iter()
                .all(|m| m.selection.context_window != Some(1_048_576)
                    && m.selection.context_window != Some(262_144))
        );
    }
    #[test]
    fn llama_server_is_recognised_by_props_on_local_hosts_only() {
        let llama = routed(vec![(
            "/props",
            r#"{"default_generation_settings":{"n_ctx":4096}}"#.into(),
        )]);
        assert!(is_llama_server(&llama, None, &[]));
        let other = routed(vec![]);
        assert!(
            !is_llama_server(&other, None, &[]),
            "/props 404 is not llama.cpp"
        );
        assert!(!is_llama_server(
            "https://api.example.invalid/v1",
            Some("secret"),
            &[]
        ));
    }
    #[test]
    fn servers_without_the_native_api_are_listed_unchanged() {
        let url = routed(vec![(
            "/v1/models",
            r#"{"data":[{"id":"a"},{"id":"b"}]}"#.into(),
        )]);
        let (status, models) = probe_models(&test_probe(url, "llama-cpp"));
        assert_eq!(status.state, "reachable");
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m.available
            && m.selection.context_window.is_none()
            && m.source == "llama-cpp"));
    }
    #[test]
    fn loaded_context_is_exact_local_only_and_rejects_hostile_model_names() {
        let url = routed(lm_studio_routes());
        assert_eq!(loaded_context(&url, "warm", None, &[]), Some(8192));
        assert_eq!(loaded_context(&url, "cold", None, &[]), None, "not loaded");
        assert_eq!(loaded_context(&url, "missing", None, &[]), None);
        for hostile in ["warm?x=1", "warm#frag", "warm%2e", "", "a\nb"] {
            assert_eq!(
                loaded_context(&url, hostile, None, &[]),
                None,
                "{hostile:?}"
            );
        }
        // A hosted API is never sent LM Studio probes, and the answer cannot come from the network.
        assert_eq!(
            loaded_context("https://api.example.com/v1", "warm", None, &[]),
            None
        );
    }
    #[test]
    fn llama_cpp_and_ollama_report_the_context_they_actually_serve() {
        // llama.cpp advertises its training context in /v1/models; the served per-slot size is in /props.
        let llama = routed(vec![
            (
                "/v1/models",
                r#"{"data":[{"id":"qwen","meta":{"n_ctx_train":131072}}]}"#.into(),
            ),
            (
                "/props",
                r#"{"default_generation_settings":{"n_ctx":4096}}"#.into(),
            ),
        ]);
        assert_eq!(loaded_context(&llama, "qwen", None, &[]), Some(4096));
        let ollama = routed(vec![("/api/ps", r#"{"models":[{"name":"qwen2.5:7b","model":"qwen2.5:7b","context_length":4096},{"name":"llama3:latest","model":"llama3:latest","context_length":8192}]}"#.into())]);
        assert_eq!(loaded_context(&ollama, "qwen2.5:7b", None, &[]), Some(4096));
        assert_eq!(
            loaded_context(&ollama, "llama3", None, &[]),
            Some(8192),
            ":latest is Ollama's implicit tag"
        );
        assert_eq!(
            loaded_context(&ollama, "mistral", None, &[]),
            None,
            "not loaded yet"
        );
        let cold = routed(vec![
            ("/api/ps", r#"{"models":[]}"#.into()),
            (
                "/api/show",
                r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":40960}}"#
                    .into(),
            ),
        ]);
        assert_eq!(
            loaded_context(&cold, "qwen3:8b", None, &[]),
            Some(40960),
            "an unloaded Ollama model is bounded by its trained context"
        );
        assert_eq!(
            loaded_context(&routed(vec![]), "anything", None, &[]),
            None,
            "a server with none of the native APIs"
        );
    }
}
