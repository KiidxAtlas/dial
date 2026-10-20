//! Facts about the local server and model behind a route, read from the servers' native APIs and
//! the model's own files: the server kind, its slot count, whether the model's template exposes a
//! thinking switch, the window memory affords, smaller draft models with the same tokenizer, and
//! unloading a model Dial loaded. Only loopback/private hosts are asked.

use super::catalog;
use super::local_fit::{self, Architecture};
use super::local_format::{self, GgufInfo, LocalFormat};
use super::model_config::{RequestHeader, ResolvedHeaders, resolve_headers};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// What one local server reports about itself and the model it serves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerFacts {
    /// Parallel decoding slots (llama.cpp `total_slots`), when the server declares them.
    pub slots: Option<u32>,
    /// Whether the model's chat template or server exposes a thinking switch. `Some(false)` is a
    /// known absence; `None` is unknown.
    pub thinking_switch: Option<bool>,
}

/// Template marker of the switch Qwen-style templates read from `enable_thinking`.
const THINKING_MARKER: &str = "enable_thinking";
/// Draft models offered for one target at most.
const MAX_DRAFT_CANDIDATES: usize = 3;
/// Tokenizer files larger than this are not digested.
const MAX_TOKENIZER_BYTES: u64 = 64 * 1024 * 1024;

/// Facts for one route. `base_url: None` is Dial's own runtime, read from the model's files; any
/// other URL is asked over its native API when it is a loopback or private host (a hosted API is
/// never contacted). Successful answers are cached per `(base_url, model)`; a transport failure is
/// not, so a server that is still starting is asked again.
pub fn facts(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    token: Option<&str>,
    headers: &[RequestHeader],
) -> ServerFacts {
    let Some(base) = base_url else {
        return dial_runtime_facts(model);
    };
    if provider == "lm-studio" {
        // LM Studio's REST API documents no template thinking switch to read.
        return ServerFacts::default();
    }
    cached(base, model, || {
        let resolved = resolve_headers(headers).ok()?;
        if provider == "ollama" {
            ollama_facts(base, model, token, &resolved)
        } else {
            llama_facts(base, token, &resolved)
        }
    })
}

/// Whether `model` is loaded in memory on the server at `base_url`. Ollama reports `/api/ps`, LM
/// Studio its `/api/v0/models` state, llama.cpp is loaded while it answers. `None` when the server
/// cannot tell (not reachable, not the expected API, or not a local host).
pub fn model_loaded(
    provider: &str,
    model: &str,
    base_url: &str,
    token: Option<&str>,
    headers: &[RequestHeader],
) -> Option<bool> {
    let resolved = resolve_headers(headers).ok()?;
    match provider {
        "ollama" => {
            let running = local_get(base_url, "/api/ps", token, &resolved)?;
            Some(ollama_listed(&running["models"], model).is_some())
        }
        "lm-studio" => {
            if !catalog::valid_model_id(model) {
                return None;
            }
            if model.contains('/') {
                // A publisher-qualified id is not a single path segment: search the list instead.
                let list = local_get(base_url, "/api/v0/models", token, &resolved)?;
                let row = list["data"]
                    .as_array()?
                    .iter()
                    .find(|row| row["id"].as_str() == Some(model));
                Some(row.is_some_and(|row| row["state"].as_str() == Some("loaded")))
            } else {
                let row = local_get(
                    base_url,
                    &format!("/api/v0/models/{model}"),
                    token,
                    &resolved,
                )?;
                Some(row["state"].as_str() == Some("loaded"))
            }
        }
        _ if catalog::is_llama_server(base_url, token, headers) => Some(true),
        _ => None,
    }
}

/// Unloads `model` from the Ollama server at `base_url`, freeing its memory now. Ollama documents
/// this as a request with `keep_alive: 0` and no prompt. Other servers have no unload this module
/// uses, so they return `Ok(false)` without a request. A failed Ollama request is an error.
pub fn unload(
    provider: &str,
    model: &str,
    base_url: &str,
    token: Option<&str>,
    headers: &[RequestHeader],
) -> Result<bool> {
    if provider != "ollama" {
        return Ok(false);
    }
    ensure!(!model.is_empty(), "no model to unload");
    let resolved = resolve_headers(headers)?;
    let body = json!({ "model": model, "keep_alive": 0 });
    ensure!(
        local_request(base_url, "/api/generate", Some(&body), token, &resolved).is_some(),
        "Ollama did not confirm unloading {model}"
    );
    Ok(true)
}

