//! Metadata-only discovery: no subnet scan, inference, or model downloads.
//! Saved credentials are plaintext JSON, protected by owner-only directories/files,
//! not an OS keychain. They are URL-scoped and written only on explicit user action.
use super::model_config::{ModelSelection, ProviderSpec};
use crate::storage::{paths::Layout, settings_store::{locked, read_private, write_private}};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::{BTreeMap, BTreeSet}, fs, io::Read, path::{Path, PathBuf}, time::Duration};

const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
const MAX_MODELS: usize = 512;
const MAX_PROVIDERS: usize = 64;
const CONCURRENCY: usize = 8;
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_VALIDATE_URL: &str = "https://openrouter.ai/api/v1/auth/key";
const KNOWN: &[(&str, &str, &str, &str)] = &[
    ("openai", "OpenAI", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    ("anthropic", "Anthropic", "https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"),
    ("openrouter", "OpenRouter", OPENROUTER_BASE_URL, "OPENROUTER_API_KEY"),
    ("groq", "Groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    ("mistral", "Mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    ("deepseek", "DeepSeek", "https://api.deepseek.com/v1", "DEEPSEEK_API_KEY"),
];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelEntry {
    pub selection: ModelSelection, pub label: String, pub source: String, pub available: bool,
    #[serde(default)]
    pub metadata: ModelMetadata,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub vision: Option<bool>,
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
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoraCheck { Waiting, Checking, Failed }
/// USD per token, as supplied by provider metadata; no guessed price tables.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TokenPricing {
    pub prompt: f64, pub completion: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}
impl TokenPricing {
    pub fn cost(self, prompt: u64, completion: u64) -> Option<f64> {
        if ![self.prompt, self.completion].iter().all(|n| n.is_finite() && *n >= 0.0) { return None; }
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
        for (tokens, rate) in [(read, self.cache_read), (write, self.cache_write)] {
            if tokens > 0 {
                let rate = rate.filter(|n| n.is_finite() && *n >= 0.0)?;
                cost += tokens as f64 * rate;
            }
        }
        cost.is_finite().then_some(cost)
    }
}

fn metadata(row: &Value) -> ModelMetadata {
    let modalities = row.pointer("/architecture/input_modalities").or_else(|| row.get("input_modalities")).and_then(Value::as_array);
    let vision = modalities.map(|items| items.iter().any(|item| item == "image"))
        .or_else(|| row.pointer("/capabilities/vision").and_then(Value::as_bool))
        .or_else(|| row.get("vision").and_then(Value::as_bool))
        .or_else(|| row.get("vision_config").filter(|config| config.is_object()).map(|_| true));
    let price = |key| row.pointer(key).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .filter(|n| n.is_finite() && *n >= 0.0);
    let pricing = price("/pricing/prompt").zip(price("/pricing/completion"))
        .map(|(prompt, completion)| TokenPricing { prompt, completion, cache_read:price("/pricing/input_cache_read"), cache_write:price("/pricing/input_cache_write") });
    ModelMetadata { vision, pricing, ..Default::default() }
}

/// Preliminary MLX architecture availability; this does not verify adapter training targets.
pub fn local_lora_support(user_dir: &Path, model: &Path) -> Option<bool> {
    lora_support(mlx_architectures(user_dir).as_deref(), model)
}
/// Cache-only lookup: never make a metadata request in the inference hot path.
pub fn model_metadata(layout: &Layout, selection: &ModelSelection) -> ModelMetadata {
    if selection.provider == super::chatgpt::PROVIDER {
        return ModelMetadata { vision:super::chatgpt::model_profile(&layout.user_dir,&selection.model).ok().flatten().and_then(|profile|profile.capabilities.vision), ..Default::default() };
    }
    if selection.provider == "mlx" && selection.base_url.is_none() {
        return read_json(&Path::new(&selection.model).join("config.json")).map(|value| metadata(&value)).unwrap_or_default();
    }
    selection.base_url.as_ref().and_then(|url| cached(&layout.user_dir).remove(url))
        .and_then(|entries| entries.into_iter().find(|entry| entry.selection.model == selection.model))
        .map(|entry| entry.metadata).unwrap_or_default()
}
#[derive(Clone, Debug)]
pub struct ProviderStatus { pub id: String, pub label: String, pub base_url: Option<String>, pub state: String, pub detail: String, pub models: usize }
#[derive(Clone, Default)]
pub struct Discovery { pub models: Vec<ModelEntry>, pub providers: Vec<ProviderStatus> }

fn validated_url(base: &str, allow_insecure: bool) -> Result<ureq::http::Uri> {
    ensure!(!base.contains('#'), "endpoint URL must not contain a fragment");
    // Endpoint validation deliberately does not echo the input, which could be a secret.
    crate::llm::provider::endpoint_url(base, allow_insecure).map_err(|_| anyhow::anyhow!("invalid or insecure endpoint URL; credentials and queries are forbidden"))?;
    base.parse().map_err(|_| anyhow::anyhow!("invalid endpoint URL"))
}
fn valid_token(token: &str) -> Result<()> {
    ensure!(!token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()), "credential must be nonempty printable ASCII without whitespace");
    Ok(())
}
fn credential_map(user_dir: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = read_private(&user_dir.join("credentials.json"))?;
    match bytes {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("credential file is invalid; no changes saved")),
        None => Ok(BTreeMap::new()),
    }
}
pub fn credential_token(user_dir: &Path, base_url: &str) -> Result<Option<String>> {
    validated_url(base_url, true)?;
    let path = user_dir.join("credentials.json"); let _lock = locked(&path)?;
    let token = credential_map(user_dir)?.remove(base_url);
    if let Some(token) = &token { valid_token(token)?; }
    Ok(token)
}
pub fn save_credential(user_dir: &Path, base_url: &str, token: &str) -> Result<()> {
    validated_url(base_url, true)?; valid_token(token)?;
    let path = user_dir.join("credentials.json"); let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.insert(base_url.to_owned(), token.to_owned());
    write_private(&path, &serde_json::to_vec(&credentials)?)
}
pub fn remove_credential(user_dir: &Path, base_url: &str) -> Result<()> {
    validated_url(base_url, true)?;
    let path = user_dir.join("credentials.json"); let _lock = locked(&path)?;
    let mut credentials = credential_map(user_dir)?;
    credentials.remove(base_url);
    write_private(&path, &serde_json::to_vec(&credentials)?)
}

