//! Local inference servers (LM Studio, llama.cpp, Ollama, OpenAI-compatible runtimes): probes and metadata.

use super::credentials::validated_url;
use super::metadata::{ModelEntry, ModelKind, ModelMetadata};
use super::{CONCURRENCY, ENRICHMENT_BUDGET, MAX_ENRICHMENTS, MAX_MODELS, MAX_RESPONSE};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{Duration, Instant};

/// What LM Studio's native REST API knows that its OpenAI-compatible model list omits: whether a
/// model is an embedder, whether it declares native tool use, and whether it is loaded and with how
/// much context. A model that is not loaded gets loaded on first use, using whatever context LM
/// Studio picks; that, not the model's maximum, is the budget Dial has to live within.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmStudioModel {
    pub id: String,
    pub kind: String,
    /// `capabilities` lists `tool_use`. Absence is not a refusal: LM Studio still prompts tools to
    /// models without native support.
    pub tool_use: bool,
    pub loaded: bool,
    pub loaded_context: Option<u32>,
    /// Explicit v1 capability flags; absent or malformed fields remain unknown.
    pub vision: Option<bool>,
    pub native_tools: Option<bool>,
    /// Public reasoning controls advertised by v1, never inferred from a model name.
    pub reasoning_options: Vec<String>,
    pub reasoning_default: Option<String>,
}
pub(super) fn parse_lmstudio(value: &Value) -> Vec<LmStudioModel> {
    if let Some(rows) = value.get("models").and_then(Value::as_array) {
        let mut found = Vec::new();
        for row in rows.iter().take(MAX_MODELS) {
            if found.len() >= MAX_MODELS {
                break;
            }
            let Some(key) = row["key"].as_str().filter(|id| valid_model_id(id)) else {
                continue;
            };
            let instances: Vec<_> = row["loaded_instances"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|instance| instance["id"].as_str().is_some_and(valid_model_id))
                .take(MAX_MODELS.saturating_sub(found.len()))
                .collect();
            let context = |instance: &Value| {
                instance
                    .pointer("/config/context_length")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n > 0)
            };
            // Requests using the key may reach any instance. Unknown capacity on any one of
            // them must not be replaced with another instance's (or the trained) maximum.
            let loaded_context = instances
                .iter()
                .map(|instance| context(instance))
                .collect::<Option<Vec<_>>>()
                .and_then(|contexts| contexts.into_iter().min());
            let reasoning_options: Vec<String> = row
                .pointer("/capabilities/reasoning/allowed_options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|option| matches!(*option, "off" | "on" | "low" | "medium" | "high"))
                .map(str::to_owned)
                .collect();
            let reasoning_default = row
                .pointer("/capabilities/reasoning/default")
                .and_then(Value::as_str)
                .filter(|default| reasoning_options.iter().any(|option| option == default))
                .map(str::to_owned);
            let model = LmStudioModel {
                id: key.to_owned(),
                kind: row["type"].as_str().unwrap_or("unknown").to_owned(),
                tool_use: row
                    .pointer("/capabilities/trained_for_tool_use")
                    .and_then(Value::as_bool)
                    == Some(true),
                native_tools: row
                    .pointer("/capabilities/trained_for_tool_use")
                    .and_then(Value::as_bool),
                vision: row.pointer("/capabilities/vision").and_then(Value::as_bool),
                loaded: !instances.is_empty(),
                loaded_context,
                reasoning_options,
                reasoning_default,
            };
            // Exact instance identifiers take precedence over their key's aggregate metadata.
            for instance in &instances {
                let mut alias = model.clone();
                alias.id = instance["id"].as_str().unwrap_or_default().to_owned();
                alias.loaded_context = context(instance);
                found.push(alias);
            }
            if !instances.iter().any(|instance| instance["id"] == key) {
                found.push(model);
            }
        }
        found.truncate(MAX_MODELS);
        return found;
    }
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
                        tool_use: row
                            .get("capabilities")
                            .and_then(Value::as_array)
                            .is_some_and(|caps| caps.iter().any(|c| c == "tool_use")),
                        loaded,
                        loaded_context,
                        vision: match row["type"].as_str() {
                            Some("llm") => Some(false),
                            Some("vlm") => Some(true),
                            _ => None,
                        },
                        native_tools: row["capabilities"]
                            .as_array()
                            .map(|caps| caps.iter().any(|c| c == "tool_use")),
                        reasoning_options: Vec::new(),
                        reasoning_default: None,
                    })
                })
                .take(MAX_MODELS)
                .collect()
        })
        .unwrap_or_default()
}
/// A native route a local server answered 404 for is not asked again for this long. The answer
/// says the server has no such API (an OpenAI-compatible server is not llama.cpp, Ollama or LM
/// Studio), and the probes run on every request; the bound lets a server replaced on the same port,
/// or a model downloaded later, be recognised without restarting Dial.
const ABSENT_ROUTE_TTL: Duration = Duration::from_secs(300);
/// GET URLs (`origin` + path) that answered 404, with when. Transport failures are not stored, so
/// a server that is still starting is probed again.
static ABSENT_ROUTES: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);
fn route_known_absent(url: &str) -> bool {
    let Ok(mut absent) = ABSENT_ROUTES.lock() else {
        return false;
    };
    match absent.get(url) {
        Some(at) if at.elapsed() < ABSENT_ROUTE_TTL => true,
        Some(_) => {
            absent.remove(url);
            false
        }
        None => false,
    }
}
/// GET a native (non-OpenAI) endpoint of a local server. Only ever asked of loopback/private hosts; a
/// hosted API is never sent these probes. A route that answered 404 is not asked again for
/// [`ABSENT_ROUTE_TTL`].
pub(crate) fn local_get(
    base_url: &str,
    path: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) -> Option<Value> {
    local_request(base_url, path, None, token, headers)
}
/// [`local_get`], or a JSON POST when `body` is set (Ollama's `/api/show` is POST-only). Only GET
/// answers are remembered as absent: a POST 404 can be about its body (an unknown model).
pub(crate) fn local_request(
    base_url: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) -> Option<Value> {
    local_request_remembering(base_url, path, body, token, headers, body.is_none())
}
/// [`local_request`]; `remember_404` also records a POST 404 as an absent route, for callers that
/// already know the server is not the one whose POST 404 can mean an unknown model.
fn local_request_remembering(
    base_url: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
    remember_404: bool,
) -> Option<Value> {
    local_request_outcome(base_url, path, body, token, headers, remember_404).ok()
}

