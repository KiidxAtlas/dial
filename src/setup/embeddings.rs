//! Local embedding models for semantic memory, pulled through Ollama's native API.
//!
//! The curated catalog is what the settings picker and `/setup embeddings` offer. Any other
//! Ollama model name is accepted when the user types it. Pulling is an explicit user action
//! (Ctrl+L on the settings row, `/setup embeddings:<name> yes`, or `dial doctor --fix ... --yes`).
//! Discovery, previews and the settings overlay never fetch weights.
//!
//! Config is written only after the pull has finished. `model_roles.embedding` is replaced as a
//! whole with `ollama / <model>` through the canonical selection writer, so a previous remote
//! endpoint's base URL, key variable, headers, account and protocol do not survive. The
//! `memory.semantic.embedding` mode becomes `on` when it was `off`. The explicit
//! `embedding_base_url` / `embedding_model` overrides are OpenAI-compatible and are left alone;
//! they still win over the role when set.

use crate::config::selection;
use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use crate::setup::deps::{self, Env};
use crate::storage::memory_backend::config::{EmbeddingMode, MemoryBackendId};
use crate::storage::paths::Layout;
use crate::storage::settings_store::{self, Change, Scope};
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

/// Picker suggestions for `memory.semantic.embedding_model`, `(name, label)` in catalog order.
/// A unit test keeps this list equal to [`CATALOG`].
pub const CHOICES: &[(&str, &str)] = &[
    ("nomic-embed-text", "nomic-embed-text · 768 dims · 274 MB"),
    ("all-minilm", "all-minilm · 384 dims · 46 MB"),
    (
        "mxbai-embed-large",
        "mxbai-embed-large · 1024 dims · 670 MB",
    ),
    ("embeddinggemma", "embeddinggemma · 768 dims · 622 MB"),
    ("bge-m3", "bge-m3 · 1024 dims · 1200 MB"),
    (
        "snowflake-arctic-embed2",
        "snowflake-arctic-embed2 · 1024 dims · 1200 MB",
    ),
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

/// Whether an Ollama server answers at `base`. Bounded to two seconds.
pub fn reachable(base: &str) -> bool {
    agent(Duration::from_secs(2))
        .get(format!("{}/api/version", base.trim_end_matches('/')))
        .call()
        .is_ok_and(|response| response.status().is_success())
}

/// Names of the models Ollama has on disk.
pub fn installed_models(base: &str) -> Result<Vec<String>> {
    let url = format!("{}/api/tags", base.trim_end_matches('/'));
    let mut response = agent(Duration::from_secs(5))
        .get(&url)
        .call()
        .with_context(|| format!("GET {url}"))?;
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

/// Pulls `model` through Ollama's streaming `/api/pull`. Ollama checks each layer's digest
/// itself. `cancel` is checked between progress frames; closing the stream stops the pull.
pub fn pull(
    base: &str,
    model: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    validate_name(model)?;
    let url = format!("{}/api/pull", base.trim_end_matches('/'));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_recv_response(Some(PULL_RESPONSE_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let body = json!({"model": model, "stream": true}).to_string();
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send(body)
        .with_context(|| format!("POST {url}"))?;
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
        let line = line.with_context(|| format!("reading the pull stream from {base}"))?;
        match parse_pull_line(&line)? {
            None => {}
            Some(PullUpdate::Success) => {
                finished = true;
                break;
            }
            Some(update) => {
                if should_report(&update, &mut last) {
                    progress(&describe(&update));
                }
            }
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

/// Starts `ollama serve` when the program is installed but no server answers. Installs Ollama
/// through the dependency registry first when the registry knows how.
fn ensure_ollama(
    layout: &Layout,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    if reachable(crate::llm::ollama::BASE_URL) {
        return Ok(());
    }
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
    while !reachable(crate::llm::ollama::BASE_URL) {
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
    ensure_ollama(layout, cancel, progress)?;
    if has_model(&installed_models(crate::llm::ollama::BASE_URL)?, model) {
        progress(&format!("{model} is already pulled"));
    } else {
        pull(crate::llm::ollama::BASE_URL, model, cancel, progress)?;
        progress("pulled");
    }
    settings_store::save_model_config(
        layout,
        Scope::Global,
        &wiring(&config, model, config.memory.semantic.embedding)?,
        "embedding",
    )?;
    let mut lines = vec![format!(
        "Embedding model: ollama / {model} (model_roles.embedding)"
    )];
    if config.memory.semantic.embedding == EmbeddingMode::Off {
        lines.push("memory.semantic.embedding is now on.".to_owned());
    }
    let semantic = &config.memory.semantic;
    if !semantic.embedding_base_url.trim().is_empty() || !semantic.embedding_model.trim().is_empty()
    {
        lines.push("The explicit embedding_base_url / embedding_model overrides still win over this role; clear them to use it.".to_owned());
    }
    if config.memory.backend != MemoryBackendId::Semantic {
        lines.push(format!(
            "Semantic recall needs the semantic memory backend; the current backend is {}. Select semantic in settings.",
            config.memory.backend.as_str()
        ));
    }
    Ok(lines.join("\n"))
}

/// What `/setup embeddings:<name> yes` will do, shown before anything runs. Reads only local
/// state; it never contacts Ollama.
pub fn preview_text(model: &str, user_dir: &Path) -> String {
    let mut lines = vec![match find(model) {
        Some(info) => format!(
            "Local embeddings with {model} ({} dims, about {} MB):",
            info.dims, info.size_mb
        ),
        None => format!("Local embeddings with {model}:"),
    }];
    let env = Env::current(user_dir);
    if env.find("ollama").is_some() {
        lines.push("Ollama is installed; it is started if it is not running.".to_owned());
    } else if deps::recipe("ollama").is_some() {
        lines.push("Ollama is not installed. It is set up first:".to_owned());
        lines.push(deps::run::preview_text("ollama", &env));
    } else {
        lines.push(INSTALL_HINT.to_owned());
    }
    lines.push(format!(
        "Pulls {model} through Ollama into its model store (skipped if already there). Ollama verifies each layer's digest."
    ));
    lines.push(format!(
        "Then sets model_roles.embedding to ollama / {model}, and memory.semantic.embedding to on if it is off."
    ));
    lines.push(format!("Confirm with: /setup {} yes", setup_id(model)));
    lines.join("\n")
}

/// The embedding model row's setup offer, when no embedding model is configured.
pub struct EmbeddingOffer {
    pub path: &'static str,
    pub id: String,
    pub title: String,
    pub preview: String,
}

/// Offers the default model on the embedding model row until one is configured.
pub fn setting_offer(user_dir: &Path, config: &Config) -> Option<EmbeddingOffer> {
    if configured(config) {
        return None;
    }
    Some(EmbeddingOffer {
        path: EMBEDDING_MODEL_PATH,
        id: setup_id(DEFAULT_MODEL),
        title: format!("{DEFAULT_MODEL} (Ollama)"),
        preview: preview_text(DEFAULT_MODEL, user_dir),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup_and_choices_agree() {
        assert_eq!(find("nomic-embed-text").map(|m| m.dims), Some(768));
        assert!(find("missing-model").is_none());
        let mut names: Vec<&str> = CATALOG.iter().map(|m| m.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CATALOG.len(), "catalog names must be unique");
        let choices: Vec<&str> = CHOICES.iter().map(|(name, _)| *name).collect();
        let catalog: Vec<&str> = CATALOG.iter().map(|m| m.name).collect();
        assert_eq!(choices, catalog);
        assert!(CATALOG.iter().all(|m| validate_name(m.name).is_ok()));
    }

    #[test]
    fn setup_ids_round_trip_and_reject_bad_names() {
        assert_eq!(model_from_id("embeddings"), Some(DEFAULT_MODEL));
        assert_eq!(model_from_id("embeddings:bge-m3"), Some("bge-m3"));
        assert_eq!(setup_id("bge-m3"), "embeddings:bge-m3");
        assert_eq!(model_from_id("embeddings:"), None);
        assert_eq!(model_from_id("embeddings:a b"), None);
        assert_eq!(model_from_id("bun"), None);
        assert_eq!(model_from_id("embeddingsx:bge"), None);
    }

    #[test]
    fn name_validation_rejects_flags_spaces_and_empties() {
        assert!(validate_name("library/model:0.6b").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("a;rm").is_err());
        assert!(validate_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn pull_lines_parse_into_status_progress_success_and_errors() {
        assert_eq!(parse_pull_line("  ").unwrap(), None);
        assert_eq!(
            parse_pull_line(r#"{"status":"pulling manifest"}"#).unwrap(),
            Some(PullUpdate::Status("pulling manifest".into()))
        );
        assert_eq!(
            parse_pull_line(
                r#"{"status":"pulling abc","digest":"sha256:abc","total":200,"completed":50}"#
            )
            .unwrap(),
            Some(PullUpdate::Progress {
                status: "pulling abc".into(),
                completed: 50,
                total: 200
            })
        );
        assert_eq!(
            parse_pull_line(r#"{"status":"success"}"#).unwrap(),
            Some(PullUpdate::Success)
        );
        let error =
            parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#).unwrap_err();
        assert!(error.to_string().contains("file does not exist"));
        assert!(parse_pull_line("<html>").is_err());
    }

    #[test]
    fn progress_is_described_and_throttled_to_five_point_steps() {
        let progress = |completed| PullUpdate::Progress {
            status: "pulling abc".into(),
            completed,
            total: 200_000_000,
        };
        assert_eq!(
            describe(&progress(50_000_000)),
            "pulling abc: 25% (50 of 200 MB)"
        );
        let mut last = None;
        assert!(should_report(&progress(0), &mut last));
        assert!(!should_report(&progress(4_000_000), &mut last));
        assert!(should_report(&progress(10_000_000), &mut last));
        assert!(should_report(
            &PullUpdate::Status("verifying".into()),
            &mut last
        ));
        assert!(should_report(&progress(10_000_000), &mut last));
    }

    #[test]
    fn installed_tags_match_with_or_without_latest() {
        let names = parse_tags(
            r#"{"models":[{"name":"nomic-embed-text:latest","model":"nomic-embed-text:latest"},{"name":"bge-m3:v1"}]}"#,
        )
        .unwrap();
        assert!(has_model(&names, "nomic-embed-text"));
        assert!(has_model(&names, "nomic-embed-text:latest"));
        assert!(!has_model(&names, "all-minilm"));
        assert!(has_model(&names, "bge-m3:v1"));
        assert!(!has_model(&names, "bge-m3"));
        assert!(parse_tags("nope").is_err());
    }

    fn sets(changes: &[Change]) -> Vec<(&str, Option<&str>)> {
        changes
            .iter()
            .filter_map(|change| match change {
                Change::Set { path, value } => Some((path.as_str(), value.as_str())),
                Change::Remove { .. } => None,
            })
            .collect()
    }

    #[test]
    fn wiring_points_the_role_at_ollama_and_turns_off_into_on() {
        let config = Config::default();
        let changes = wiring(&config, "bge-m3", EmbeddingMode::Off).unwrap();
        let off = sets(&changes);
        assert!(off.contains(&("model_roles.embedding.provider", Some("ollama"))));
        assert!(off.contains(&("model_roles.embedding.model", Some("bge-m3"))));
        assert!(off.contains(&("memory.semantic.embedding", Some("on"))));
        for mode in [EmbeddingMode::Auto, EmbeddingMode::On] {
            let changes = wiring(&config, "bge-m3", mode).unwrap();
            assert!(!changes.iter().any(
                |c| matches!(c, Change::Set { path, .. } if path == "memory.semantic.embedding")
            ));
        }
        assert!(wiring(&config, "", EmbeddingMode::Off).is_err());
    }

    #[test]
    fn switching_a_remote_embedding_role_to_ollama_clears_its_stale_route() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let headers: toml::Value = toml::from_str::<toml::Table>(
            "rows = [{ name = \"X-Remote-Feature\", value = { literal = \"custom\" } }]",
        )
        .unwrap()["rows"]
            .clone();
        let set = |path: &str, value: toml::Value| Change::Set {
            path: path.into(),
            value,
        };
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[
                set("model_roles.embedding.provider", "openai".into()),
                set(
                    "model_roles.embedding.model",
                    "text-embedding-3-small".into(),
                ),
                set(
                    "model_roles.embedding.base_url",
                    "https://remote.example/v1".into(),
                ),
                set(
                    "model_roles.embedding.api_key_env",
                    "REMOTE_EMBED_KEY".into(),
                ),
                set("model_roles.embedding.allow_insecure", true.into()),
                set("model_roles.embedding.protocol", "chat-completions".into()),
                set("model_roles.embedding.headers", headers),
                set("memory.semantic.embedding", "auto".into()),
                set(
                    "memory.semantic.embedding_base_url",
                    "http://127.0.0.1:9/v1".into(),
                ),
                set("memory.semantic.embedding_model", "override-model".into()),
            ],
        )
        .unwrap();
        let before = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
        assert!(
            before
                .role_selection("embedding")
                .unwrap()
                .headers
                .is_some()
        );
        let changes = wiring(&before, "bge-m3", before.memory.semantic.embedding).unwrap();
        settings_store::save_model_config(&layout, Scope::Global, &changes, "embedding").unwrap();

        let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
        let role = saved.role_selection("embedding").expect("role assigned");
        assert_eq!(role.provider, "ollama");
        assert_eq!(role.model, "bge-m3");
        assert_eq!(role.base_url.as_deref(), Some("http://127.0.0.1:11434"));
        assert!(role.api_key_env.as_deref().unwrap_or_default().is_empty());
        assert!(!role.allow_insecure);
        assert!(matches!(
            role.protocol,
            Some(crate::llm::model_config::ApiProtocol::Ollama)
        ));
        assert!(
            role.headers.is_none(),
            "headers inherit the Ollama provider"
        );
        assert!(role.account_id.as_deref().unwrap_or_default().is_empty());
        let endpoint = saved
            .for_role("embedding")
            .endpoint
            .expect("effective Ollama route");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:11434");
        assert!(endpoint.api_key_env.unwrap_or_default().is_empty());
        assert!(matches!(
            endpoint.protocol,
            Some(crate::llm::model_config::ApiProtocol::Ollama)
        ));
        // Mode and explicit overrides are untouched: auto stays auto, overrides still win.
        assert_eq!(saved.memory.semantic.embedding, EmbeddingMode::Auto);
        assert_eq!(
            saved.memory.semantic.embedding_base_url,
            "http://127.0.0.1:9/v1"
        );
        assert_eq!(saved.memory.semantic.embedding_model, "override-model");
    }

    #[test]
    fn configured_counts_a_role_or_a_complete_override_only() {
        let mut config = Config::default();
        assert!(!configured(&config));
        config.memory.semantic.embedding_base_url = "http://127.0.0.1:1/v1".into();
        assert!(!configured(&config));
        config.memory.semantic.embedding_model = "m".into();
        assert!(configured(&config));
        let mut role = Config::default();
        let selection: crate::llm::model_config::ModelSelection =
            toml::from_str("provider = \"ollama\"\nmodel = \"nomic-embed-text\"").unwrap();
        role.model_roles.insert("embedding".into(), selection);
        assert!(configured(&role));
    }

    #[test]
    fn offer_appears_only_while_no_embedding_model_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let offer = setting_offer(dir.path(), &config).expect("unconfigured offers the default");
        assert_eq!(offer.path, EMBEDDING_MODEL_PATH);
        assert_eq!(offer.id, "embeddings:nomic-embed-text");
        assert!(
            offer
                .preview
                .contains("Confirm with: /setup embeddings:nomic-embed-text yes")
        );
        let mut configured_config = Config::default();
        configured_config.memory.semantic.embedding_base_url = "http://127.0.0.1:1/v1".into();
        configured_config.memory.semantic.embedding_model = "m".into();
        assert!(setting_offer(dir.path(), &configured_config).is_none());
    }

    #[test]
    fn pull_reports_progress_and_stops_on_cancel() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if std::io::Read::read(&mut stream, &mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let length: usize = String::from_utf8_lossy(&request)
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse().unwrap())
                    .unwrap_or(0);
                let mut request_body = vec![0; length];
                std::io::Read::read_exact(&mut stream, &mut request_body).unwrap();
                let body = concat!(
                    "{\"status\":\"pulling manifest\"}\n",
                    "{\"status\":\"pulling abc\",\"total\":100,\"completed\":50}\n",
                    "{\"status\":\"success\"}\n"
                );
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        pull(&base, "nomic-embed-text", &cancel, &mut |line| {
            seen.push(line.to_owned())
        })
        .unwrap();
        assert!(seen.iter().any(|line| line == "pulling manifest"));
        assert!(seen.iter().any(|line| line.starts_with("pulling abc: 50%")));
        let cancelled = AtomicBool::new(true);
        let error = pull(&base, "nomic-embed-text", &cancelled, &mut |_| {}).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        server.join().unwrap();
    }
}