// Never derive Debug: this transient object can contain a bearer credential.
struct Probe { id: String, spec: ProviderSpec, token: Option<String>, blocked: Option<(&'static str, &'static str)> }
fn origin(uri: &ureq::http::Uri) -> (String, String, u16) {
    (uri.scheme_str().unwrap_or("").to_ascii_lowercase(), uri.host().unwrap_or("").to_ascii_lowercase(), uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 }))
}
fn env_token(name: &str, uri: &ureq::http::Uri) -> Result<Option<String>> {
    if let Some((_, _, url, _)) = KNOWN.iter().find(|(_, _, _, key)| *key == name) {
        let known: ureq::http::Uri = url.parse()?;
        ensure!(origin(uri) == origin(&known), "provider credential environment variable does not match endpoint origin");
    }
    let token = std::env::var(name).ok().filter(|s| !s.is_empty());
    if let Some(token) = &token { valid_token(token)?; }
    Ok(token)
}

/// Shared inference/discovery credential precedence, with provider-origin checks for known env keys.
pub fn resolve_credential(user_dir: &Path, base_url: &str, variable: Option<&str>) -> Result<Option<String>> {
    let uri = validated_url(base_url, true)?;
    let variable = variable.or_else(|| KNOWN.iter().find(|(_, _, url, _)|
        url.parse::<ureq::http::Uri>().is_ok_and(|known| origin(&known) == origin(&uri))).map(|(_, _, _, key)| *key));
    if let Some(variable) = variable {
        if let Some(token) = env_token(variable, &uri)? { return Ok(Some(token)); }
    }
    let token = if user_dir.as_os_str().is_empty() { None } else { credential_token(user_dir, base_url)? };
    ensure!(variable.is_none() || token.is_some(), "provider credentials are missing; configure an API key in /providers or set its credential environment variable");
    Ok(token)
}
fn prepare(id: String, spec: ProviderSpec, user_dir: &Path) -> Probe {
    let mut probe = Probe { id, spec, token: None, blocked: None };
    if !probe.spec.enabled { probe.blocked = Some(("disabled", "Disabled in configuration; not probed")); return probe; }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) {
        Ok(uri) => uri,
        Err(_) => { probe.blocked = Some(("unavailable", "Invalid or insecure endpoint URL")); return probe; }
    };
    let env = probe.spec.api_key_env.as_deref().or_else(|| KNOWN.iter().find(|(_, _, base, _)| base.parse::<ureq::http::Uri>().is_ok_and(|known| origin(&known) == origin(&uri))).map(|(_, _, _, key)| *key));
    let token = match env.map(|name| env_token(name, &uri)).transpose() {
        Ok(token) => token.flatten(),
        Err(_) => { probe.blocked = Some(("auth-required", "Credential environment variable is invalid or belongs to a different provider origin")); return probe; }
    };
    probe.token = match token { Some(token) => Some(token), None => match credential_token(user_dir, &probe.spec.base_url) {
        Ok(token) => token,
        Err(_) => { probe.blocked = Some(("auth-required", "Saved credentials cannot be read safely")); return probe; }
    }};
    if env.is_some() && probe.token.is_none() { probe.blocked = Some(("auth-required", "Set the configured credential environment variable or save a URL-scoped token")); }
    probe
}
fn status(probe: &Probe, state: &str, detail: &str, models: usize) -> ProviderStatus {
    ProviderStatus { id:probe.id.clone(), label:if probe.spec.label.is_empty() { probe.id.clone() } else { probe.spec.label.clone() }, base_url:validated_url(&probe.spec.base_url, true).ok().map(|_| probe.spec.base_url.clone()), state:state.into(), detail:detail.into(), models }
}
fn context(value: &Value) -> Option<u32> {
    ["/context_window", "/context_length", "/max_context_length", "/max_position_embeddings", "/text_config/max_position_embeddings", "/top_provider/context_length", "/meta/n_ctx_train", "/details/context_length"]
        .iter().filter_map(|key| value.pointer(key)).find_map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0))
}
fn parse_models(value: &Value, probe: &Probe) -> Result<Vec<ModelEntry>> {
    let rows = value.get(if probe.id == "ollama" { "models" } else { "data" }).and_then(Value::as_array).ok_or_else(|| anyhow::anyhow!("model response has no model array"))?;
    let mut seen = BTreeSet::new();
    Ok(rows.iter().filter_map(|row| {
        let model = row.get("id").or_else(|| row.get("model")).or_else(|| row.get("name"))?.as_str()?.trim();
        if model.is_empty() || model.len() > 4096 || model.chars().any(char::is_control) || !seen.insert(model.to_owned()) { return None; }
        Some(ModelEntry { selection:ModelSelection { provider:probe.id.clone(), model:model.into(), base_url:Some(probe.spec.base_url.clone()), api_key_env:probe.spec.api_key_env.clone(), allow_insecure:probe.spec.allow_insecure, context_window:context(row) }, label:model.into(), source:probe.spec.label.clone(), available:true, metadata:metadata(row) })
    }).take(MAX_MODELS).collect())
}
fn probe_models(probe: &Probe) -> (ProviderStatus, Vec<ModelEntry>) {
    if let Some((state, detail)) = probe.blocked { return (status(probe, state, detail, 0), Vec::new()); }
    let uri = match validated_url(&probe.spec.base_url, probe.spec.allow_insecure) { Ok(uri) => uri, Err(_) => return (status(probe, "unavailable", "Invalid endpoint URL", 0), Vec::new()) };
    let local = crate::llm::is_local_host(uri.host().unwrap_or(""));
    let builder = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(if local { 1 } else { 5 }))).http_status_as_error(false).max_redirects(0);
    let builder = if local { builder.proxy(None) } else { builder };
    let agent = ureq::Agent::new_with_config(builder.build());
    let url = if probe.id == "ollama" {
        let base = probe.spec.base_url.trim_end_matches('/').trim_end_matches("/v1").trim_end_matches("/api");
        format!("{base}/api/tags")
    } else {
        let chat = match crate::llm::provider::endpoint_url(&probe.spec.base_url, probe.spec.allow_insecure) { Ok(url) => url, Err(_) => return (status(probe, "unavailable", "Invalid endpoint URL", 0), Vec::new()) };
        format!("{}models", chat.strip_suffix("chat/completions").unwrap_or(&chat))
    };
    let mut request = agent.get(&url);
    if let Some(token) = &probe.token {
        if probe.id == "anthropic" {
            request = request.header("x-api-key", token).header("anthropic-version", "2023-06-01");
        } else {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
    }
    let mut response = match request.call() { Ok(response) => response, Err(_) => return (status(probe, "unavailable", "Connection failed or metadata probe timed out", 0), Vec::new()) };
    let code = response.status().as_u16();
    if code == 401 || code == 403 { return (status(probe, "auth-required", "Endpoint requires valid credentials", 0), Vec::new()); }
    if !(200..300).contains(&code) { return (status(probe, "unavailable", &format!("Endpoint returned HTTP {code}; redirects are not followed"), 0), Vec::new()); }
    let mut bytes = Vec::new();
    if response.body_mut().as_reader().take(MAX_RESPONSE + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_RESPONSE {
        return (status(probe, "unavailable", "Metadata response exceeded limits or could not be read", 0), Vec::new());
    }
    match serde_json::from_slice::<Value>(&bytes).ok().and_then(|value| parse_models(&value, probe).ok()) {
        Some(mut models) => {
            if local && probe.id != "ollama" {
                if let Some(info) = lmstudio_models(&probe.spec.base_url, probe.token.as_deref()) { annotate_lmstudio(&mut models, &info); }
            }
            (status(probe, "reachable", "Live model metadata", models.len()), models)
        }
        None => (status(probe, "unavailable", "Endpoint returned malformed model metadata", 0), Vec::new()),
    }
}

/// Verify credentials with a provider endpoint before persisting them.
/// OpenRouter's `GET /api/v1/auth/key` is an auth check; `/models` is public and cannot validate keys.
pub fn verify_and_save_credential(user_dir: &Path, provider: &str, base_url: &str, token: &mut String) -> Result<()> {
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
            "openai" | "anthropic" => {
                let spec = ProviderSpec { label: provider.into(), base_url: base_url.into(), ..ProviderSpec::default() };
                let mut probe = Probe { id: provider.into(), spec, token: Some(token.clone()), blocked: None };
                let (status, _) = probe_models(&probe);
                let verification = match status.state.as_str() {
                    "reachable" => Ok(()),
                    "auth-required" => Err(anyhow::anyhow!("{provider} rejected this API key")),
                    _ => Err(anyhow::anyhow!("could not verify {provider} API key: {}", status.detail)),
                };
                if let Some(secret) = probe.token.as_mut() { wipe_token(secret); }
                verification?;
            }
            "openrouter" => {
                ensure!(base_url == OPENROUTER_BASE_URL, "OpenRouter sign-in is only valid for its official endpoint");
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
    let mut response = agent.get(url).header("Authorization", format!("Bearer {token}"))
        .call().map_err(|_| anyhow::anyhow!("could not reach OpenRouter's key-validation endpoint"))?;
    match response.status().as_u16() {
        200..=299 => {
            let mut bytes = Vec::new();
            response.body_mut().as_reader().take(64 * 1024 + 1).read_to_end(&mut bytes)
                .map_err(|_| anyhow::anyhow!("could not read OpenRouter key-validation response"))?;
            ensure!(bytes.len() <= 64 * 1024, "OpenRouter key-validation response exceeded limits");
            serde_json::from_slice::<Value>(&bytes).map_err(|_| anyhow::anyhow!("OpenRouter key-validation response was invalid"))?;
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
pub struct LmStudioModel { pub id: String, pub kind: String, pub loaded: bool, pub loaded_context: Option<u32> }

fn parse_lmstudio(value: &Value) -> Vec<LmStudioModel> {
    value.get("data").and_then(Value::as_array).map(|rows| rows.iter().filter_map(|row| {
        let id = row.get("id")?.as_str()?.to_owned();
        let loaded = row.get("state").and_then(Value::as_str) == Some("loaded");
        let loaded_context = row.get("loaded_context_length").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).filter(|n| loaded && *n > 0);
        Some(LmStudioModel { id, kind: row.get("type").and_then(Value::as_str).unwrap_or("llm").to_owned(), loaded, loaded_context })
    }).take(MAX_MODELS).collect()).unwrap_or_default()
}

/// GET a native (non-OpenAI) endpoint of a local server. Only ever asked of loopback/private hosts; a
/// hosted API is never sent these probes.
fn local_get(base_url: &str, path: &str, token: Option<&str>) -> Option<Value> {
    let uri = validated_url(base_url, true).ok()?;
    if !crate::llm::is_local_host(uri.host()?) { return None; }
    let origin = format!("{}://{}", uri.scheme_str()?, uri.authority()?);
    let builder = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(1))).http_status_as_error(false).max_redirects(0).proxy(None);
    let agent = ureq::Agent::new_with_config(builder.build());
    let mut request = agent.get(&format!("{origin}{path}"));
    if let Some(token) = token { request = request.header("Authorization", format!("Bearer {token}")); }
    let mut response = request.call().ok()?;
    if !response.status().is_success() { return None; }
    let mut bytes = Vec::new();
    response.body_mut().as_reader().take(MAX_RESPONSE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_RESPONSE { return None; }
    serde_json::from_slice(&bytes).ok()
}

pub fn lmstudio_models(base_url: &str, token: Option<&str>) -> Option<Vec<LmStudioModel>> {
    let found = parse_lmstudio(&local_get(base_url, "/api/v0/models", token)?);
    (!found.is_empty()).then_some(found)
}

/// Context window the local server is serving `model` with right now: LM Studio's loaded size,
/// llama.cpp's per-slot `n_ctx` (`-c` divided by `--parallel`), or the `context_length` Ollama loaded
/// the model with (its default is far below Dial's, and it truncates silently rather than failing).
/// `None` for hosted APIs, other servers, or a model that is not loaded yet.
pub fn loaded_context(base_url: &str, model: &str, token: Option<&str>) -> Option<u32> {
    if model.is_empty() || model.len() > 512 || model.chars().any(|c| c.is_control() || c == '?' || c == '#' || c == '%') { return None; }
    let positive = |value: Option<&Value>| value?.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    if let Some(value) = local_get(base_url, &format!("/api/v0/models/{model}"), token) {
        return parse_lmstudio(&serde_json::json!({"data":[value]})).into_iter().find(|m| m.id == model)?.loaded_context;
    }
    if let Some(props) = local_get(base_url, "/props", token) {
        return positive(props.pointer("/default_generation_settings/n_ctx"));
    }
    let running = local_get(base_url, "/api/ps", token)?;
    let bare = |name: &str| name.strip_suffix(":latest").unwrap_or(name).to_owned();
    running["models"].as_array()?.iter()
        .find(|m| ["name", "model"].iter().any(|key| m[*key].as_str().is_some_and(|name| bare(name) == bare(model))))
        .and_then(|m| positive(m.get("context_length")))
}

fn annotate_lmstudio(models: &mut [ModelEntry], info: &[LmStudioModel]) {
    for entry in models {
        let Some(known) = info.iter().find(|m| m.id == entry.selection.model) else { continue; };
        if known.kind == "embeddings" {
            entry.available = false;
            entry.source = format!("{} · embedding model, not for chat", entry.source);
        } else if known.loaded {
            entry.selection.context_window = known.loaded_context;
            entry.source = format!("{} · loaded", entry.source);
        } else {
            entry.source = format!("{} · loads on first use", entry.source);
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_RESPONSE { return None; }
    let mut bytes = Vec::new(); file.take(MAX_RESPONSE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_RESPONSE { return None; }
    serde_json::from_slice(&bytes).ok()
}
fn snapshot(path: &Path) -> Option<Option<u32>> {
    let config = read_json(&path.join("config.json"))?;
    config.as_object()?;
    if path.join("model.safetensors.index.json").exists() {
        let index = read_json(&path.join("model.safetensors.index.json"))?;
        let weights = index.get("weight_map")?.as_object()?;
        if weights.is_empty() || !weights.values().all(|v| v.as_str().is_some_and(|name| Path::new(name).components().count() == 1 && fs::metadata(path.join(name)).is_ok_and(|meta| meta.is_file() && meta.len() > 0))) { return None; }
    } else if !fs::read_dir(path).ok()?.filter_map(Result::ok).any(|entry| entry.path().extension().is_some_and(|ext| ext == "safetensors") && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file() && meta.len() > 0)) { return None; }
    Some(context(&config))
}
fn local_models(cache: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    let Ok(repos) = fs::read_dir(cache) else { return models; };
    let mut repos: Vec<_> = repos.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("models--")).take(4096).collect();
    repos.sort_by_key(|entry| entry.file_name());
    for repo in repos {
        if models.len() >= MAX_MODELS { break; }
        let root = repo.path();
        let main = fs::File::open(root.join("refs/main")).ok().and_then(|file| { let mut text = String::new(); file.take(256).read_to_string(&mut text).ok()?; Some(text.trim().to_owned()) }).filter(|s| !s.is_empty() && s.len() < 256 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        let mut candidates = Vec::new();
        if let Some(main) = main { candidates.push(root.join("snapshots").join(main)); }
        if let Ok(entries) = fs::read_dir(root.join("snapshots")) {
            let mut revisions: Vec<_> = entries.filter_map(Result::ok).take(1024).collect();
            revisions.sort_by_key(|entry| std::cmp::Reverse(entry.metadata().and_then(|m| m.modified()).ok()));
            candidates.extend(revisions.into_iter().map(|e| e.path()));
        }
        for path in candidates {
            let Some(window) = snapshot(&path) else { continue; };
            let label = repo.file_name().to_string_lossy().trim_start_matches("models--").replace("--", "/");
            models.push(ModelEntry { selection:ModelSelection { model:path.to_string_lossy().into_owned(), context_window:window, ..ModelSelection::default() }, label, source:"Hugging Face cache (MLX)".into(), available:true, metadata:ModelMetadata { lora: lora_support(trainable, &path), ..ModelMetadata::default() } });
            break;
        }
    }
    models
}
fn hf_cache() -> Option<PathBuf> {
    std::env::var_os("HF_HUB_CACHE").filter(|s| !s.is_empty()).map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").filter(|s| !s.is_empty()).map(|s| PathBuf::from(s).join("hub")))
        .or_else(|| std::env::var_os("HOME").map(|s| PathBuf::from(s).join(".cache/huggingface/hub")))
}
/// The `mlx_lm/models` directory of dial's Python environment (`dial setup` creates it under the user dir).
fn mlx_architectures(user_dir: &Path) -> Option<PathBuf> {
    let lib = user_dir.join("python/.venv/lib");
    fs::read_dir(&lib).ok()?.filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
        .map(|entry| entry.path().join("site-packages/mlx_lm/models"))
        .find(|dir| dir.is_dir())
}
/// Whether mlx-lm implements the architecture in `model/config.json` (`model_type`, or the text model's
/// for multimodal configs). A static check: no weights are loaded.
fn lora_support(architectures: Option<&Path>, model: &Path) -> Option<bool> {
    let architectures = architectures?;
    let config = read_json(&model.join("config.json"))?;
    let kind = config.get("model_type").or_else(|| config.pointer("/text_config/model_type")).and_then(Value::as_str)?;
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { return Some(false); }
    // MLX also resolves architecture aliases (for example gemma4_unified).
    // Absence of a same-named module cannot prove incompatibility.
    architectures.join(format!("{kind}.py")).is_file().then_some(true)
}
/// LM Studio's download folder: its `downloadsFolder` setting, else `~/.lmstudio/models`.
fn lmstudio_folder() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|s| !s.is_empty())?);
    let configured = read_json(&home.join(".lmstudio/settings.json"))
        .and_then(|settings| settings["downloadsFolder"].as_str().filter(|s| !s.is_empty()).map(PathBuf::from));
    Some(configured.unwrap_or_else(|| home.join(".lmstudio/models")))
}
/// MLX models LM Studio downloaded, laid out as `<folder>/<publisher>/<model>/`. Only folders that
/// pass the same check as Hugging Face snapshots (config plus complete safetensors) are listed, so GGUF
/// downloads and LM Studio-specific bundles are skipped. Entries are labelled `publisher/model`.
fn lmstudio_mlx_models(folder: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    let sorted = |dir: &Path| -> Vec<fs::DirEntry> {
        let mut entries: Vec<_> = fs::read_dir(dir).map(|it| it.filter_map(Result::ok).take(4096).collect()).unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        entries
    };
    let mut models = Vec::new();
    for publisher in sorted(folder) {
        for repo in sorted(&publisher.path()) {
            if models.len() >= MAX_MODELS { return models; }
            let path = repo.path();
            let Some(window) = snapshot(&path) else { continue };
            let label = format!("{}/{}", publisher.file_name().to_string_lossy(), repo.file_name().to_string_lossy());
            models.push(ModelEntry { selection:ModelSelection { model:path.to_string_lossy().into_owned(), context_window:window, ..ModelSelection::default() }, label, source:"LM Studio download (MLX)".into(), available:true, metadata:ModelMetadata { lora: lora_support(trainable, &path), ..ModelMetadata::default() } });
        }
    }
    models
}
fn cached(user_dir: &Path) -> BTreeMap<String, Vec<ModelEntry>> {
    read_private(&user_dir.join("model-catalog.json")).ok().flatten().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}
fn persist_cache(user_dir: &Path, updates: BTreeMap<String, Vec<ModelEntry>>) -> Result<()> {
    if updates.is_empty() { return Ok(()); }
    let path = user_dir.join("model-catalog.json"); let _lock = locked(&path)?;
    let mut cache = cached(user_dir); cache.extend(updates);
    while cache.len() > MAX_PROVIDERS { if let Some(key) = cache.keys().next().cloned() { cache.remove(&key); } }
    write_private(&path, &serde_json::to_vec(&cache)?)
}
fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool { a.provider == b.provider && a.model == b.model && a.base_url == b.base_url }

pub fn discover(layout: &Layout, configured: &BTreeMap<String, ProviderSpec>, selections: &[ModelSelection]) -> Discovery {
    let mut discovery = Discovery::default();
    let trainable = mlx_architectures(&layout.user_dir);
    if let Some(cache) = hf_cache() { discovery.models = local_models(&cache, trainable.as_deref()); }
    if let Some(folder) = lmstudio_folder() {
        // The same repository downloaded by both keeps its Hugging Face entry.
        for entry in lmstudio_mlx_models(&folder, trainable.as_deref()) {
            if discovery.models.len() < MAX_MODELS && !discovery.models.iter().any(|known| known.label == entry.label) { discovery.models.push(entry); }
        }
    }
    discovery.providers.push(ProviderStatus { id:"mlx".into(), label:"Local MLX".into(), base_url:None, state:if discovery.models.is_empty() { "unavailable" } else { "cached" }.into(), detail:"Downloaded config and weights only; no model loading or downloads".into(), models:discovery.models.len() });
    let mut endpoints = BTreeMap::new();
    for (id, label, variable, default) in [("lm-studio", "LM Studio", "LM_STUDIO_BASE_URL", "http://127.0.0.1:1234/v1"), ("ollama", "Ollama", "OLLAMA_BASE_URL", "http://127.0.0.1:11434"), ("llama-cpp", "llama.cpp", "LLAMA_CPP_BASE_URL", "http://127.0.0.1:8080/v1")] {
        let override_url = std::env::var(variable).ok().filter(|s| !s.is_empty()).or_else(|| if id == "ollama" { std::env::var("OLLAMA_HOST").ok().filter(|s| !s.is_empty()).map(|s| if s.contains("://") { s } else { format!("http://{s}") }) } else { None });
        endpoints.insert(id.to_owned(), ProviderSpec { label:label.into(), base_url:override_url.unwrap_or_else(|| default.into()), ..ProviderSpec::default() });
    }
    for &(id, label, base, env) in KNOWN {
        endpoints.insert(id.into(), ProviderSpec { label:label.into(), base_url:base.into(), api_key_env:Some(env.into()), ..ProviderSpec::default() });
    }
    endpoints.extend(configured.iter().map(|(id, spec)| (id.clone(), spec.clone())));
    let mut endpoints: Vec<_> = endpoints.into_iter().collect();
    for selection in selections {
        if let Some(base) = &selection.base_url {
            if configured.get(&selection.provider).is_some_and(|spec| !spec.enabled) { continue; }
            if endpoints.iter().any(|(id, spec)| id == &selection.provider && &spec.base_url == base) { continue; }
            endpoints.push((selection.provider.clone(), ProviderSpec { label:selection.provider.clone(), base_url:base.clone(), api_key_env:selection.api_key_env.clone(), allow_insecure:selection.allow_insecure, ..ProviderSpec::default() }));
        }
    }
    let storage_safe = layout.ensure_state_outside_project().is_ok() && !layout.user_dir.starts_with(&layout.project_root);
    if !storage_safe {
        discovery.providers.push(ProviderStatus { id:"storage".into(), label:"Private storage".into(), base_url:None, state:"unavailable".into(), detail:"Private storage must be outside the editable workspace; endpoint discovery skipped".into(), models:0 });
    } else {
        if configured.get(super::chatgpt::PROVIDER).is_some_and(|spec| !spec.enabled) {
            discovery.providers.push(ProviderStatus { id:super::chatgpt::PROVIDER.into(),label:"ChatGPT subscription".into(),base_url:Some(super::chatgpt::BASE_URL.into()),state:"disabled".into(),detail:"Disabled in configuration".into(),models:0 });
        } else {
            let (chatgpt, models) = super::chatgpt::discover(&layout.user_dir);
            discovery.providers.push(chatgpt); discovery.models.extend(models);
        }
        let cache = cached(&layout.user_dir);
        let probes: Vec<_> = endpoints.into_iter().filter(|(id, _)| id != super::chatgpt::PROVIDER).enumerate().map(|(index, (id, spec))| {
            if index >= MAX_PROVIDERS {
                Probe { id, spec, token:None, blocked:Some(("unavailable", "Endpoint probe limit reached")) }
            } else { prepare(id, spec, &layout.user_dir) }
        }).collect();
        let mut updates = BTreeMap::new();
        // At most eight simultaneous probes, each with a total request deadline.
        for batch in probes.chunks(CONCURRENCY) {
            let results = std::thread::scope(|scope| {
                let jobs: Vec<_> = batch.iter().map(|probe| scope.spawn(move || probe_models(probe))).collect();
                jobs.into_iter().zip(batch).map(|(job, probe)| job.join().unwrap_or_else(|_| (status(probe, "unavailable", "Metadata worker failed", 0), Vec::new()))).collect::<Vec<_>>()
            });
            for ((mut status, mut models), probe) in results.into_iter().zip(batch) {
                if status.state == "reachable" { updates.insert(probe.spec.base_url.clone(), models.clone()); }
                else if status.state != "disabled" {
                    if let Some(stale) = cache.get(&probe.spec.base_url) {
                        models = stale.iter().take(MAX_MODELS).cloned().map(|mut entry| { entry.available = false; entry.source = format!("{} (cached, unverified)", probe.spec.label); entry.selection.provider = probe.id.clone(); entry.selection.api_key_env = probe.spec.api_key_env.clone(); entry.selection.allow_insecure = probe.spec.allow_insecure; entry }).collect();
                        if !models.is_empty() { status.detail.push_str("; showing cached metadata as unverified"); status.models = models.len(); }
                    }
                }
                discovery.providers.push(status); discovery.models.extend(models);
            }
        }
        if persist_cache(&layout.user_dir, updates).is_err() {
            discovery.providers.push(ProviderStatus { id:"catalog-cache".into(), label:"Metadata cache".into(), base_url:None, state:"unavailable".into(), detail:"Live metadata could not be saved privately".into(), models:0 });
        }
    }
    for selection in selections {
        if selection.model.trim().is_empty() { continue; }
        if discovery.models.iter().any(|m| same_model(&m.selection, selection)) { continue; }
        let mut selection = selection.clone();
        if selection.base_url.as_deref().is_some_and(|url| validated_url(url, true).is_err()) { selection.base_url = None; }
        let local = selection.provider == "mlx" && selection.base_url.is_none();
        let available = local && snapshot(Path::new(&selection.model)).is_some();
        let lora = if local { lora_support(trainable.as_deref(), Path::new(&selection.model)) } else { None };
        discovery.models.push(ModelEntry { label:selection.model.clone(), selection, source:if available { "Configured local model" } else { "Configured (unverified)" }.into(), available, metadata:ModelMetadata { lora, ..ModelMetadata::default() } });
    }
    discovery
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{BufRead, BufReader, Write}, net::TcpListener};
    #[test]
    fn provider_metadata_preserves_supported_prices_and_vision() {
        let info = metadata(&serde_json::json!({"architecture":{"input_modalities":["text","image"]},"pricing":{"prompt":"0.000001","completion":"0.000003"}}));
        assert_eq!(info.vision, Some(true));
        assert!((info.pricing.unwrap().cost(1000, 500).unwrap() - 0.0025).abs() < 1e-12);
        assert_eq!(metadata(&serde_json::json!({"input_modalities":["text"]})).vision, Some(false));
        assert_eq!(metadata(&serde_json::json!({})).vision, None);
        for value in [serde_json::json!({"prompt":"NaN","completion":"1"}), serde_json::json!({"prompt":-1,"completion":1}), serde_json::json!({"prompt":"1"})] {
            assert!(metadata(&serde_json::json!({"pricing":value})).pricing.is_none());
        }
    }
    fn server(code: u16, body: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap(); let address = listener.local_addr().unwrap();
        let job = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap(); stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap()); let mut request = String::new();
            loop { let mut line = String::new(); reader.read_line(&mut line).unwrap(); if line == "\r\n" || line.is_empty() { break; } request.push_str(&line); }
            write!(stream, "HTTP/1.1 {code} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap(); request
        });
        (format!("http://{address}/v1"), job)
    }
    fn test_probe(url: String, id: &str) -> Probe { Probe { id:id.into(), spec:ProviderSpec { label:id.into(), base_url:url, ..ProviderSpec::default() }, token:None, blocked:None } }
    #[test]
    fn probes_real_http_models_and_keeps_unknown_context_unknown() {
        let (url, job) = server(200, r#"{"data":[{"id":"one","context_length":12345},{"id":"two"},{"id":"one"}]}"#.into());
        let (status, models) = probe_models(&test_probe(url, "custom")); assert_eq!(status.state, "reachable");
        assert_eq!(models.iter().map(|m| (&*m.selection.model, m.selection.context_window)).collect::<Vec<_>>(), [("one", Some(12345)), ("two", None)]);
        assert!(job.join().unwrap().starts_with("GET /v1/models "));
        let (url, job) = server(200, r#"{"models":[{"name":"llama:latest"}]}"#.into());
        let (_, models) = probe_models(&test_probe(url, "ollama")); assert_eq!(models[0].selection.model, "llama:latest"); assert!(job.join().unwrap().starts_with("GET /api/tags "));
    }
    #[test]
    fn auth_redirect_invalid_and_oversized_responses_are_explicit_failures() {
        for (code, body, expected) in [(401, "secret must not appear".into(), "auth-required"), (302, "redirect".into(), "unavailable"), (200, "invalid".into(), "unavailable"), (200, "x".repeat(MAX_RESPONSE as usize + 1), "unavailable")] {
            let (url, job) = server(code, body); let (status, models) = probe_models(&test_probe(url, "custom"));
            assert_eq!(status.state, expected); assert!(models.is_empty()); assert!(!status.detail.contains("secret")); job.join().unwrap();
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
        assert!(request.contains("x-api-key: test-secret") && request.contains("anthropic-version: 2023-06-01"));
        assert!(!request.contains("authorization:"));
        assert_eq!(credential_token(home.path(), &url).unwrap().as_deref(), Some("test-secret"));

        let rejected_home = tempfile::tempdir().unwrap();
        let (url, job) = server(401, r#"{"error":"invalid key"}"#.into());
        let mut key = "bad-secret".to_owned();
        let error = verify_and_save_credential(rejected_home.path(), "openai", &url, &mut key).unwrap_err().to_string();
        let request = job.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer bad-secret"));
        assert!(error.contains("rejected") && !error.contains("bad-secret"));
        assert!(key.is_empty());
        assert_eq!(credential_token(rejected_home.path(), &url).unwrap(), None, "rejected keys are never persisted");
    }
    #[test]
    fn openrouter_key_validation_precedes_persistence() {
        let home = tempfile::tempdir().unwrap();
        let (url, job) = server(200, r#"{"data":{"label":"Dial"}}"#.into());
        let validate = url.replace("/v1", "/api/v1/auth/key");
        let mut key = "sk-or-valid".to_owned();
        verify_and_save_credential_at(home.path(), "openrouter", OPENROUTER_BASE_URL, &mut key, &validate).unwrap();
        assert!(key.is_empty());
        let request = job.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/v1/auth/key "));
        assert!(request.contains("authorization: bearer sk-or-valid"));
        assert_eq!(credential_token(home.path(), OPENROUTER_BASE_URL).unwrap().as_deref(), Some("sk-or-valid"));

        let rejected_home = tempfile::tempdir().unwrap();
        let (url, job) = server(401, r#"{"error":"no"}"#.into());
        let mut key = "sk-or-invalid".to_owned();
        let validate = url.replace("/v1", "/api/v1/auth/key");
        let error = verify_and_save_credential_at(rejected_home.path(), "openrouter", OPENROUTER_BASE_URL, &mut key, &validate).unwrap_err().to_string();
        assert!(error.contains("rejected") && !error.contains("sk-or-invalid"));
        assert!(key.is_empty());
        job.join().unwrap();
        assert_eq!(credential_token(rejected_home.path(), OPENROUTER_BASE_URL).unwrap(), None);
    }
    #[test]
    fn key_verification_rejects_unreachable_metadata_without_persisting() {
        let home = tempfile::tempdir().unwrap();
        let (url, job) = server(500, r#"{"error":"upstream"}"#.into());
        let mut key = "test-secret".to_owned();
        let error = verify_and_save_credential(home.path(), "anthropic", &url, &mut key).unwrap_err().to_string();
        assert!(error.contains("HTTP 500") && !error.contains("test-secret"));
        assert!(key.is_empty(), "the caller's key buffer is cleared whether verification succeeds or fails");
        job.join().unwrap();
        assert_eq!(credential_token(home.path(), &url).unwrap(), None);
    }
    #[test]
    fn credentials_are_url_scoped_private_and_preserve_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        save_credential(dir.path(), "https://first.test/v1", "first-secret").unwrap();
        save_credential(dir.path(), "https://second.test/v1", "second-secret").unwrap();
        remove_credential(dir.path(), "https://first.test/v1").unwrap();
        assert_eq!(credential_token(dir.path(), "https://first.test/v1").unwrap(), None);
        assert_eq!(credential_token(dir.path(), "https://second.test/v1").unwrap().as_deref(), Some("second-secret"));
        assert_eq!(credential_token(dir.path(), "https://second.test/other").unwrap(), None);
        for url in ["https://user:secret@first.test/v1", "https://first.test/v1?key=secret", "https://first.test/v1#secret"] { assert!(save_credential(dir.path(), url, "ok").is_err()); }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(dir.path().join("credentials.json")).unwrap().permissions().mode() & 0o777, 0o600);
        let uri = "https://unrelated.test/v1".parse().unwrap(); assert!(env_token("OPENAI_API_KEY", &uri).is_err());
    }
    #[test]
    fn hf_main_revision_wins_and_incomplete_snapshots_are_ignored() {
        let cache = tempfile::tempdir().unwrap(); let repo = cache.path().join("models--mlx-community--example");
        fs::create_dir_all(repo.join("refs")).unwrap(); fs::write(repo.join("refs/main"), "current\n").unwrap();
        for name in ["old", "current"] { let path = repo.join("snapshots").join(name); fs::create_dir_all(&path).unwrap(); fs::write(path.join("config.json"), r#"{"max_position_embeddings":8192}"#).unwrap(); fs::write(path.join("model.safetensors"), "weights").unwrap(); }
        let entries = local_models(cache.path(), None); assert_eq!(entries.len(), 1); assert!(entries[0].selection.model.ends_with("/current")); assert_eq!(entries[0].selection.context_window, Some(8192));
        fs::write(repo.join("snapshots/current/model.safetensors.index.json"), r#"{"weight_map":{"w":"missing.safetensors"}}"#).unwrap();
        assert!(local_models(cache.path(), None)[0].selection.model.ends_with("/old"));
    }
    #[test]
    fn lm_studio_mlx_downloads_are_listed_and_other_formats_skipped() {
        let folder = tempfile::tempdir().unwrap();
        let mlx = folder.path().join("lmstudio-community/Qwen-MLX-8bit");
        fs::create_dir_all(&mlx).unwrap();
        fs::write(mlx.join("config.json"), r#"{"model_type":"qwen3_5","text_config":{"max_position_embeddings":32768}}"#).unwrap();
        let gemma = folder.path().join("lmstudio-community/gemma-MLX");
        fs::create_dir_all(&gemma).unwrap();
        fs::write(gemma.join("config.json"), r#"{"text_config":{"model_type":"gemma4_unified"}}"#).unwrap();
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
        assert_eq!(entries.iter().map(|e| (e.label.as_str(), e.metadata.lora)).collect::<Vec<_>>(),
            [("lmstudio-community/Qwen-MLX-8bit", Some(true)), ("lmstudio-community/gemma-MLX", None)]);
        assert_eq!(lmstudio_mlx_models(folder.path(), None)[0].metadata.lora, None, "unknown without an mlx-lm install");
        assert_eq!(entries[0].selection.model, mlx.to_string_lossy());
        assert_eq!(entries[0].selection.context_window, Some(32768));
        assert!(entries[0].selection.base_url.is_none(), "served by dial's MLX server, not LM Studio");
    }
    #[test]
    fn disabled_endpoint_is_never_contacted() {
        let dir = tempfile::tempdir().unwrap(); let probe = prepare("custom".into(), ProviderSpec { base_url:"http://127.0.0.1:1".into(), enabled:false, ..ProviderSpec::default() }, dir.path());
        let (status, models) = probe_models(&probe); assert_eq!(status.state, "disabled"); assert!(models.is_empty());
    }
    #[test]
    fn failed_live_probe_retains_cached_and_configured_models_as_unverified() {
        let home = tempfile::tempdir().unwrap(); let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let (url, job) = server(401, "{}".into());
        let selection = ModelSelection { provider:"custom".into(), model:"saved".into(), base_url:Some(url.clone()), ..ModelSelection::default() };
        persist_cache(&layout.user_dir, BTreeMap::from([(url.clone(), vec![ModelEntry { selection:selection.clone(), label:"saved".into(), source:"prior live probe".into(), available:true, metadata:ModelMetadata::default() }])])).unwrap();
        let mut configured = BTreeMap::new();
        for id in ["lm-studio", "ollama", "llama-cpp"].into_iter().chain(KNOWN.iter().map(|row| row.0)) {
            configured.insert(id.into(), ProviderSpec { enabled:false, ..ProviderSpec::default() });
        }
        configured.insert("custom".into(), ProviderSpec { base_url:url, ..ProviderSpec::default() });
        let mut another = selection.clone(); another.model = "not-in-cache".into();
        let result = discover(&layout, &configured, &[selection, another]);
        assert_eq!(result.providers.iter().find(|s| s.id == "custom").unwrap().state, "auth-required");
        let models: Vec<_> = result.models.iter().filter(|m| m.selection.provider == "custom").collect();
        assert_eq!(models.len(), 2); assert!(models.iter().all(|m| !m.available));
        assert!(models.iter().any(|m| m.selection.model == "saved" && m.source.contains("cached")));
        assert!(models.iter().any(|m| m.selection.model == "not-in-cache"));
        job.join().unwrap();
    }
    #[test]
    fn simultaneous_credential_saves_do_not_lose_other_urls() {
        let home = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for n in 0..8 { let path = home.path(); scope.spawn(move || save_credential(path, &format!("https://provider{n}.test/v1"), "private-key").unwrap()); }
        });
        for n in 0..8 { assert_eq!(credential_token(home.path(), &format!("https://provider{n}.test/v1")).unwrap().as_deref(), Some("private-key")); }
    }
    #[test]
    fn metadata_caps_models_and_rejects_invalid_context_values() {
        let probe = test_probe("http://127.0.0.1:1234/v1".into(), "custom");
        let rows: Vec<_> = (0..MAX_MODELS + 20).map(|i| serde_json::json!({"id":format!("model-{i}"), "context_length":if i == 0 { u64::MAX } else { 0 }})).collect();
        let models = parse_models(&serde_json::json!({"data":rows}), &probe).unwrap();
        assert_eq!(models.len(), MAX_MODELS);
        assert!(models.iter().all(|entry| entry.selection.context_window.is_none()));
        assert!(models.iter().all(|entry| entry.selection.model != format!("model-{MAX_MODELS}")));
    }
    #[test]
    fn malformed_credentials_fail_closed_without_echoing_content() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("credentials.json"), "secret-token malformed").unwrap();
        let error = save_credential(home.path(), "https://provider.test/v1", "new-token").unwrap_err();
        assert!(!format!("{error:#}").contains("secret-token"));
        assert_eq!(fs::read_to_string(home.path().join("credentials.json")).unwrap(), "secret-token malformed");
    }
    /// Answers any number of requests by path; unknown paths get 404 like a server without the native API.
    fn routed(routes: Vec<(&'static str, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap(); let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
                let mut reader = BufReader::new(stream.try_clone().unwrap()); let mut first = String::new();
                reader.read_line(&mut first).ok();
                loop { let mut line = String::new(); if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" { break; } }
                let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (code, body) = routes.iter().find(|(route, _)| *route == path).map(|(_, body)| (200, body.clone())).unwrap_or((404, "{}".into()));
                let _ = write!(stream, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
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
    fn lm_studio_discovery_hides_embedders_and_reports_only_the_loaded_context() {
        let url = routed(lm_studio_routes());
        let (status, models) = probe_models(&test_probe(url, "lm-studio"));
        assert_eq!(status.state, "reachable");
        let by = |id: &str| models.iter().find(|m| m.selection.model == id).unwrap();
        assert_eq!(by("warm").selection.context_window, Some(8192), "loaded size is the budget");
        assert!(by("warm").source.contains("loaded") && by("warm").available);
        assert_eq!(by("cold").selection.context_window, None, "an unloaded model's maximum must never become its window");
        assert!(by("cold").source.contains("loads on first use") && by("cold").available);
        assert!(!by("embedder").available && by("embedder").source.contains("embedding"), "embedders cannot chat");
        assert!(models.iter().all(|m| m.selection.context_window != Some(1_048_576) && m.selection.context_window != Some(262_144)));
    }
    #[test]
    fn servers_without_the_native_api_are_listed_unchanged() {
        let url = routed(vec![("/v1/models", r#"{"data":[{"id":"a"},{"id":"b"}]}"#.into())]);
        let (status, models) = probe_models(&test_probe(url, "llama-cpp"));
        assert_eq!(status.state, "reachable");
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m.available && m.selection.context_window.is_none() && m.source == "llama-cpp"));
    }
    #[test]
    fn loaded_context_is_exact_local_only_and_rejects_hostile_model_names() {
        let url = routed(lm_studio_routes());
        assert_eq!(loaded_context(&url, "warm", None), Some(8192));
        assert_eq!(loaded_context(&url, "cold", None), None, "not loaded");
        assert_eq!(loaded_context(&url, "missing", None), None);
        for hostile in ["warm?x=1", "warm#frag", "warm%2e", "", "a\nb"] {
            assert_eq!(loaded_context(&url, hostile, None), None, "{hostile:?}");
        }
        // A hosted API is never sent LM Studio probes, and the answer cannot come from the network.
        assert_eq!(loaded_context("https://api.example.com/v1", "warm", None), None);
    }
    #[test]
    fn llama_cpp_and_ollama_report_the_context_they_actually_serve() {
        // llama.cpp advertises its training context in /v1/models; the served per-slot size is in /props.
        let llama = routed(vec![
            ("/v1/models", r#"{"data":[{"id":"qwen","meta":{"n_ctx_train":131072}}]}"#.into()),
            ("/props", r#"{"default_generation_settings":{"n_ctx":4096}}"#.into()),
        ]);
        assert_eq!(loaded_context(&llama, "qwen", None), Some(4096));
        let ollama = routed(vec![("/api/ps", r#"{"models":[{"name":"qwen2.5:7b","model":"qwen2.5:7b","context_length":4096},{"name":"llama3:latest","model":"llama3:latest","context_length":8192}]}"#.into())]);
        assert_eq!(loaded_context(&ollama, "qwen2.5:7b", None), Some(4096));
        assert_eq!(loaded_context(&ollama, "llama3", None), Some(8192), ":latest is Ollama's implicit tag");
        assert_eq!(loaded_context(&ollama, "mistral", None), None, "not loaded yet");
        assert_eq!(loaded_context(&routed(vec![]), "anything", None), None, "a server with none of the native APIs");
    }
}
