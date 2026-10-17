//! `dial config` and `dial models`: config and model commands over Dial's settings
//! store (`storage::settings_store`) and model catalog (`llm::catalog`).
//!
//! Not implemented (no Dial subsystem): `config init-xdg` (Dial stores state under
//! `DIAL_HOME`, not XDG), `models refresh` (the catalog is local discovery only),
//! and the `-e/--extensions`, `--no-extensions`, `--config` overlay flags (no extension model
//! registry).

use crate::cli::models_recommend;
use crate::config::types::Config;
use crate::llm::catalog::{self, ModelEntry};
use crate::llm::model_config::ModelSelection;
use crate::storage::paths::Layout;
use crate::storage::settings_store::{self, Change, Scope};
use anyhow::{Result, bail, ensure};
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args, Debug, Clone)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
    /// Emit JSON (`list`, `get`).
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// List effective settings; credential values are redacted (default).
    List,
    /// Print one setting by dotted key.
    Get { key: String },
    /// Set a setting. The value is parsed as TOML; anything that does not parse is a string.
    Set {
        key: String,
        value: String,
        /// Write the project file instead of the global file.
        #[arg(long)]
        project: bool,
    },
    /// Remove a setting from the global file (or the project file with `--project`).
    Reset {
        key: String,
        #[arg(long)]
        project: bool,
    },
    /// Show the config files and state directories.
    Path,
    /// Generate the authoritative settings reference (Markdown, or JSON with --json).
    Reference,
}

#[derive(Args, Debug, Clone)]
pub struct ModelsArgs {
    /// `ls` (default), `list`, `find <pattern>`, `refresh`, `recommend` (RAM-aware fit of
    /// downloaded local MLX models), or `status` (per-model speed, fit, adapter and gate; `--json`).
    /// Any other first word is a provider/substring filter for `ls`.
    pub words: Vec<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Filter by the model's declared purpose (unknown means no type metadata was reported).
    #[arg(long, value_parser = ["chat", "embedding", "rerank", "image", "audio", "unknown"])]
    pub kind: Option<String>,
    /// `recommend` only: token context to estimate (default: configured context, else 32768).
    #[arg(long)]
    pub context: Option<u64>,
    /// `recommend` only: quantized KV-cache bits (2, 3, 4, 6 or 8); default fp16.
    #[arg(long)]
    pub kv_bits: Option<u8>,
    /// `recommend` only: physical memory in GiB instead of detected memory.
    #[arg(long)]
    pub memory_gb: Option<f64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelsRoute {
    List(Option<String>),
    Find(String),
    Refresh,
    Recommend,
    Status,
}

/// Routes positional words for the `models` command.
pub fn route(words: &[String]) -> Result<ModelsRoute> {
    let rest = |from: usize| {
        let joined = words.get(from..).unwrap_or_default().join(" ");
        (!joined.trim().is_empty()).then(|| joined.trim().to_owned())
    };
    Ok(match words.first().map(String::as_str) {
        None => ModelsRoute::List(None),
        Some("ls" | "list") => ModelsRoute::List(rest(1)),
        Some("find") => {
            let pattern = rest(1);
            ensure!(pattern.is_some(), "models find requires a search pattern");
            ModelsRoute::Find(pattern.unwrap_or_default())
        }
        Some("refresh") => ModelsRoute::Refresh,
        Some("recommend") => ModelsRoute::Recommend,
        Some("status") => ModelsRoute::Status,
        Some(_) => ModelsRoute::List(rest(0)),
    })
}

/// Credential-bearing keys. `*_env` names an environment variable, not a secret.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    if key.ends_with("_env") {
        return false;
    }
    key.split('.').any(|part| {
        [
            "api_key",
            "apikey",
            "secret",
            "password",
            "credential",
            "authorization",
        ]
        .iter()
        .any(|needle| part.contains(needle))
            || part == "token"
            || part.ends_with("_token")
            || matches!(part, "cookie" | "cookies")
    })
}

/// Flattens nested tables to dotted keys. Arrays and scalars are leaves.
pub fn flatten(table: &toml::Table) -> Vec<(String, toml::Value)> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&path, inner, out),
                other => out.push((path, other.clone())),
            }
        }
    }
    let mut out = Vec::new();
    walk("", table, &mut out);
    out
}

