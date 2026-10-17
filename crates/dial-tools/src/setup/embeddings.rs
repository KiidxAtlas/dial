//! Local embedding models for semantic memory, pulled through Ollama's native API.
//!
//! The curated catalog is what the settings role picker and `/setup embeddings` offer. Any other
//! Ollama model name is accepted when the user types it. Pulling is an explicit user action
//! (picking a curated model on the settings row and pressing Ctrl+L, `/setup embeddings:<name> yes`,
//! or `dial doctor --fix ... --yes`).
//! Discovery, previews and the settings overlay never fetch weights.
//!
//! Config is written only after the pull has finished. `model_roles.embedding` is replaced as a
//! whole with `ollama / <model>` through the canonical selection writer, so a previous remote
//! endpoint's base URL, key variable, headers, account and protocol do not survive. The
//! `memory.semantic.embedding` mode becomes `on` when it was `off`. The explicit
//! `embedding_base_url` / `embedding_model` overrides are OpenAI-compatible and are left alone;
//! they still win over the role when set.
//! Setup reaches the Ollama endpoint that `ollama` resolves to in config (a configured provider's
//! URL, headers and key, or the local default), and the saved role points at that same endpoint.
//! Only the built-in local default is started or installed by setup.

use crate::config::memory_backend::{EmbeddingMode, MemoryBackendId};
use crate::config::paths::Layout;
use crate::config::selection;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::Config;
use crate::llm::model_config::{HeaderValue, ModelSelection, RequestHeader, resolve_headers};
use crate::setup::deps::{self, Env};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Settings path of the embedding model row, which carries the setup offer.
pub const EMBEDDING_MODEL_PATH: &str = "memory.semantic.embedding_model";
/// The model offered when nothing is configured.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";
/// First words of a successful setup summary; the app recognizes a finished embedding setup by it.
pub const SUMMARY_PREFIX: &str = "Embedding model: ";
/// `/setup memory <this>`: the single-key offer to switch memory to semantic, run after an
/// embedding model is set up or assigned while another backend is selected.
pub const OFFER_SEMANTIC_ARG: &str = "offer-semantic";
/// Prefix of the setup id that the settings overlay and `/setup` use for these models.
const ID_PREFIX: &str = "embeddings";
const INSTALL_HINT: &str = "Ollama is not installed. Install it from https://ollama.com/download (macOS: brew install ollama), then run this again.";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ERROR_BODY: u64 = 64 * 1024;

/// One curated embedding model. Sizes are the Ollama library download sizes, rounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub name: &'static str,
    pub dims: u32,
    pub size_mb: u32,
    pub description: &'static str,
}

pub const CATALOG: &[Model] = &[
    Model {
        name: "nomic-embed-text",
        dims: 768,
        size_mb: 274,
        description: "General English embedder with an 8k context; a good default.",
    },
    Model {
        name: "all-minilm",
        dims: 384,
        size_mb: 46,
        description: "Smallest and fastest; English only, best for short texts on limited machines.",
    },
    Model {
        name: "mxbai-embed-large",
        dims: 1024,
        size_mb: 670,
        description: "Larger English embedder for higher-quality retrieval.",
    },
    Model {
        name: "embeddinggemma",
        dims: 768,
        size_mb: 622,
        description: "Google's about-300M multilingual embedder built on Gemma 3.",
    },
    Model {
        name: "bge-m3",
        dims: 1024,
        size_mb: 1200,
        description: "Multilingual, multi-granularity embedder with long inputs; large download.",
    },
    Model {
        name: "snowflake-arctic-embed2",
        dims: 1024,
        size_mb: 1200,
        description: "Multilingual retrieval embedder (about 568M); strong outside English.",
    },
];

/// The catalog entry for `name`, if it is one of the curated models.
pub fn find(name: &str) -> Option<&'static Model> {
    CATALOG.iter().find(|model| model.name == name)
}

/// Setup id for a model: `embeddings:<name>`. The default model is also `embeddings`.
pub fn setup_id(name: &str) -> String {
    format!("{ID_PREFIX}:{name}")
}

/// The model a setup id names, or `None` if the id is not an embeddings id or the name is invalid.
pub fn model_from_id(id: &str) -> Option<&str> {
    if id == ID_PREFIX {
        return Some(DEFAULT_MODEL);
    }
    let name = id.strip_prefix("embeddings:")?;
    validate_name(name).is_ok().then_some(name)
}

