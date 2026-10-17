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

/// Facts of Dial's own runtime: the template in the model's files decides the thinking switch.
fn dial_runtime_facts(model: &str) -> ServerFacts {
    let path = Path::new(model);
    let template = match local_format::detect(path) {
        Some(LocalFormat::Gguf) => {
            local_format::gguf_info(path).and_then(|info| info.chat_template)
        }
        Some(LocalFormat::Mlx) => catalog::chat_template(path),
        _ => None,
    };
    ServerFacts {
        slots: None,
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
mod tests {
    use super::*;
    use crate::llm::catalog::tests::routed;
    use crate::llm::local_format::gguf_metadata_tests::{header, string};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn template_dir(template: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        std::fs::write(dir.path().join("model.safetensors"), b"weights").unwrap();
        if let Some(template) = template {
            std::fs::write(dir.path().join("chat_template.jinja"), template).unwrap();
        }
        dir
    }

    #[test]
    fn dial_runtime_reads_the_thinking_switch_from_the_mlx_template() {
        let with = template_dir(Some("{% if enable_thinking %}<think>{% endif %}"));
        let facts = facts("mlx", with.path().to_str().unwrap(), None, None, &[]);
        assert_eq!(
            facts,
            ServerFacts {
                slots: None,
                thinking_switch: Some(true)
            }
        );
        let without = template_dir(Some("{{ messages }}"));
        let facts = super::facts("mlx", without.path().to_str().unwrap(), None, None, &[]);
        assert_eq!(
            facts.thinking_switch,
            Some(false),
            "a known template with no switch"
        );
        let bare = template_dir(None);
        let facts = super::facts("mlx", bare.path().to_str().unwrap(), None, None, &[]);
        assert_eq!(facts, ServerFacts::default(), "no template is unknown");
    }

    #[test]
    fn dial_runtime_reads_the_gguf_header_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        let template = "{% if enable_thinking %}think{% endif %}";
        let bytes = header(&[
            ("general.architecture", string("llama")),
            ("tokenizer.chat_template", string(template)),
        ]);
        std::fs::write(&path, bytes).unwrap();
        let facts = facts("llama-cpp", path.to_str().unwrap(), None, None, &[]);
        assert_eq!(facts.thinking_switch, Some(true));
    }

    #[test]
    fn llama_cpp_props_report_slots_and_the_template_switch() {
        let url = routed(vec![(
            "/props",
            r#"{"total_slots":4,"chat_template":"{% if enable_thinking %}x{% endif %}","default_generation_settings":{"n_ctx":4096}}"#.into(),
        )]);
        let facts = facts("llama-cpp", "qwen", Some(&url), None, &[]);
        assert_eq!(
            facts,
            ServerFacts {
                slots: Some(4),
                thinking_switch: Some(true)
            }
        );
        let plain = routed(vec![(
            "/props",
            r#"{"total_slots":1,"chat_template":"{{ messages }}","default_generation_settings":{}}"#.into(),
        )]);
        let facts = super::facts("llama-cpp", "qwen", Some(&plain), None, &[]);
        assert_eq!(
            facts,
            ServerFacts {
                slots: Some(1),
                thinking_switch: Some(false)
            }
        );
        let not_llama = routed(vec![]);
        assert_eq!(
            super::facts("llama-cpp", "qwen", Some(&not_llama), None, &[]),
            ServerFacts::default()
        );
    }

    #[test]
    fn ollama_capabilities_report_the_thinking_switch_and_lm_studio_reports_none() {
        let url = routed(vec![(
            "/api/show",
            r#"{"capabilities":["completion","thinking"],"model_info":{}}"#.into(),
        )]);
        let facts = facts("ollama", "qwen3", Some(&url), None, &[]);
        assert_eq!(
            facts,
            ServerFacts {
                slots: None,
                thinking_switch: Some(true)
            }
        );
        let plain = routed(vec![(
            "/api/show",
            r#"{"capabilities":["completion"]}"#.into(),
        )]);
        assert_eq!(
            super::facts("ollama", "llama3", Some(&plain), None, &[]).thinking_switch,
            Some(false)
        );
        // LM Studio is never asked: its answer is always unknown here.
        let studio = routed(vec![("/api/v0/models", r#"{"data":[]}"#.into())]);
        assert_eq!(
            super::facts("lm-studio", "x", Some(&studio), None, &[]),
            ServerFacts::default()
        );
    }

    #[test]
    fn hosted_routes_are_never_asked_for_facts_or_unloaded() {
        let hosted = "https://api.example.invalid/v1";
        assert_eq!(
            super::facts("llama-cpp", "m", Some(hosted), Some("secret"), &[]),
            ServerFacts::default()
        );
        assert_eq!(
            model_loaded("ollama", "m", hosted, Some("secret"), &[]),
            None
        );
        assert!(unload("ollama", "m", hosted, Some("secret"), &[]).is_err());
    }

    #[test]
    fn model_loaded_reads_ollama_ps_lm_studio_state_and_llama_cpp() {
        let ollama = routed(vec![(
            "/api/ps",
            r#"{"models":[{"name":"qwen2.5:7b","model":"qwen2.5:7b"},{"name":"llama3:latest","model":"llama3:latest"}]}"#.into(),
        )]);
        assert_eq!(
            model_loaded("ollama", "qwen2.5:7b", &ollama, None, &[]),
            Some(true)
        );
        assert_eq!(
            model_loaded("ollama", "llama3", &ollama, None, &[]),
            Some(true),
            ":latest is implicit"
        );
        assert_eq!(
            model_loaded("ollama", "qwen2.5", &ollama, None, &[]),
            Some(false)
        );
        let studio = routed(vec![
            ("/api/v0/models/warm", r#"{"id":"warm","state":"loaded"}"#.into()),
            ("/api/v0/models/cold", r#"{"id":"cold","state":"not-loaded"}"#.into()),
            (
                "/api/v0/models",
                r#"{"data":[{"id":"qwen/qwen3","state":"loaded"},{"id":"other/x","state":"not-loaded"}]}"#.into(),
            ),
        ]);
        assert_eq!(
            model_loaded("lm-studio", "warm", &studio, None, &[]),
            Some(true)
        );
        assert_eq!(
            model_loaded("lm-studio", "cold", &studio, None, &[]),
            Some(false)
        );
        assert_eq!(
            model_loaded("lm-studio", "qwen/qwen3", &studio, None, &[]),
            Some(true)
        );
        assert_eq!(
            model_loaded("lm-studio", "other/x", &studio, None, &[]),
            Some(false)
        );
        assert_eq!(
            model_loaded("lm-studio", "missing", &studio, None, &[]),
            None
        );
        assert_eq!(model_loaded("lm-studio", "bad?x", &studio, None, &[]), None);
        let llama = routed(vec![(
            "/props",
            r#"{"default_generation_settings":{}}"#.into(),
        )]);
        assert_eq!(
            model_loaded("llama-cpp", "m", &llama, None, &[]),
            Some(true)
        );
        assert_eq!(
            model_loaded("llama-cpp", "m", &routed(vec![]), None, &[]),
            None
        );
    }

    /// One-shot server that records the request line and body, then answers `code` with `body`.
    fn capture(code: u16, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let job = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut received = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                received.push_str(&line);
            }
            let mut body_bytes = vec![0u8; length];
            reader.read_exact(&mut body_bytes).unwrap();
            received.push_str(&String::from_utf8_lossy(&body_bytes));
            write!(stream, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            received
        });
        (format!("http://{address}/v1"), job)
    }

    #[test]
    fn ollama_unload_sends_keep_alive_zero_and_other_servers_have_no_unload() {
        let (url, job) = capture(200, r#"{"model":"llama3","done_reason":"unload"}"#);
        assert!(unload("ollama", "llama3", &url, None, &[]).unwrap());
        let request = job.join().unwrap();
        // The body is pretty-printed by the client; compare it without layout whitespace.
        let compact: String = request.split_whitespace().collect();
        assert!(request.starts_with("POST /api/generate "), "{request}");
        assert!(compact.contains(r#""keep_alive":0"#), "{request}");
        assert!(compact.contains(r#""model":"llama3""#), "{request}");
        // No request is made for a server without a documented unload.
        assert!(!unload("lm-studio", "m", "http://127.0.0.1:9/v1", None, &[]).unwrap());
        assert!(!unload("llama-cpp", "m", "http://127.0.0.1:9/v1", None, &[]).unwrap());
        // A refused unload is an error, never a silent success.
        let (url, job) = capture(500, "{}");
        assert!(unload("ollama", "llama3", &url, None, &[]).is_err());
        job.join().unwrap();
    }

    fn llama3_show() -> Value {
        json!({"model_info": {
            "general.architecture": "llama",
            "llama.block_count": 32,
            "llama.attention.head_count": 32,
            "llama.attention.head_count_kv": 8,
            "llama.embedding_length": 4096,
            "llama.context_length": 8192
        }})
    }

    #[test]
    fn affordable_context_is_the_memory_budget_capped_at_the_native_window() {
        let shown = llama3_show();
        let weights = 4_500_000_000u64;
        let memory = 16u64 << 30;
        let expected = local_fit::max_context(
            weights,
            &Architecture::from_gguf(&GgufInfo {
                architecture: Some("llama".into()),
                block_count: Some(32),
                head_count: Some(32),
                head_count_kv: Some(8),
                embedding_length: Some(4096),
                context_length: Some(8192),
                ..GgufInfo::default()
            })
            .unwrap(),
            None,
            Some(memory),
        );
        assert!(expected > 0 && expected <= 8192, "{expected}");
        assert_eq!(
            affordable_from(&shown, weights, None, Some(memory)),
            Some(expected as u32)
        );
        assert_eq!(
            affordable_from(&shown, weights, None, None),
            None,
            "unknown memory"
        );
        assert_eq!(
            affordable_from(&shown, 64u64 << 30, None, Some(memory)),
            None,
            "weights exhaust the budget"
        );
        let no_layout = json!({"model_info": {"general.architecture": "llama"}});
        assert_eq!(
            affordable_from(&no_layout, weights, None, Some(memory)),
            None
        );
    }

    #[test]
    fn affordable_context_needs_the_reported_size_from_ollama() {
        // /api/tags lists no size for the model, so the weights are unknown and no window is claimed.
        let url = routed(vec![
            ("/api/show", llama3_show().to_string()),
            (
                "/api/tags",
                r#"{"models":[{"name":"other:latest"}]}"#.into(),
            ),
        ]);
        assert_eq!(affordable_context("llama3", &url, None, &[], None), None);
    }

    #[test]
    fn draft_candidates_are_smaller_same_tokenizer_models_nearest_first() {
        let root = tempfile::tempdir().unwrap();
        let mlx = |name: &str, weights: usize, tokenizer: &str| {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.json"), "{}").unwrap();
            std::fs::write(dir.join("model.safetensors"), vec![0u8; weights]).unwrap();
            std::fs::write(dir.join("tokenizer.json"), tokenizer).unwrap();
            dir
        };
        let same = r#"{"model":{"type":"BPE","vocab":{"a":0,"b":1},"merges":["a b"]}}"#;
        let target = mlx("target", 1000, same);
        let big = mlx("bigger", 2000, same);
        let near = mlx("near", 600, same);
        let far = mlx("far", 100, same);
        let other = mlx(
            "other",
            500,
            r#"{"model":{"type":"BPE","vocab":{"a":0,"c":1},"merges":[]}}"#,
        );
        let pool = vec![
            big.clone(),
            near.clone(),
            far.clone(),
            other,
            target.clone(),
        ];
        assert_eq!(
            draft_candidates_among(&target, &pool),
            vec![
                near.to_string_lossy().into_owned(),
                far.to_string_lossy().into_owned()
            ],
            "larger and different-tokenizer models are excluded, nearest size first"
        );
        let extra: Vec<PathBuf> = (0..4)
            .map(|i| mlx(&format!("small{i}"), 10 + i, same))
            .collect();
        assert_eq!(
            draft_candidates_among(&target, &extra).len(),
            MAX_DRAFT_CANDIDATES
        );
        // Hosted or unknown models have no draft.
        assert!(draft_candidates_among(Path::new("gpt-4o"), &pool).is_empty());
    }

    #[test]
    fn mlx_tokenizer_digest_follows_content_not_formatting() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        std::fs::write(
            a.path().join("tokenizer.json"),
            r#"{"model":{"vocab":{"x":0},"merges":[]}}"#,
        )
        .unwrap();
        std::fs::write(
            b.path().join("tokenizer.json"),
            "{\n \"model\": {\"vocab\": {\"x\": 0}, \"merges\": []}}\n",
        )
        .unwrap();
        std::fs::write(
            c.path().join("tokenizer.json"),
            r#"{"model":{"vocab":{"x":1},"merges":[]}}"#,
        )
        .unwrap();
        assert_eq!(
            mlx_tokenizer_digest(a.path()),
            mlx_tokenizer_digest(b.path())
        );
        assert!(mlx_tokenizer_digest(a.path()).is_some());
        assert_ne!(
            mlx_tokenizer_digest(a.path()),
            mlx_tokenizer_digest(c.path())
        );
        assert_eq!(mlx_tokenizer_digest(root_without_file().path()), None);
    }

    fn root_without_file() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn gguf_tokenizer_digest_is_the_token_list_only() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, tokens: &[&str], extra: Vec<u8>| {
            let path = dir.path().join(name);
            let mut array = 9u32.to_le_bytes().to_vec();
            array.extend(8u32.to_le_bytes());
            array.extend((tokens.len() as u64).to_le_bytes());
            for token in tokens {
                array.extend(string(token).get(4..).unwrap_or_default());
            }
            let mut entries = vec![
                ("general.architecture", string("llama")),
                ("tokenizer.ggml.tokens", array),
                ("general.file_type", extra),
            ];
            entries.sort_by(|a, b| a.0.cmp(b.0));
            std::fs::write(&path, header(&entries)).unwrap();
            path
        };
        let one = write(
            "one.gguf",
            &["a", "bc"],
            crate::llm::local_format::gguf_metadata_tests::uint32(1),
        );
        let two = write(
            "two.gguf",
            &["a", "bc"],
            crate::llm::local_format::gguf_metadata_tests::uint32(7),
        );
        let other = write(
            "other.gguf",
            &["a", "bd"],
            crate::llm::local_format::gguf_metadata_tests::uint32(1),
        );
        let digest = local_format::gguf_tokenizer_digest(&one);
        assert!(digest.is_some());
        assert_eq!(
            digest,
            local_format::gguf_tokenizer_digest(&two),
            "other keys do not matter"
        );
        assert_ne!(digest, local_format::gguf_tokenizer_digest(&other));
    }
}