/// Resolves a dotted key through nested tables.
pub fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    crate::config::schema::get(table, key)
}

/// Parses a `config set` value as a TOML scalar or inline value; anything else is a string.
pub fn parse_value(raw: &str) -> toml::Value {
    match toml::from_str::<toml::Table>(&format!("value = {raw}")) {
        Ok(mut table) => table
            .remove("value")
            .unwrap_or_else(|| toml::Value::String(raw.to_owned())),
        Err(_) => toml::Value::String(raw.to_owned()),
    }
}

/// Applies the same in-memory migrations `Config::load` applies to one layer (nothing is written),
/// so legacy keys such as retired model roles are displayed in their current form.
#[cfg(test)]
fn migrate_for_display(mut table: toml::Table) -> toml::Table {
    crate::config::migrate_memory_tool_table(&mut table);
    crate::config::migrate_edit_configuration(&mut table);
    crate::config::migrate_model_role_table(&mut table);
    crate::config::migrate_removed_settings(&mut table);
    table
}

fn merged(layout: &Layout, overlay: &toml::Table) -> Result<toml::Table> {
    // Use the runtime resolver: migrations, defaults and append/deduplicate hooks all match.
    Ok(toml::Table::try_from(settings_store::load_config(
        layout, overlay,
    )?)?)
}

fn scope(project: bool) -> Scope {
    if project {
        Scope::Project
    } else {
        Scope::Global
    }
}

/// Redact recursively, including `config get` of a whole table or an array of inline tables.
fn redacted_value(key: &str, value: &toml::Value) -> toml::Value {
    if is_secret_key(key) {
        return "<redacted>".into();
    }
    match value {
        toml::Value::Table(table) => {
            let secret_header = table
                .get("name")
                .and_then(toml::Value::as_str)
                .is_some_and(is_secret_key);
            toml::Value::Table(
                table
                    .iter()
                    .map(|(name, value)| {
                        let value = if secret_header && name == "value" {
                            "<redacted>".into()
                        } else {
                            redacted_value(&format!("{key}.{name}"), value)
                        };
                        (name.clone(), value)
                    })
                    .collect(),
            )
        }
        toml::Value::Array(values) => {
            toml::Value::Array(values.iter().map(|v| redacted_value(key, v)).collect())
        }
        other => other.clone(),
    }
}

fn display_value(key: &str, value: &toml::Value) -> String {
    let shown = redacted_value(key, value);
    if shown.as_str() == Some("<redacted>") {
        "<redacted>".into()
    } else {
        shown.to_string()
    }
}

/// Runs `dial config` (list when no action is given).
pub fn run_config(cwd: &Path, args: &ConfigArgs) -> Result<i32> {
    run_config_with_overlay(cwd, args, None)
}

pub fn run_config_with_overlay(
    cwd: &Path,
    args: &ConfigArgs,
    overlay: Option<toml::Table>,
) -> Result<i32> {
    let overlay = overlay.unwrap_or_default();
    if matches!(args.action, Some(ConfigAction::Reference)) {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::config::settings::reference())?
            );
        } else {
            print!("{}", crate::config::settings::reference_markdown());
        }
        return Ok(0);
    }
    let layout = Layout::discover(cwd)?;
    match args.action.clone().unwrap_or(ConfigAction::List) {
        ConfigAction::List => {
            let entries = flatten(&merged(&layout, &overlay)?);
            if args.json {
                let settings = entries
                    .iter()
                    .map(|(key, value)| {
                        Ok(serde_json::json!({
                            "key": key,
                            "value": serde_json::to_value(redacted_value(key, value))?,
                            "redacted": redacted_value(key, value) != *value,
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "settings": settings }))?
                );
            } else {
                for (key, value) in &entries {
                    println!("{key} = {}", display_value(key, value));
                }
            }
        }
        ConfigAction::Get { key } => {
            let table = merged(&layout, &overlay)?;
            let value =
                lookup(&table, &key).ok_or_else(|| anyhow::anyhow!("no setting at {key:?}"))?;
            if args.json {
                let shown = serde_json::to_value(redacted_value(&key, value))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "key": key, "value": shown })
                    )?
                );
            } else {
                println!("{}", display_value(&key, value));
            }
        }
        ConfigAction::Set {
            key,
            value,
            project,
        } => {
            let scope = scope(project);
            let value = parse_value(&value);
            crate::config::schema::check_value_for(&merged(&layout, &overlay)?, &key, &value)
                .map_err(anyhow::Error::msg)?;
            settings_store::save_config(
                &layout,
                scope,
                &[Change::Set {
                    path: key.clone(),
                    value,
                }],
            )?;
            println!("set {key} in {}", scope_name(scope));
        }
        ConfigAction::Reset { key, project } => {
            let scope = scope(project);
            settings_store::save_config(&layout, scope, &[Change::Remove { path: key.clone() }])?;
            println!("reset {key} in {}", scope_name(scope));
        }
        ConfigAction::Path => {
            let [global, project] = layout.config_files();
            println!("global config: {}", global.display());
            println!("project config: {}", project.display());
            println!("user dir: {}", layout.user_dir.display());
            println!("state dir: {}", layout.state_dir.display());
        }
        ConfigAction::Reference => unreachable!("reference does not load operator configuration"),
    }
    Ok(0)
}

fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global config",
        Scope::Project => "project config",
    }
}

fn selections(config: &Config) -> Vec<ModelSelection> {
    std::iter::once(config.selection())
        .chain(config.model_favorites.iter().cloned())
        .chain(config.model_roles.values().cloned())
        .collect()
}

/// Case-insensitive substring match on provider, model id, or label.
pub fn matches(entry: &ModelEntry, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    [
        entry.selection.provider.as_str(),
        entry.selection.model.as_str(),
        entry.label.as_str(),
    ]
    .iter()
    .any(|field| field.to_ascii_lowercase().contains(&needle))
}

fn tri(value: Option<bool>) -> String {
    match value {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "-".into(),
    }
}

/// Column-aligned table; cells are not truncated.
pub fn format_table(entries: &[ModelEntry]) -> String {
    const HEADER: [&str; 8] = [
        "PROVIDER",
        "MODEL",
        "KIND",
        "AVAILABLE",
        "VISION",
        "TOOLS",
        "REASONING",
        "MAX_OUT",
    ];
    let rows: Vec<[String; 8]> = entries
        .iter()
        .map(|e| {
            [
                e.selection.provider.clone(),
                e.selection.model.clone(),
                e.metadata.kind.label().to_owned(),
                tri(Some(e.available)),
                tri(e.metadata.vision),
                tri(e.metadata.tools),
                tri(e.metadata.reasoning),
                e.metadata
                    .max_output_tokens
                    .map_or_else(|| "-".into(), |n| n.to_string()),
            ]
        })
        .collect();
    super::table::render(HEADER, &rows)
}

/// Runs `dial models`. `overlay` is the CLI config overlay (e.g. from `--model`).
pub fn run_models(cwd: &Path, args: &ModelsArgs, overlay: Option<toml::Table>) -> Result<i32> {
    let route = route(&args.words)?;
    let needle = match route {
        ModelsRoute::Refresh => {
            bail!("models refresh is not supported: Dial's model catalog is local discovery only")
        }
        ModelsRoute::Status => {
            return crate::cli::local_models::run_status(cwd, args.json, overlay);
        }
        ModelsRoute::Recommend => {
            let options = models_recommend::RecommendOptions {
                context: args.context,
                kv_bits: args.kv_bits,
                memory_gb: args.memory_gb,
                json: args.json,
            };
            return models_recommend::run(cwd, &options, overlay);
        }
        ModelsRoute::List(filter) => filter,
        ModelsRoute::Find(pattern) => Some(pattern),
    };
    let layout = Layout::discover(cwd)?;
    let config = Config::load(&layout, overlay)?;
    let discovery = catalog::discover(&layout, &config.providers, &selections(&config));
    let entries: Vec<ModelEntry> = discovery
        .models
        .into_iter()
        .filter(|entry| needle.as_deref().is_none_or(|n| matches(entry, n)))
        .filter(|entry| {
            args.kind
                .as_deref()
                .is_none_or(|kind| entry.metadata.kind.label() == kind)
        })
        .collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "models": entries }))?
        );
    } else if entries.is_empty() {
        println!("no models found");
    } else {
        print!("{}", format_table(&entries));
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