/// Ollama model names: letters, digits and `-_./:`, not starting with `-`, at most 128 bytes.
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "embedding model name is empty");
    ensure!(name.len() <= 128, "embedding model name is too long");
    ensure!(
        !name.starts_with('-'),
        "embedding model name must not start with '-'"
    );
    ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':')),
        "embedding model name {name:?} may contain only letters, digits and - _ . / :"
    );
    Ok(())
}

/// The catalog as text for `/setup embeddings`.
pub fn catalog_text() -> String {
    let mut lines = vec![
        "Local embedding models (Ollama). Set one up with /setup embeddings:<name>:".to_owned(),
    ];
    let width = CATALOG.iter().map(|m| m.name.len()).max().unwrap_or(0);
    for model in CATALOG {
        lines.push(format!(
            "  {:<width$}  {:>4} dims  {:>5} MB  {}",
            model.name, model.dims, model.size_mb, model.description
        ));
    }
    lines.push(
        "Any other Ollama embedding model name also works, for example /setup embeddings:<name>."
            .to_owned(),
    );
    lines.join("\n")
}

/// Whether an embedding model is already assigned, either as the role or as a complete override.
pub fn configured(config: &Config) -> bool {
    let semantic = &config.memory.semantic;
    config.role_selection("embedding").is_some()
        || (!semantic.embedding_base_url.trim().is_empty()
            && !semantic.embedding_model.trim().is_empty())
}

/// One progress frame of Ollama's `/api/pull` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullUpdate {
    Status(String),
    Progress {
        status: String,
        completed: u64,
        total: u64,
    },
    Success,
}

/// Parses one NDJSON line of `/api/pull`. Blank lines are `None`; an Ollama `error` frame or
/// a line that is not JSON is an error.
pub fn parse_pull_line(line: &str) -> Result<Option<PullUpdate>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(line).context("Ollama pull stream sent a non-JSON line")?;
    if let Some(error) = value["error"].as_str() {
        bail!("Ollama could not pull the model: {error}");
    }
    let status = value["status"].as_str().unwrap_or_default().to_owned();
    if status == "success" {
        return Ok(Some(PullUpdate::Success));
    }
    match (value["completed"].as_u64(), value["total"].as_u64()) {
        (Some(completed), Some(total)) if total > 0 => Ok(Some(PullUpdate::Progress {
            status,
            completed: completed.min(total),
            total,
        })),
        _ => Ok(Some(PullUpdate::Status(status))),
    }
}

/// A human line for one update, for the job's progress output.
pub fn describe(update: &PullUpdate) -> String {
    match update {
        PullUpdate::Status(status) => status.clone(),
        PullUpdate::Success => "success".to_owned(),
        PullUpdate::Progress {
            status,
            completed,
            total,
        } => format!(
            "{status}: {}% ({} of {} MB)",
            completed * 100 / total,
            completed / 1_000_000,
            total / 1_000_000
        ),
    }
}

/// Whether to show an update: every status change, and each five-point step of a download.
fn should_report(update: &PullUpdate, last: &mut Option<(String, u64)>) -> bool {
    match update {
        PullUpdate::Status(_) | PullUpdate::Success => {
            *last = None;
            true
        }
        PullUpdate::Progress {
            status,
            completed,
            total,
        } => {
            let step = completed * 100 / total / 5;
            let fresh = match last {
                Some((previous, previous_step)) => {
                    previous.as_str() != status.as_str() || *previous_step != step
                }
                None => true,
            };
            if fresh {
                *last = Some((status.clone(), step));
            }
            fresh
        }
    }
}

/// Model names from `/api/tags`.
pub fn parse_tags(text: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).context("Ollama model list is not JSON")?;
    let models = value["models"].as_array().cloned().unwrap_or_default();
    Ok(models
        .iter()
        .filter_map(|model| model["model"].as_str().or_else(|| model["name"].as_str()))
        .map(str::to_owned)
        .collect())
}

/// Whether `names` holds `model`. A name without a tag means `:latest`.
pub fn has_model(names: &[String], model: &str) -> bool {
    let wanted = with_tag(model);
    names.iter().any(|name| with_tag(name) == wanted)
}