/// The context an Ollama model can take within the memory estimate: its attention layout from
/// `/api/show` `model_info` and its weight bytes from `/api/tags`, sized by
/// `local_fit::max_context` against this machine's memory. `None` when the server does not report
/// the layout or the size, or memory is unknown.
pub fn affordable_context(
    model: &str,
    base_url: &str,
    token: Option<&str>,
    headers: &[RequestHeader],
    kv_bits: Option<u8>,
) -> Option<u32> {
    let resolved = resolve_headers(headers).ok()?;
    let shown = local_request(
        base_url,
        "/api/show",
        Some(&json!({ "model": model })),
        token,
        &resolved,
    )?;
    let tags = local_get(base_url, "/api/tags", token, &resolved)?;
    let weights = ollama_listed(&tags["models"], model)?["size"]
        .as_u64()
        .filter(|bytes| *bytes > 0)?;
    affordable_from(&shown, weights, kv_bits, local_fit::physical_memory_bytes())
}

/// Smaller installed models with the same tokenizer as `model`, for speculative decoding. Only the
/// same format is compared (MLX with MLX, GGUF with GGUF); the tokenizer digests are content-based
/// (`tokenizer.json` vocabulary and merges, or the GGUF `tokenizer.ggml.tokens` array). Nearest
/// size first, at most three. Empty for hosted or unknown models.
pub fn draft_candidates(model: &str) -> Vec<String> {
    let pool: Vec<PathBuf> = catalog::local_model_entries(None)
        .into_iter()
        .map(|entry| PathBuf::from(entry.selection.model))
        .collect();
    draft_candidates_among(Path::new(model), &pool)
}

fn draft_candidates_among(model: &Path, pool: &[PathBuf]) -> Vec<String> {
    let Some(format) = local_format::detect(model) else {
        return Vec::new();
    };
    let Some(target) = Fingerprint::of(model, format) else {
        return Vec::new();
    };
    let mut smaller: Vec<(u64, &Path)> = pool
        .iter()
        .filter(|candidate| candidate.as_path() != model)
        .filter(|candidate| local_format::detect(candidate) == Some(format))
        .filter_map(|candidate| Some((weight_bytes(candidate, format)?, candidate.as_path())))
        .filter(|(weights, _)| *weights < target.weights)
        .collect();
    // Nearest size first: the largest smaller model is the closest to the target.
    smaller.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    smaller
        .into_iter()
        .filter(|(_, candidate)| {
            Fingerprint::of(candidate, format).is_some_and(|f| f.tokenizer == target.tokenizer)
        })
        .take(MAX_DRAFT_CANDIDATES)
        .map(|(_, candidate)| candidate.to_string_lossy().into_owned())
        .collect()
}

/// Facts of Dial's own runtime: the template in the model's files decides the thinking switch. A
/// GGUF model runs on a Dial-launched `llama-server`, whose slot layout is decided by the installed
/// binary (`server::dial_llama_slots`); MLX servers have no slots.
fn dial_runtime_facts(model: &str) -> ServerFacts {
    let path = Path::new(model);
    let format = local_format::detect(path);
    let template = match format {
        Some(LocalFormat::Gguf) => {
            local_format::gguf_info(path).and_then(|info| info.chat_template)
        }
        Some(LocalFormat::Mlx) => catalog::chat_template(path),
        _ => None,
    };
    ServerFacts {
        slots: (format == Some(LocalFormat::Gguf))
            .then(super::server::dial_llama_slots)
            .flatten(),
        thinking_switch: template.map(|template| template.contains(THINKING_MARKER)),
    }
}

/// llama.cpp `/props`: `total_slots` and the model's `chat_template`. A server that answers without
/// either is not llama.cpp, so no facts are returned for it.
fn llama_facts(
    base: &str,
    token: Option<&str>,
    headers: &ResolvedHeaders<'_>,
) -> Option<ServerFacts> {
    let props = local_get(base, "/props", token, headers)?;
    if props.get("total_slots").is_none() && props.get("default_generation_settings").is_none() {
        return None;
    }
    catalog::record_llama_props(base, &props);
    Some(ServerFacts {
        slots: props["total_slots"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0),
        thinking_switch: props["chat_template"]
            .as_str()
            .map(|template| template.contains(THINKING_MARKER)),
    })
}

/// Ollama `/api/show` `capabilities`: `"thinking"` is the switch Ollama's `think` field reads.
fn ollama_facts(
    base: &str,
    model: &str,
    token: Option<&str>,
    headers: &ResolvedHeaders<'_>,
) -> Option<ServerFacts> {
    let shown = local_request(
        base,
        "/api/show",
        Some(&json!({ "model": model })),
        token,
        headers,
    )?;
    let capabilities = shown["capabilities"].as_array()?;
    Some(ServerFacts {
        slots: None,
        thinking_switch: Some(capabilities.iter().any(|c| c.as_str() == Some("thinking"))),
    })
}