// Preserve the HTTP status for native API version negotiation. None is transport/invalid data.
fn local_request_outcome(
    base_url: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
    remember_404: bool,
) -> std::result::Result<Value, Option<u16>> {
    let uri = validated_url(base_url, true).map_err(|_| None)?;
    if !crate::llm::is_local_host(uri.host().ok_or(None)?) {
        return Err(None);
    }
    let origin = format!(
        "{}://{}",
        uri.scheme_str().ok_or(None)?,
        uri.authority().ok_or(None)?
    );
    let agent = crate::llm::agent_with(1, false, None, None);
    let url = format!("{origin}{path}");
    if remember_404 && route_known_absent(&url) {
        return Err(Some(404));
    }
    let mut response = match body {
        None => {
            let mut request = agent.get(&url);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            for (name, value) in headers.pairs() {
                request = request.header(
                    name,
                    crate::llm::model_config::private_header_value(value).map_err(|_| None)?,
                );
            }
            request.call().map_err(|_| None)?
        }
        Some(body) => {
            let mut request = agent.post(&url);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            for (name, value) in headers.pairs() {
                request = request.header(
                    name,
                    crate::llm::model_config::private_header_value(value).map_err(|_| None)?,
                );
            }
            request.send_json(body).map_err(|_| None)?
        }
    };
    if remember_404 && response.status() == 404 {
        if let Ok(mut absent) = ABSENT_ROUTES.lock() {
            absent.insert(url, Instant::now());
        }
        return Err(Some(404));
    }
    if !response.status().is_success() {
        return Err(Some(response.status().as_u16()));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| None)?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err(None);
    }
    serde_json::from_slice(&bytes).map_err(|_| None)
}
/// A model name that is safe in a URL path segment or query value: non-empty, short, with no control
/// characters or URL syntax (`?`, `#`, `%`).
pub(crate) fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 512
        && !model
            .chars()
            .any(|c| c.is_control() || c == '?' || c == '#' || c == '%')
}
pub(crate) fn lmstudio_models_with_headers(
    base_url: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) -> Option<Vec<LmStudioModel>> {
    lmstudio_metadata(base_url, None, token, headers)
}
pub(crate) fn lmstudio_model_with_headers(
    base_url: &str,
    model: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) -> Option<LmStudioModel> {
    lmstudio_metadata(base_url, Some(model), token, headers)?
        .into_iter()
        .find(|known| known.id == model)
}
fn lmstudio_metadata(
    base_url: &str,
    model: Option<&str>,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) -> Option<Vec<LmStudioModel>> {
    let value = match local_request_outcome(base_url, "/api/v1/models", None, token, headers, true)
    {
        Ok(value) => value,
        Err(Some(404 | 405 | 501)) => {
            if let Some(model) = model.filter(|model| valid_model_id(model) && !model.contains('/'))
            {
                let row = local_get(base_url, &format!("/api/v0/models/{model}"), token, headers)?;
                serde_json::json!({"data": [row]})
            } else {
                local_get(base_url, "/api/v0/models", token, headers)?
            }
        }
        // Authentication, transport and malformed replies do not mean an older server.
        Err(_) => return None,
    };
    if !value["models"].is_array() && !value["data"].is_array() {
        return None;
    }
    Some(parse_lmstudio(&value))
}
/// What a llama.cpp `llama-server` reports about itself in `/props`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LlamaProps {
    /// `chat_template_caps.supports_tools && supports_tool_calls`: the model's chat template
    /// natively renders tool definitions and tool calls. `None` when the server reports no caps
    /// (older builds).
    pub native_tool_template: Option<bool>,
}
impl LlamaProps {
    /// The facts in a `/props` document, or `None` when it lacks llama.cpp's
    /// `default_generation_settings`.
    pub fn parse(props: &Value) -> Option<Self> {
        props.get("default_generation_settings")?;
        let caps = &props["chat_template_caps"];
        let cap = |name: &str| caps.get(name).and_then(Value::as_bool);
        let native_tool_template = match (cap("supports_tools"), cap("supports_tool_calls")) {
            (Some(tools), Some(calls)) => Some(tools && calls),
            (Some(false), None) | (None, Some(false)) => Some(false),
            _ => None,
        };
        Some(Self {
            native_tool_template,
        })
    }
}
/// `/props` answers per base URL: `Some` for llama.cpp, `None` for a server that answered as
/// something else. Transport failures are not stored.
static LLAMA_PROPS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Option<LlamaProps>>>> =
    std::sync::LazyLock::new(Default::default);