fn with_tag(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("{name}:latest")
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Ollama server that setup reaches: the endpoint `ollama` resolves to in config, with the headers
/// and key the runtime sends to it, so the pull and the saved role point at one place.
pub struct Server {
    base_url: String,
    headers: Vec<RequestHeader>,
    token: Option<String>,
}

impl Server {
    /// A server with no headers or key, for tests that only have a base URL.
    #[cfg(test)]
    fn bare(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            headers: Vec::new(),
            token: None,
        }
    }

    /// URL of a native Ollama API path. A trailing `/v1` (the OpenAI-compatible base some configs
    /// use) is dropped, as the embedding route does.
    fn api(&self, path: &str) -> String {
        let root = self.base_url.trim_end_matches('/');
        let root = root.strip_suffix("/v1").unwrap_or(root);
        format!("{root}{path}")
    }

    /// Only the built-in local address is started and installed by setup; any other server is one
    /// the user runs.
    fn is_local_default(&self) -> bool {
        self.base_url.trim_end_matches('/') == crate::llm::ollama::BASE_URL
    }

    /// Redacts the bearer key and env-sourced header values from server-controlled text.
    /// Literal header values are not secret and are left alone.
    fn redact_text(&self, mut text: String) -> String {
        let mut secrets: Vec<String> = self.token.iter().cloned().collect();
        if let Ok(resolved) = resolve_headers(&self.headers) {
            secrets.extend(
                resolved
                    .pairs()
                    .zip(&self.headers)
                    .filter(|(_, row)| matches!(row.value, HeaderValue::Env(_)))
                    .map(|((_, value), _)| value.to_owned()),
            );
        }
        secrets.retain(|secret| !secret.is_empty());
        for secret in secrets {
            text = text.replace(&secret, "[redacted]");
        }
        text
    }

    fn redact(&self, error: anyhow::Error) -> anyhow::Error {
        anyhow::anyhow!(self.redact_text(format!("{error:#}")))
    }
}

/// Adds the server's configured headers and bearer key, as the runtime embedding request does.
fn authorize<B>(
    mut request: ureq::RequestBuilder<B>,
    server: &Server,
) -> Result<ureq::RequestBuilder<B>> {
    let resolved = resolve_headers(&server.headers)?;
    for (name, value) in resolved.pairs() {
        request = request.header(name, value);
    }
    if let Some(token) = &server.token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    Ok(request)
}

/// Whether an Ollama server answers at `server`. Bounded to two seconds.
fn reachable(server: &Server) -> bool {
    authorize(
        agent(Duration::from_secs(2)).get(server.api("/api/version")),
        server,
    )
    .is_ok_and(|request| {
        request
            .call()
            .is_ok_and(|response| response.status().is_success())
    })
}

/// Model names the server has on disk. Errors never carry the key or env-sourced header values.
fn list_models(server: &Server) -> Result<Vec<String>> {
    fetch_models(server).map_err(|error| server.redact(error))
}

fn fetch_models(server: &Server) -> Result<Vec<String>> {
    let url = server.api("/api/tags");
    let request = authorize(agent(Duration::from_secs(5)).get(&url), server)?;
    let mut response = request.call().with_context(|| format!("GET {url}"))?;
    ensure!(
        response.status().is_success(),
        "Ollama model list returned HTTP {}",
        response.status().as_u16()
    );
    let mut text = String::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_ERROR_BODY * 16)
        .read_to_string(&mut text)
        .with_context(|| format!("reading {url}"))?;
    parse_tags(&text)
}

/// Model names on the Ollama endpoint config resolves for `ollama`: the same server setup pulls from,
/// with its configured headers and key. Only the model list is read.
pub fn configured_models(layout: &Layout) -> Result<Vec<String>> {
    let config = Config::load(layout, None)?;
    let server = resolve_server(layout, &config)?;
    list_models(&server)
}

/// Pulls `model` through Ollama's streaming `/api/pull`. Ollama checks each layer's digest
/// itself. `cancel` is checked between progress frames; closing the stream stops the pull.
pub fn pull(
    server: &Server,
    model: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    pull_stream(server, model, cancel, &mut |line| {
        progress(&server.redact_text(line.to_owned()));
    })
    .map_err(|error| server.redact(error))
}

