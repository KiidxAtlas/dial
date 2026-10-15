//! `dial config` and `dial models`: config and model commands over Dial's settings
//! store (`storage::settings_store`) and model catalog (`llm::catalog`).
//!
//! Not implemented (no Dial subsystem): `config init-xdg` (Dial stores state under
//! `DIAL_HOME`, not XDG), `models refresh` (the catalog is local discovery only), `models --kind`,
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
    /// `ls` (default), `list`, `find <pattern>`, `refresh`, or `recommend` (RAM-aware fit of
    /// downloaded local MLX models). Any other first word is a provider/substring filter for `ls`.
    pub words: Vec<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
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
        Some(_) => ModelsRoute::List(rest(0)),
    })
}

/// Credential-bearing keys. `*_env` names an environment variable, not a secret.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_env") {
        return false;
    }
    ["api_key", "apikey", "token", "secret", "password"]
        .iter()
        .any(|needle| key.contains(needle))
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
fn migrate_for_display(mut table: toml::Table) -> toml::Table {
    crate::config::migrate_memory_tool_table(&mut table);
    crate::config::migrate_edit_configuration(&mut table);
    crate::config::migrate_model_role_table(&mut table);
    crate::config::migrate_removed_settings(&mut table);
    table
}

fn migrated_layer(layout: &Layout, scope: Scope) -> Result<toml::Table> {
    Ok(migrate_for_display(settings_store::load_layer(
        layout, scope,
    )?))
}

fn merged(layout: &Layout) -> Result<toml::Table> {
    let mut table = migrated_layer(layout, Scope::Global)?;
    crate::config::merge(&mut table, migrated_layer(layout, Scope::Project)?);
    Ok(table)
}

fn scope(project: bool) -> Scope {
    if project {
        Scope::Project
    } else {
        Scope::Global
    }
}

fn display_value(key: &str, value: &toml::Value) -> String {
    if is_secret_key(key) {
        "<redacted>".to_owned()
    } else {
        value.to_string()
    }
}

/// Runs `dial config` (list when no action is given).
pub fn run_config(cwd: &Path, args: &ConfigArgs) -> Result<i32> {
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
            let entries = flatten(&merged(&layout)?);
            if args.json {
                let settings: Vec<_> = entries
                    .iter()
                    .map(|(key, value)| {
                        serde_json::json!({
                            "key": key,
                            "value": if is_secret_key(key) {
                                serde_json::Value::String("<redacted>".into())
                            } else {
                                serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
                            },
                            "redacted": is_secret_key(key),
                        })
                    })
                    .collect();
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
            let table = merged(&layout)?;
            let value =
                lookup(&table, &key).ok_or_else(|| anyhow::anyhow!("no setting at {key:?}"))?;
            if args.json {
                let shown = if is_secret_key(&key) {
                    serde_json::Value::String("<redacted>".into())
                } else {
                    serde_json::to_value(value)?
                };
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
            crate::config::schema::check_value_for(&merged(&layout)?, &key, &value)
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
    const HEADER: [&str; 7] = [
        "PROVIDER",
        "MODEL",
        "AVAILABLE",
        "VISION",
        "TOOLS",
        "REASONING",
        "MAX_OUT",
    ];
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                e.selection.provider.clone(),
                e.selection.model.clone(),
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
    let mut widths: Vec<usize> = HEADER.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let push_row = |out: &mut String, cells: &[String]| {
        let line: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}", width = *width))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    };
    let mut out = String::new();
    push_row(
        &mut out,
        &HEADER.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
    );
    for row in &rows {
        push_row(&mut out, row);
    }
    out
}

/// Runs `dial models`. `overlay` is the CLI config overlay (e.g. from `--model`).
pub fn run_models(cwd: &Path, args: &ModelsArgs, overlay: Option<toml::Table>) -> Result<i32> {
    let route = route(&args.words)?;
    let needle = match route {
        ModelsRoute::Refresh => {
            bail!("models refresh is not supported: Dial's model catalog is local discovery only")
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
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn routes_like_omp_models() {
        assert_eq!(route(&[]).unwrap(), ModelsRoute::List(None));
        assert_eq!(route(&words(&["ls"])).unwrap(), ModelsRoute::List(None));
        assert_eq!(
            route(&words(&["ls", "anthropic"])).unwrap(),
            ModelsRoute::List(Some("anthropic".into()))
        );
        assert_eq!(
            route(&words(&["openrouter"])).unwrap(),
            ModelsRoute::List(Some("openrouter".into()))
        );
        assert_eq!(
            route(&words(&["find", "qwen", "coder"])).unwrap(),
            ModelsRoute::Find("qwen coder".into())
        );
        assert!(route(&words(&["find"])).is_err());
        assert_eq!(route(&words(&["refresh"])).unwrap(), ModelsRoute::Refresh);
        assert_eq!(
            route(&words(&["recommend"])).unwrap(),
            ModelsRoute::Recommend
        );
    }

    #[test]
    fn redacts_only_credential_keys() {
        assert!(is_secret_key("endpoint.api_key"));
        assert!(is_secret_key("auth.token"));
        assert!(!is_secret_key("endpoint.api_key_env"));
        assert!(!is_secret_key("model"));
    }

    #[test]
    fn flattens_and_looks_up_dotted_keys() {
        let table: toml::Table = toml::from_str(
            "model = \"m\"\n[endpoint]\nbase_url = \"http://x\"\n[sandbox]\nmode = \"ro\"\n",
        )
        .unwrap();
        let keys: Vec<String> = flatten(&table).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["endpoint.base_url", "model", "sandbox.mode"]);
        assert_eq!(
            lookup(&table, "sandbox.mode").and_then(|v| v.as_str()),
            Some("ro")
        );
        assert!(lookup(&table, "sandbox.missing").is_none());
    }

    #[test]
    fn parses_scalars_and_falls_back_to_string() {
        assert_eq!(parse_value("true"), toml::Value::Boolean(true));
        assert_eq!(parse_value("42"), toml::Value::Integer(42));
        assert_eq!(parse_value("[1, 2]").as_array().map(Vec::len), Some(2));
        assert_eq!(
            parse_value("hello world"),
            toml::Value::String("hello world".into())
        );
    }

    #[test]
    fn table_has_header_and_aligned_rows() {
        let table = format_table(&[]);
        assert!(table.starts_with("PROVIDER"));
        assert_eq!(table.lines().count(), 1);
    }

    #[test]
    fn config_display_shows_migrated_role_keys() {
        let legacy: toml::Table = toml::from_str("[model_roles]\nslow = \"big\"\n").unwrap();
        let shown = migrate_for_display(legacy);
        assert!(lookup(&shown, "model_roles.slow").is_none());
        assert_eq!(
            lookup(&shown, "model_roles.coach"),
            Some(&toml::Value::String("big".into()))
        );
    }
}