/// The Ollama `/api/ps` or `/api/tags` row for `model`. Ollama's implicit `:latest` tag is ignored
/// on both sides, so `llama3` matches `llama3:latest`.
fn ollama_listed<'a>(rows: &'a Value, model: &str) -> Option<&'a Value> {
    let bare = |name: &str| name.strip_suffix(":latest").unwrap_or(name).to_owned();
    rows.as_array()?.iter().find(|row| {
        ["name", "model"].iter().any(|key| {
            row[*key]
                .as_str()
                .is_some_and(|name| bare(name) == bare(model))
        })
    })
}

/// The window `model` can take on this machine. Pure over the `/api/show` document so it can be
/// tested with a fixed memory size.
fn affordable_from(
    shown: &Value,
    weights: u64,
    kv_bits: Option<u8>,
    memory: Option<u64>,
) -> Option<u32> {
    let info = shown["model_info"].as_object()?;
    let arch = info.get("general.architecture")?.as_str()?;
    let number = |key: &str| info.get(&format!("{arch}.{key}")).and_then(Value::as_u64);
    let header = GgufInfo {
        architecture: Some(arch.to_owned()),
        context_length: number("context_length"),
        block_count: number("block_count"),
        head_count: number("attention.head_count"),
        head_count_kv: number("attention.head_count_kv"),
        embedding_length: number("embedding_length"),
        key_length: number("attention.key_length"),
        value_length: number("attention.value_length"),
        kv_lora_rank: number("attention.kv_lora_rank"),
        rope_dimension_count: number("rope.dimension_count"),
        full_attention_interval: number("full_attention_interval"),
        ..GgufInfo::default()
    };
    let layout = Architecture::from_gguf(&header)?;
    let tokens = local_fit::max_context(weights, &layout, kv_bits, memory);
    (tokens > 0).then(|| u32::try_from(tokens).unwrap_or(u32::MAX))
}

/// Fingerprint of a model for draft matching: weight bytes and the tokenizer digest.
struct Fingerprint {
    weights: u64,
    tokenizer: String,
}

impl Fingerprint {
    fn of(path: &Path, format: LocalFormat) -> Option<Self> {
        Some(Self {
            weights: weight_bytes(path, format)?,
            tokenizer: tokenizer_digest(path, format)?,
        })
    }
}

fn weight_bytes(path: &Path, format: LocalFormat) -> Option<u64> {
    match format {
        LocalFormat::Mlx => local_fit::safetensors_bytes(path),
        LocalFormat::Gguf => local_format::gguf_weight_bytes(path),
        LocalFormat::Splash => None,
    }
}

fn tokenizer_digest(path: &Path, format: LocalFormat) -> Option<String> {
    match format {
        LocalFormat::Mlx => mlx_tokenizer_digest(path),
        LocalFormat::Gguf => local_format::gguf_tokenizer_digest(path),
        LocalFormat::Splash => None,
    }
}

/// SHA-256 (lowercase hex) of an MLX checkpoint's `tokenizer.json` vocabulary, merges and added
/// tokens. Two checkpoints with the same digest encode text identically.
fn mlx_tokenizer_digest(dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let file = std::fs::File::open(dir.join("tokenizer.json")).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TOKENIZER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_TOKENIZER_BYTES {
        return None;
    }
    let tokenizer: Value = serde_json::from_slice(&bytes).ok()?;
    let model = &tokenizer["model"];
    let parts = json!([
        model.get("vocab")?,
        model.get("merges"),
        tokenizer.get("added_tokens")
    ]);
    let digest = Sha256::digest(serde_json::to_vec(&parts).ok()?);
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

type FactsKey = (String, String);

/// Answers already learned for `(base_url, model)`. Only successful loads are stored.
fn cached(base: &str, model: &str, load: impl FnOnce() -> Option<ServerFacts>) -> ServerFacts {
    static KNOWN: LazyLock<Mutex<BTreeMap<FactsKey, ServerFacts>>> =
        LazyLock::new(Default::default);
    let key = (base.trim_end_matches('/').to_owned(), model.to_owned());
    if let Some(known) = KNOWN.lock().ok().and_then(|known| known.get(&key).copied()) {
        return known;
    }
    match load() {
        Some(facts) => {
            if let Ok(mut known) = KNOWN.lock() {
                known.insert(key, facts);
            }
            facts
        }
        None => ServerFacts::default(),
    }
}

/// GET on a local server's native API; `None` for hosted hosts, failures and non-success replies.
fn local_get(
    base_url: &str,
    path: &str,
    token: Option<&str>,
    headers: &ResolvedHeaders<'_>,
) -> Option<Value> {
    catalog::local_get(base_url, path, token, headers)
}

/// JSON POST on a local server's native API; `None` for hosted hosts, failures and non-success replies.
fn local_request(
    base_url: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
    headers: &ResolvedHeaders<'_>,
) -> Option<Value> {
    catalog::local_request(base_url, path, body, token, headers)
}

#[cfg(test)]
mod tests;