fn pull_stream(
    server: &Server,
    model: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    validate_name(model)?;
    let url = server.api("/api/pull");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_recv_response(Some(PULL_RESPONSE_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let body = json!({"model": model, "stream": true}).to_string();
    let request = authorize(
        agent.post(&url).header("Content-Type", "application/json"),
        server,
    )?;
    let mut response = request.send(body).with_context(|| format!("POST {url}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let mut text = String::new();
        let _ = response
            .body_mut()
            .as_reader()
            .take(MAX_ERROR_BODY)
            .read_to_string(&mut text);
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        };
        bail!("Ollama refused to pull {model}: HTTP {status}{detail}");
    }
    progress(&format!("pulling {model} from the Ollama library"));
    let reader = BufReader::new(response.body_mut().as_reader());
    let mut finished = false;
    let mut last = None;
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled while pulling {model}");
        }
        let line =
            line.with_context(|| format!("reading the pull stream from {}", server.base_url))?;
        match parse_pull_line(&line)? {
            None => {}
            Some(PullUpdate::Success) => {
                finished = true;
                break;
            }
            Some(update) if should_report(&update, &mut last) => progress(&describe(&update)),
            Some(_) => {}
        }
    }
    ensure!(
        finished,
        "Ollama closed the pull of {model} before it finished; run the setup again"
    );
    Ok(())
}

/// Config changes that make `model` the embedding role. The role is replaced whole by
/// `selection::changes`, the same writer `/model` uses, so stale endpoint fields are cleared.
/// `embedding` turns `off` into `on`; `auto` and `on` are kept as the user set them. The explicit
/// overrides are never written here.
pub fn wiring(config: &Config, model: &str, mode: EmbeddingMode) -> Result<Vec<Change>> {
    let mut out = selection::changes(config, &ModelSelection::new("ollama", model), "embedding")?;
    if mode == EmbeddingMode::Off {
        out.push(Change::Set {
            path: "memory.semantic.embedding".into(),
            value: toml::Value::String("on".into()),
        });
    }
    Ok(out)
}

