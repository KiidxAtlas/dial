//! `dial config` and `dial models`: config and model commands over Dial's settings
//! store (`config::settings_store`) and model catalog (`llm::catalog`).
//!
//! Not implemented (no Dial subsystem): `config init-xdg` (Dial stores state under
//! `DIAL_HOME`, not XDG), `models refresh` (the catalog is local discovery only),
//! and the `--config` overlay flag.

use crate::cli::models_recommend;
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::Config;
use crate::llm::catalog::{self, ModelEntry};
use crate::llm::model_config::ModelSelection;
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
    Get {
        /// Dotted setting key, e.g. `ui.theme`, or a table such as `ui`.
        key: String,
    },
    /// Set a setting. The value is parsed as TOML; anything that does not parse is a string.
    Set {
        /// Dotted setting key, e.g. `ui.theme`.
        key: String,
        /// Negative numbers are accepted as values (`config set key -1`).
        #[arg(allow_hyphen_values = true)]
        value: String,
        /// Write the project file instead of the global file.
        #[arg(long)]
        project: bool,
    },
    /// Remove a setting from the global file (or the project file with `--project`).
    Reset {
        /// Dotted setting key, e.g. `ui.theme`.
        key: String,
        /// Remove it from the project file instead of the global file.
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
    /// `ls` (default), `list`, `find <pattern>`, `recommend` (RAM-aware fit of
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

/// Checks that need more than the value's type: the theme a theme setting names must load, and a
/// status-line list may name only known segments. Only writes are checked; loading stays lenient.
pub(crate) fn check_written_value(user_dir: &Path, key: &str, value: &toml::Value) -> Result<()> {
    crate::config::schema::check_status_segments(key, value).map_err(anyhow::Error::msg)?;
    if let ("ui.theme" | "ui.theme_dark" | "ui.theme_light", Some(name)) = (key, value.as_str()) {
        crate::ui::theme::ThemeManager::check_name(user_dir, name, key == "ui.theme")
            .map_err(|error| anyhow::anyhow!("{key}: {error}"))?;
    }
    Ok(())
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
            let mut reference = crate::config::settings::reference();
            if let Some(object) = reference.as_object_mut() {
                object.insert(
                    "resources".into(),
                    serde_json::json!(crate::tools::integrations::resources::DESCRIPTORS),
                );
                object.insert(
                    "native_tools".into(),
                    crate::tools::schemas(
                        crate::config::tools::BUILTIN,
                        crate::config::types::EditMode::default(),
                    )
                    .expect("built-in schemas are valid")
                    .into(),
                );
            }
            println!("{}", serde_json::to_string_pretty(&reference)?);
        } else {
            print!("{}", crate::config::settings::reference_markdown());
        }
        return Ok(0);
    }
    let layout = Layout::discover(cwd)?;
    match args.action.clone().unwrap_or(ConfigAction::List) {
        ConfigAction::List => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "settings": setting_entries(&layout, &overlay)? })
                    )?
                );
            } else {
                for (key, value) in &flatten(&merged(&layout, &overlay)?) {
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
            } else if let toml::Value::Table(table) = value {
                // A whole table prints one `key = value` line per setting, like `config` does.
                for (name, value) in flatten(table) {
                    let name = format!("{key}.{name}");
                    println!("{name} = {}", display_value(&name, &value));
                }
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
            set_setting(&layout, &overlay, &key, parse_value(&value), scope)?;
            println!("set {key} in {}", scope_name(scope));
        }
        ConfigAction::Reset { key, project } => {
            let scope = scope(project);
            if !reset_setting(&layout, &key, scope)? {
                println!(
                    "{key} is not set in {}; nothing to reset",
                    scope_name(scope)
                );
                return Ok(0);
            }
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

/// Every effective setting as `{key, value, redacted}` with credentials redacted. Shared by
/// `dial config list --json` and the RPC `get_settings` method.
pub(crate) fn setting_entries(
    layout: &Layout,
    overlay: &toml::Table,
) -> Result<Vec<serde_json::Value>> {
    flatten(&merged(layout, overlay)?)
        .iter()
        .map(|(key, value)| {
            let shown = redacted_value(key, value);
            Ok(serde_json::json!({
                "key": key,
                "value": serde_json::to_value(&shown)?,
                "redacted": shown != *value,
            }))
        })
        .collect()
}

/// One effective setting (a scalar or a whole table), redacted like `dial config get --json`.
pub(crate) fn setting_value(
    layout: &Layout,
    overlay: &toml::Table,
    key: &str,
) -> Result<serde_json::Value> {
    let table = merged(layout, overlay)?;
    let value = lookup(&table, key).ok_or_else(|| anyhow::anyhow!("no setting at {key:?}"))?;
    Ok(serde_json::to_value(redacted_value(key, value))?)
}

/// Validates and saves one setting exactly as `dial config set` does: schema coercion and range
/// checks, write-only checks (themes, status segments), then a validated layer publication.
pub(crate) fn set_setting(
    layout: &Layout,
    overlay: &toml::Table,
    key: &str,
    value: toml::Value,
    scope: Scope,
) -> Result<()> {
    let current = merged(layout, overlay)?;
    let value = crate::config::schema::coerce_value(&current, key, value);
    crate::config::schema::check_value_for(&current, key, &value).map_err(anyhow::Error::msg)?;
    check_written_value(&layout.user_dir, key, &value)?;
    settings_store::save_config(
        layout,
        scope,
        &[Change::Set {
            path: key.to_owned(),
            value,
        }],
    )
    .map_err(|error| unknown_key_error(error, key, &current))?;
    Ok(())
}

/// Removes one setting from a layer as `dial config reset` does. Returns `false` (and writes
/// nothing) when the layer does not set it.
pub(crate) fn reset_setting(layout: &Layout, key: &str, scope: Scope) -> Result<bool> {
    if lookup(&settings_store::load_layer(layout, scope)?, key).is_none() {
        return Ok(false);
    }
    settings_store::save_config(
        layout,
        scope,
        &[Change::Remove {
            path: key.to_owned(),
        }],
    )?;
    Ok(true)
}

/// Replaces the configuration loader's "unknown field" error (which lists every field of the
/// table) with a short message naming the key and the closest known setting.
fn unknown_key_error(error: anyhow::Error, key: &str, current: &toml::Table) -> anyhow::Error {
    // Only an unknown field named by this key: an unrelated defect elsewhere keeps its own error.
    let text = format!("{error:#}");
    if !key
        .split('.')
        .any(|part| text.contains(&format!("unknown field `{part}`")))
    {
        return error;
    }
    let known: Vec<String> = crate::config::schema::registry()
        .iter()
        .map(|def| def.path.to_owned())
        .chain(flatten(current).into_iter().map(|(name, _)| name))
        .collect();
    match closest_key(key, &known) {
        Some(near) => anyhow::anyhow!(
            "unknown setting {key:?}; did you mean {near:?}? `dial config` lists every setting"
        ),
        None => anyhow::anyhow!("unknown setting {key:?}; `dial config` lists every setting"),
    }
}

/// The known key nearest to `key`: an exact case-insensitive match, a key ending in `.key`, or
/// the smallest edit distance within a quarter of the key's length (at least 2).
fn closest_key<'a>(key: &str, known: &'a [String]) -> Option<&'a str> {
    let lower = key.to_ascii_lowercase();
    if let Some(exact) = known.iter().find(|name| name.to_ascii_lowercase() == lower) {
        return Some(exact);
    }
    let suffix = format!(".{lower}");
    if let Some(tail) = known
        .iter()
        .find(|name| name.to_ascii_lowercase().ends_with(&suffix))
    {
        return Some(tail);
    }
    let limit = (lower.chars().count() / 4).max(2);
    known
        .iter()
        .map(|name| (edit_distance(&lower, &name.to_ascii_lowercase()), name))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.as_str())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
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

/// The name to show for a model: a downloaded local model's path becomes its short label
/// (`mlx-community/Qwen2.5-7B-Instruct-4bit`), which `--model` resolves back to the same weights.
/// `--json` keeps the full path.
fn shown_model(entry: &ModelEntry) -> String {
    let model = &entry.selection.model;
    if Path::new(model).is_absolute() && !entry.label.trim().is_empty() {
        entry.label.clone()
    } else {
        model.clone()
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
                shown_model(e),
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
            bail!(
                "there is nothing to refresh: `dial models` discovers models each time it runs; run it again for the current list"
            )
        }
        ModelsRoute::Status => {
            return crate::stats::local_models::run_status(cwd, args.json, overlay);
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