fn llama_props_key(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_owned()
}
/// Forgets the `/props` answers and absent native routes cached for every base URL on `origin`
/// (`scheme://host:port`): a test server bound to a port an earlier one used is a different server.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn forget_llama_props(origin: &str) {
    if let Ok(mut known) = LLAMA_PROPS.lock() {
        known.retain(|key, _| !crate::llm::same_origin(key, origin));
    }
    if let Ok(mut known) = OPENAI_RUNTIME.lock() {
        known.retain(|key, _| !crate::llm::same_origin(key, origin));
    }
    if let Ok(mut absent) = ABSENT_ROUTES.lock() {
        absent.retain(|url, _| !crate::llm::same_origin(url, origin));
    }
}
/// Records a `/props` document fetched elsewhere (`local_server::facts`), so later lookups reuse it.
pub(crate) fn record_llama_props(base_url: &str, props: &Value) {
    if let Some(found) = LlamaProps::parse(props)
        && let Ok(mut known) = LLAMA_PROPS.lock()
    {
        known.insert(llama_props_key(base_url), Some(found));
    }
}
/// The `/props` facts already known for `base_url`, without contacting it.
pub fn known_llama_props(base_url: &str) -> Option<LlamaProps> {
    LLAMA_PROPS
        .lock()
        .ok()?
        .get(&llama_props_key(base_url))
        .copied()
        .flatten()
}
/// The llama.cpp facts of the server at `base_url`, recognised by the `default_generation_settings`
/// of its native `/props` document. Only loopback/private hosts are asked (`local_request` refuses
/// others). An answer is cached per base URL; a transport failure is not, so a server that is still
/// starting is probed again on the next request.
pub fn llama_server_props(
    base_url: &str,
    token: Option<&str>,
    headers: &[crate::llm::model_config::RequestHeader],
) -> Option<LlamaProps> {
    let key = llama_props_key(base_url);
    if let Some(known) = LLAMA_PROPS
        .lock()
        .ok()
        .and_then(|known| known.get(&key).copied())
    {
        return known;
    }
    let resolved = crate::llm::model_config::resolve_headers(headers).ok()?;
    let props = local_get(base_url, "/props", token, &resolved)?;
    let found = LlamaProps::parse(&props);
    if let Ok(mut known) = LLAMA_PROPS.lock() {
        known.insert(key, found);
    }
    found
}
/// Whether the OpenAI-compatible server at `base_url` is llama.cpp's `llama-server`
/// ([`llama_server_props`]).
pub fn is_llama_server(
    base_url: &str,
    token: Option<&str>,
    headers: &[crate::llm::model_config::RequestHeader],
) -> bool {
    llama_server_props(base_url, token, headers).is_some()
}
/// A server that implements the OpenAI API with vLLM's extensions and says so in its model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiRuntime {
    Vllm,
    Sglang,
}
impl OpenAiRuntime {
    /// The runtime a `/v1/models` document names. Both servers set `owned_by` to their own name on
    /// every row (`"vllm"`, `"sglang"`); this comes from their documented examples, not from a live
    /// server, so a server that words it differently is simply not recognised.
    fn parse(models: &Value) -> Option<Self> {
        models.get("data")?.as_array()?.iter().find_map(|row| {
            match row.get("owned_by")?.as_str()?.to_ascii_lowercase().as_str() {
                "vllm" => Some(Self::Vllm),
                "sglang" => Some(Self::Sglang),
                _ => None,
            }
        })
    }
}
/// `/v1/models` answers per base URL: `Some` for a recognised runtime, `None` for a server that
/// answered as something else. Failures are not stored.
static OPENAI_RUNTIME: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<String, Option<OpenAiRuntime>>>,
> = std::sync::LazyLock::new(Default::default);
/// The vLLM or SGLang runtime behind the loopback/private server at `base_url`, recognised from its
/// `/v1/models` list. Hosted hosts are never asked (`local_request` refuses them). An answer is
/// cached per base URL.
pub fn openai_runtime(
    base_url: &str,
    token: Option<&str>,
    headers: &[crate::llm::model_config::RequestHeader],
) -> Option<OpenAiRuntime> {
    let key = llama_props_key(base_url);
    // A server that already answered llama.cpp's `/props` (as llama.cpp or as something else) is
    // not asked again: vLLM and SGLang have no such route, so the extra request only costs.
    if LLAMA_PROPS
        .lock()
        .ok()
        .is_some_and(|known| known.contains_key(&key))
    {
        return None;
    }
    if let Some(known) = OPENAI_RUNTIME
        .lock()
        .ok()
        .and_then(|known| known.get(&key).copied())
    {
        return known;
    }
    let resolved = crate::llm::model_config::resolve_headers(headers).ok()?;
    let models = local_get(base_url, "/v1/models", token, &resolved)?;
    let found = OpenAiRuntime::parse(&models);
    if let Ok(mut known) = OPENAI_RUNTIME.lock() {
        known.insert(key, found);
    }
    found
}
/// The window vLLM or SGLang serves `model` with: the `max_model_len` of its `/v1/models` row. The
/// server rejects longer prompts, so unlike a configured window this is exact. Unverified against
/// a live server: a row without the field yields `None`.
fn served_model_len(models: &Value, model: &str) -> Option<u32> {
    let rows = models.get("data")?.as_array()?;
    let row = rows
        .iter()
        .find(|row| row["id"] == model)
        .or_else(|| (rows.len() == 1).then(|| &rows[0]))?;
    row["max_model_len"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}
/// Context window the local server is serving `model` with right now: LM Studio's loaded size,
/// llama.cpp's per-slot `n_ctx` (`-c` divided by `--parallel`), or the `context_length` Ollama loaded
/// the model with (its default is far below Dial's, and it truncates silently rather than failing).
/// Servers without a fixed served size that still answer Ollama's `/api/show` (Osaurus) are bounded
/// by the model's trained context. `None` for hosted APIs, other servers, or a model that is not
/// loaded yet.
pub fn loaded_context(
    base_url: &str,
    model: &str,
    token: Option<&str>,
    headers: &[crate::llm::model_config::RequestHeader],
) -> Option<u32> {
    let resolved = crate::llm::model_config::resolve_headers(headers).ok()?;
    if !valid_model_id(model) {
        return None;
    }
    let positive = |value: Option<&Value>| {
        value?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    if let Some(found) = lmstudio_model_with_headers(base_url, model, token, &resolved) {
        return found.loaded_context;
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
        return trained_context(base_url, model, token, &resolved, false);
    }
    // vLLM and SGLang: the model list carries the length the server accepts.
    local_get(base_url, "/v1/models", token, &resolved)
        .and_then(|models| served_model_len(&models, model))
        // OpenAI-compatible servers that also answer `/api/show` without `/api/ps` (Osaurus). With
        // no `/api/ps` this is not Ollama, so a 404 means the route is absent, not the model.
        .or_else(|| trained_context(base_url, model, token, &resolved, true))
}
/// The `*.context_length` a server's Ollama-style `/api/show` reports in `model_info`.
fn trained_context(
    base_url: &str,
    model: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
    remember_404: bool,
) -> Option<u32> {
    let shown = local_request_remembering(
        base_url,
        "/api/show",
        Some(&serde_json::json!({ "model": model })),
        token,
        headers,
        remember_404,
    )?;
    shown_context(&shown)
}
/// Trained context declared by an Ollama-compatible metadata response.
fn shown_context(shown: &Value) -> Option<u32> {
    shown["model_info"].as_object().and_then(|info| {
        info.iter()
            .find(|(key, _)| key.ends_with(".context_length"))
            .and_then(|(_, value)| value.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    })
}
/// Explicit purpose tokens a row declares (`type`, `pipeline_tag`, `task`). Matching is exact: an
/// unfamiliar or differently spelled value stays `Unknown` rather than being guessed from its words.
pub(super) fn kind_from_declared(token: &str) -> ModelKind {
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
pub(super) fn declared_kind(row: &Value) -> ModelKind {
    if row["owned_by"] == "mlx-serve"
        && let Some(caps) = row["capabilities"].as_array()
    {
        if caps.iter().any(|cap| cap == "image") {
            return ModelKind::Image;
        }
        if caps.iter().any(|cap| cap == "embeddings") {
            return ModelKind::Embedding;
        }
        if caps.iter().any(|cap| cap == "chat") {
            return ModelKind::Chat;
        }
        if caps.iter().any(|cap| cap == "audio") {
            return ModelKind::Audio;
        }
    }
    ["type", "pipeline_tag", "task"]
        .iter()
        .filter_map(|key| row.get(*key).and_then(Value::as_str))
        .map(kind_from_declared)
        .find(|kind| *kind != ModelKind::Unknown)
        .unwrap_or_default()
}
/// Ollama and Osaurus expose purpose and capabilities through `/api/show`. Optional enrichment
/// uses the probe's agent, headers and key with bounded concurrency, deadline and request count.
/// Osaurus also exposes its trained context here; Ollama's served context is resolved separately.
/// Failures leave the basic listing available with unknown metadata.
pub(super) fn annotate_show_metadata(
    models: &mut [ModelEntry],
    agent: &ureq::Agent,
    base: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
) {
    let deadline = Instant::now() + ENRICHMENT_BUDGET;
    let pending: Vec<(usize, String)> = models
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.metadata.kind == ModelKind::Unknown
                || (entry.selection.provider == "osaurus"
                    && (entry.selection.context_window.is_none()
                        || entry.metadata.vision.is_none()
                        || entry.metadata.tools.is_none()
                        || entry.metadata.reasoning.is_none()))
        })
        .take(MAX_ENRICHMENTS)
        .map(|(index, entry)| (index, entry.selection.model.clone()))
        .collect();
    for chunk in pending.chunks(CONCURRENCY) {
        if Instant::now() >= deadline {
            break;
        }
        let shown: Vec<(usize, Value)> = std::thread::scope(|scope| {
            let jobs: Vec<_> = chunk
                .iter()
                .map(|(index, model)| {
                    scope.spawn(move || {
                        (
                            *index,
                            ollama_show(agent, base, model, token, headers, deadline),
                        )
                    })
                })
                .collect();
            jobs.into_iter()
                .filter_map(|job| job.join().ok())
                .filter_map(|(index, shown)| Some((index, shown?)))
                .collect()
        });
        for (index, shown) in shown {
            let entry = &mut models[index];
            apply_ollama_capabilities(&mut entry.metadata, &shown);
            if entry.selection.provider == "osaurus" {
                entry.selection.context_window = entry
                    .selection
                    .context_window
                    .or_else(|| shown_context(&shown));
            }
        }
    }
}
pub(super) fn ollama_show(
    agent: &ureq::Agent,
    base: &str,
    model: &str,
    token: Option<&str>,
    headers: &crate::llm::model_config::ResolvedHeaders<'_>,
    deadline: Instant,
) -> Option<Value> {
    let mut request = agent
        .post(format!("{base}/api/show"))
        .header("Accept", "application/json");
    for (name, value) in headers.pairs() {
        let field = crate::llm::model_config::private_header_value(value).ok()?;
        request = request.header(name, field);
    }
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let request = request.config().timeout_global(Some(remaining)).build();
    let mut response = request
        .send_json(serde_json::json!({ "model": model }))
        .ok()?;
    if !response.status().is_success() {
        return None;
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
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
/// Purpose and capabilities from Ollama's `/api/show`. Its `capabilities` list is the server's own
/// declaration (Ollama refuses tools or images for a model that lacks them), so a listed name is
/// `Some(true)` and an absent one `Some(false)`; without a list everything stays unknown.
fn apply_ollama_capabilities(metadata: &mut ModelMetadata, shown: &Value) {
    if metadata.kind == ModelKind::Unknown {
        metadata.kind = ollama_capability_kind(shown);
    }
    let Some(capabilities) = shown["capabilities"].as_array() else {
        return;
    };
    let has = |name: &str| Some(capabilities.iter().any(|c| c == name));
    metadata.vision = metadata.vision.or(has("vision"));
    metadata.tools = metadata.tools.or(has("tools"));
    metadata.reasoning = metadata.reasoning.or(has("thinking"));
}
/// An `embedding` capability without `completion` is an embedder; `completion` is chat.
fn ollama_capability_kind(shown: &Value) -> ModelKind {
    let capabilities = shown["capabilities"].as_array();
    let has = |name: &str| capabilities.is_some_and(|all| all.iter().any(|c| c == name));
    if has("embedding") && !has("completion") {
        ModelKind::Embedding
    } else if has("completion") {
        ModelKind::Chat
    } else {
        ModelKind::Unknown
    }
}
/// LM Studio's native `type` is the purpose source for its served models. A served embedder keeps
/// its availability: it is selectable for embedding roles and excluded from chat by its kind.
pub(super) fn annotate_lmstudio(models: &mut [ModelEntry], info: &[LmStudioModel]) {
    for entry in models {
        let Some(known) = info.iter().find(|m| m.id == entry.selection.model) else {
            continue;
        };
        if entry.metadata.kind == ModelKind::Unknown {
            entry.metadata.kind = kind_from_declared(&known.kind);
        }
        entry.metadata.vision = entry.metadata.vision.or(known.vision);
        if entry.metadata.tools.is_none() && known.tool_use {
            // Not trained for tools is not a refusal: LM Studio also prompts tools to those models.
            entry.metadata.tools = Some(true);
        }
        if !known.reasoning_options.is_empty() {
            entry.metadata.reasoning =
                Some(known.reasoning_options.iter().any(|option| option != "off"));
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