/// Ensures the server answers. Only the local default is started (installing Ollama through the
/// dependency registry first when the registry knows how); other servers must already be running.
fn ensure_ollama(
    layout: &Layout,
    server: &Server,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    if reachable(server) {
        return Ok(());
    }
    ensure!(
        server.is_local_default(),
        "Ollama is not reachable at {}; start it there and run the setup again",
        server.base_url
    );
    let program = match Env::current(&layout.user_dir).find("ollama") {
        Some(program) => program,
        None => {
            ensure!(deps::recipe("ollama").is_some(), "{INSTALL_HINT}");
            progress("installing Ollama");
            deps::run::install_and_save(layout, "ollama", cancel, progress)?;
            Env::current(&layout.user_dir)
                .find("ollama")
                .context("Ollama was installed but is not on PATH")?
        }
    };
    progress("starting ollama serve");
    Command::new(&program)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {} serve", program.display()))?;
    let started = Instant::now();
    while !reachable(server) {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "cancelled while starting Ollama"
        );
        ensure!(
            started.elapsed() < START_TIMEOUT,
            "Ollama did not start within {}s; run `ollama serve` in a terminal and try again",
            START_TIMEOUT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// The Ollama endpoint `ollama` resolves to in config: a configured provider's URL, headers and key,
/// else the local default. The same resolution feeds `wiring`, so the pull and the role agree.
fn resolve_server(layout: &Layout, config: &Config) -> Result<Server> {
    let endpoint = config
        .selection_endpoint(&ModelSelection::new("ollama", ""))
        .context("Ollama has no endpoint configured")?;
    crate::llm::provider::endpoint_url(&endpoint.base_url, endpoint.allow_insecure)
        .context("Ollama endpoint")?;
    let token = crate::llm::catalog::resolve_credential(
        &layout.user_dir,
        &endpoint.base_url,
        endpoint.api_key_env.as_deref(),
    )?;
    Ok(Server {
        base_url: endpoint.base_url,
        headers: endpoint.headers.unwrap_or_default(),
        token,
    })
}

/// Ensures Ollama runs, pulls `model` if it is missing, then saves the embedding wiring.
/// Settings are written only after the pull succeeds. Returns a summary for the user.
pub fn setup(
    layout: &Layout,
    model: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    validate_name(model)?;
    let config = Config::load(layout, None)?;
    let server = resolve_server(layout, &config)?;
    (|| -> Result<()> {
        ensure_ollama(layout, &server, cancel, progress)?;
        if has_model(&list_models(&server)?, model) {
            progress(&format!("{model} is already pulled"));
        } else {
            pull(&server, model, cancel, progress)?;
            progress("pulled");
        }
        Ok(())
    })()
    .map_err(|error| server.redact(error))?;
    let saved = settings_store::save_model_config(
        layout,
        Scope::Global,
        &wiring(&config, model, config.memory.semantic.embedding)?,
        "embedding",
    )?;
    Ok(summary(model, &config, &saved))
}

/// What setup reports. Mode, role and overrides are read from `saved`, the config after the write, so
/// a project setting that still overrides the global change is reported instead of hidden.
fn summary(model: &str, before: &Config, saved: &Config) -> String {
    let mut lines = vec![format!(
        "{SUMMARY_PREFIX}ollama / {model} (model_roles.embedding, all projects)"
    )];
    if let Some(role) = saved.role_selection("embedding")
        && !(role.provider == "ollama" && role.model == model)
    {
        lines.push(format!(
            "This project assigns its own embedding role ({}), which overrides the new global one. Clear it in /roles (Alt+M) with \"This project\" scope to use {model}.",
            role.display_name()
        ));
    }
    if before.memory.semantic.embedding == EmbeddingMode::Off {
        if saved.memory.semantic.embedding == EmbeddingMode::On {
            lines.push("memory.semantic.embedding is now on.".to_owned());
        } else {
            let mode = match saved.memory.semantic.embedding {
                EmbeddingMode::Off => "off",
                EmbeddingMode::Auto => "auto",
                EmbeddingMode::On => "on",
            };
            lines.push(format!(
                "memory.semantic.embedding is still {mode}: a project setting overrides the global change. Set it to on in the project settings."
            ));
        }
    }
    let semantic = &saved.memory.semantic;
    if !semantic.embedding_base_url.trim().is_empty() || !semantic.embedding_model.trim().is_empty()
    {
        lines.push("The explicit embedding_base_url / embedding_model overrides still win over this role; clear them to use it.".to_owned());
    }
    if saved.memory.backend != MemoryBackendId::Semantic {
        lines.push(format!(
            "Only the semantic memory backend uses embeddings; memory is currently {}. Switch with /setup memory semantic or in /settings → Memory.",
            saved.memory.backend.as_str()
        ));
    }
    lines.join("\n")
}

/// What `/setup embeddings:<name> yes` will do, shown before anything runs. Reads only local
/// state; it never contacts Ollama.
pub fn preview_text(model: &str, user_dir: &Path) -> String {
    preview_with(model, &Env::current(user_dir))
}

fn preview_with(model: &str, env: &Env) -> String {
    let mut lines = vec![match find(model) {
        Some(info) => format!(
            "Local embeddings with {model} ({} dims, about {} MB):",
            info.dims, info.size_mb
        ),
        None => format!("Local embeddings with {model}:"),
    }];
    if env.find("ollama").is_some() {
        lines.push("Ollama is installed. With no providers.ollama endpoint configured, it is started if it is not running; a configured endpoint must already be running.".to_owned());
    } else if deps::recipe("ollama").is_some() {
        lines.push("Ollama is not installed. With no providers.ollama endpoint configured, it is set up first:".to_owned());
        lines.push(deps::run::preview_text("ollama", env));
    } else {
        lines.push(INSTALL_HINT.to_owned());
    }
    lines.push(format!(
        "Pulls {model} through the Ollama endpoint (the configured providers.ollama, or the local default) into its model store, skipped if already there. Ollama verifies each layer's digest."
    ));
    lines.push(format!(
        "Then sets model_roles.embedding to ollama / {model}, and memory.semantic.embedding to on if it is off."
    ));
    lines.push(format!("Confirm with: /setup {} yes", setup_id(model)));
    lines.join("\n")
}

/// The embedding model row's setup offer for one model.
pub struct EmbeddingOffer {
    pub path: &'static str,
    pub id: String,
    pub title: String,
    pub preview: String,
}

fn offer(model: &str, env: &Env) -> EmbeddingOffer {
    EmbeddingOffer {
        path: EMBEDDING_MODEL_PATH,
        id: setup_id(model),
        title: format!("{model} (Ollama)"),
        preview: preview_with(model, env),
    }
}

/// Offers the default model on the embedding model row until one is configured.
pub fn setting_offer(user_dir: &Path, config: &Config) -> Option<EmbeddingOffer> {
    if configured(config) {
        return None;
    }
    Some(offer(DEFAULT_MODEL, &Env::current(user_dir)))
}

/// One setup offer per curated model, so Ctrl+L sets up the model chosen on the settings row.
pub fn catalog_offers(user_dir: &Path) -> Vec<EmbeddingOffer> {
    let env = Env::current(user_dir);
    CATALOG
        .iter()
        .map(|model| offer(model.name, &env))
        .collect()
}

#[cfg(test)]
mod tests;
