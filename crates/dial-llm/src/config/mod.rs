//! Configuration loading, layering and validation; the shared data types live in [`types`].

pub mod adaptive;
pub mod agent_defs;
pub mod ask;
pub mod checkpoint;
pub mod collab;
pub mod compaction;
pub mod embeddings;
pub mod hooks;
pub mod isolation;
pub mod laya;
pub mod memory_backend;
pub mod paths;
pub mod reliability;
pub mod schema;
pub mod selection;
pub mod session_models;
pub mod settings;
pub mod settings_store;
pub mod share;
pub mod snapcompact;
pub mod solve;
pub mod stats;
pub mod steering;
pub mod subagents;
pub mod task;
pub mod telemetry;
pub mod thinking;
pub mod tools;
pub mod types;
pub mod voice;

use crate::config::{
    paths::Layout,
    settings_store::{locked, write_private},
    tools::{BUILTIN, EXTRA_TOOLS, integrations::INTEGRATION_TOOLS},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
pub use types::{Config, Harness};

/// Provider request tiers accepted by `service_tier` and `--service-tier`. `none` omits the field.
pub const SERVICE_TIERS: &[&str] = &[
    "none",
    "auto",
    "default",
    "flex",
    "scale",
    "priority",
    "ultrafast",
];

/// Deep-merge `over` into `base`; tables merge key by key, everything else is replaced.
pub fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(a)), toml::Value::Table(b)) => merge(a, b),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Like `merge`, but the top-level `hooks` array appends to the hooks already configured (identical
/// hooks are kept once), so a launch overlay such as `--hook` adds hooks instead of dropping the
/// project's. Every other value is merged as in `merge`.
pub fn merge_overlay(base: &mut toml::Table, mut over: toml::Table) {
    let hooks = over.remove("hooks");
    merge(base, over);
    if let Some(toml::Value::Array(added)) = hooks {
        match base.get_mut("hooks") {
            Some(toml::Value::Array(existing)) => {
                for hook in added {
                    if !existing.contains(&hook) {
                        existing.push(hook);
                    }
                }
            }
            _ => {
                base.insert("hooks".into(), toml::Value::Array(added));
            }
        }
    }
}

/// Global settings, then project settings, then the command-line overlay, before loading.
pub fn merged_layers(
    global: &toml::Table,
    project: &toml::Table,
    overlay: &toml::Table,
) -> toml::Table {
    let mut table = global.clone();
    merge_overlay(&mut table, project.clone());
    merge_overlay(&mut table, overlay.clone());
    table
}

fn legacy_chatgpt_api_url(url: &str) -> bool {
    matches!(
        url,
        "https://api.openai.com/v1" | "https://api.openai.com/v1/"
    )
}

/// Rewrite only removed memory selectors, preserving every unrelated TOML value.
/// Preview/validation callers may use this without writing configuration files.
pub fn migrate_memory_tool_table(table: &mut toml::Table) -> bool {
    fn migrate(value: Option<&mut toml::Value>) -> bool {
        let Some(values) = value.and_then(toml::Value::as_array_mut) else {
            return false;
        };
        if !values.iter().any(|value| value.as_str() == Some("memory"))
            || !values.iter().all(|value| value.as_str().is_some())
        {
            return false;
        }
        let mut names: Vec<String> = std::mem::take(values)
            .into_iter()
            .map(|value| match value {
                toml::Value::String(name) => name,
                _ => unreachable!("string array checked above"),
            })
            .collect();
        types::migrate_memory_tool_names(&mut names);
        *values = names.into_iter().map(toml::Value::String).collect();
        true
    }
    let mut changed = migrate(
        table
            .get_mut("harness")
            .and_then(toml::Value::as_table_mut)
            .and_then(|harness| harness.get_mut("tools")),
    );
    if let Some(profiles) = table
        .get_mut("model_profiles")
        .and_then(toml::Value::as_table_mut)
    {
        for (_, value) in profiles.iter_mut() {
            let Some(profile) = value.as_table_mut() else {
                continue;
            };
            changed |= migrate(profile.get_mut("tools"));
        }
    }
    changed
}
pub fn migrate_edit_configuration(table: &mut toml::Table) -> bool {
    let Some(mode) = table.remove("edit_mode") else {
        return false;
    };
    let edit = table
        .entry("edit")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if let Some(edit) = edit.as_table_mut() {
        edit.entry("mode").or_insert(mode);
    }
    true
}

/// Drops settings whose features were removed so saved configuration from older releases still loads
/// under `deny_unknown_fields`: the sidecar memory backend's settings and its `memory_edit` tool, memory
/// role fields that named the embedding role (an embedding model cannot extract or consolidate),
/// skill install scripts, the browser relay and the automatic reliability check. The retired `mnemopi`
/// backend is translated to `semantic`, its successor, rather than dropped. Unrelated values are preserved.
/// Returns whether anything changed.
pub fn migrate_removed_settings(table: &mut toml::Table) -> bool {
    fn table_mut<'a>(table: &'a mut toml::Table, key: &str) -> Option<&'a mut toml::Table> {
        table.get_mut(key).and_then(toml::Value::as_table_mut)
    }
    /// Removes `memory_edit` tool names from string arrays.
    fn drop_removed_items(table: &mut toml::Table) -> bool {
        let mut changed = false;
        for (_, value) in table.iter_mut() {
            match value {
                toml::Value::Table(child) => changed |= drop_removed_items(child),
                toml::Value::Array(items) => {
                    let before = items.len();
                    items.retain(|item| !matches!(item.as_str(), Some("memory_edit")));
                    changed |= items.len() != before;
                }
                _ => {}
            }
        }
        changed
    }
    let mut changed = drop_removed_items(table);
    if let Some(memory) = table_mut(table, "memory") {
        changed |= memory.remove("mnemopi").is_some();
        if memory.get("backend").and_then(toml::Value::as_str) == Some("mnemopi") {
            memory.insert(
                "backend".to_owned(),
                toml::Value::String("semantic".to_owned()),
            );
            changed = true;
        }
        // A memory role field naming the removed role falls back to its default ("memory").
        for name in ["local", "sharpshooter"] {
            let Some(backend) = table_mut(memory, name) else {
                continue;
            };
            for field in ["extraction_role", "consolidation_role", "model_role"] {
                if backend.get(field).and_then(toml::Value::as_str) == Some("embedding") {
                    backend.remove(field);
                    changed = true;
                }
            }
        }
    }
    if let Some(reliability) = table_mut(table, "reliability") {
        changed |= reliability.remove("auto_check").is_some();
    }
    if let Some(harness) = table_mut(table, "harness") {
        changed |= harness.remove("allow_skill_install_scripts").is_some();
    }
    if let Some(browser) = table_mut(table, "browser") {
        changed |= browser.remove("tern").is_some();
    }
    // `ui.worker_shards` (bool, default on) became `ui.pinned_workers`: on maps to the default
    // `collapsed` block and off to `off`. An explicit `pinned_workers` value wins.
    if let Some(ui) = table_mut(table, "ui")
        && let Some(value) = ui.remove("worker_shards")
    {
        if value.as_bool() == Some(false) && !ui.contains_key("pinned_workers") {
            ui.insert(
                "pinned_workers".to_owned(),
                toml::Value::String("off".to_owned()),
            );
        }
        changed = true;
    }
    changed
}

/// A semantic embedding override set on one side only, across the global and project layers:
/// `(the set key, its value)`. The pre-role Settings picker wrote `embedding_model` alone.
fn lone_embedding_override(layers: &[toml::Table; 2]) -> Option<(&'static str, String)> {
    let value = |key: &str| {
        layers.iter().rev().find_map(|layer| {
            layer
                .get("memory")?
                .get("semantic")?
                .get(key)?
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
    };
    match (value("embedding_base_url"), value("embedding_model")) {
        (None, Some(model)) => Some(("embedding_model", model)),
        (Some(url), None) => Some(("embedding_base_url", url)),
        _ => None,
    }
}

/// Removes `memory.semantic.<key>`; returns whether it was present.
fn drop_semantic_key(table: &mut toml::Table, key: &str) -> bool {
    table
        .get_mut("memory")
        .and_then(toml::Value::as_table_mut)
        .and_then(|memory| memory.get_mut("semantic"))
        .and_then(toml::Value::as_table_mut)
        .is_some_and(|semantic| semantic.remove(key).is_some())
}

/// Current roles that a legacy `default` role fills when they are not otherwise assigned. `decision` is the
/// Laya checkpoint, so it is not filled.
const DEFAULT_FILLS: &[&str] = &["task", "small", "coach", "tester", "planner", "memory"];

/// The current role a legacy role name maps to, if it is one.
fn legacy_role_target(name: &str) -> Option<&'static str> {
    crate::llm::model_config::LEGACY_ROLE_MIGRATIONS
        .iter()
        .find(|(legacy, _)| *legacy == name)
        .map(|(_, role)| *role)
}

/// Rewrites `@legacy` role references in string arrays (model lists, fallback chains).
fn rewrite_legacy_role_refs(table: &mut toml::Table) -> bool {
    let mut changed = false;
    for (_, value) in table.iter_mut() {
        match value {
            toml::Value::Table(child) => changed |= rewrite_legacy_role_refs(child),
            toml::Value::Array(items) => {
                for item in items.iter_mut() {
                    let Some(role) = item.as_str().and_then(|text| text.strip_prefix('@')) else {
                        continue;
                    };
                    if let Some(target) = legacy_role_target(role).filter(|target| *target != role)
                    {
                        *item = toml::Value::String(format!("@{target}"));
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

/// Renames legacy role names stored in memory backend role fields to their current role.
fn rewrite_memory_role_fields(table: &mut toml::Table) -> bool {
    const FIELDS: &[[&str; 3]] = &[
        ["memory", "local", "extraction_role"],
        ["memory", "local", "consolidation_role"],
        ["memory", "sharpshooter", "model_role"],
    ];
    let mut changed = false;
    for [root, backend, field] in FIELDS {
        let Some(value) = table
            .get_mut(*root)
            .and_then(toml::Value::as_table_mut)
            .and_then(|memory| memory.get_mut(*backend))
            .and_then(toml::Value::as_table_mut)
            .and_then(|backend| backend.get_mut(*field))
        else {
            continue;
        };
        let target = match value.as_str() {
            // `decision` is the Laya checkpoint now; a Sharpshooter model role named `decision` meant memory.
            Some("decision") => Some("memory"),
            Some(text) => legacy_role_target(text),
            None => None,
        }
        .filter(|target| value.as_str() != Some(*target));
        if let Some(target) = target {
            *value = toml::Value::String(target.to_owned());
            changed = true;
        }
    }
    changed
}

/// Migrates legacy `model_roles` keys to the current roles (see `LEGACY_ROLE_MIGRATIONS`).
/// Precedence: an explicit current key wins; among legacy keys for one role the first in
/// `LEGACY_ROLE_MIGRATIONS` wins; `default` fills unassigned roles in `DEFAULT_FILLS`; `vision` is
/// dropped. Also removes the retired `escalation.role` (escalation always uses coach) and rewrites
/// `@legacy` references. Unknown keys are left for validation to report. Returns whether anything changed.
pub fn migrate_model_role_table(table: &mut toml::Table) -> bool {
    let mut changed = rewrite_legacy_role_refs(table) | rewrite_memory_role_fields(table);
    if let Some(escalation) = table
        .get_mut("escalation")
        .and_then(toml::Value::as_table_mut)
    {
        changed |= escalation.remove("role").is_some();
    }
    let Some(roles) = table
        .get_mut("model_roles")
        .and_then(toml::Value::as_table_mut)
    else {
        return changed;
    };
    // `decision` is the Laya checkpoint (provider `laya`). Any other `decision` value was the old Sharpshooter
    // chat model, which now belongs to `memory`.
    if let Some(value) = roles.get("decision").cloned()
        && !matches!(
            value
                .as_table()
                .and_then(|table| table.get("provider"))
                .and_then(toml::Value::as_str),
            Some(crate::config::laya::PROVIDER | "inherit")
        )
    {
        roles.remove("decision");
        roles.entry("memory".to_owned()).or_insert(value);
    }
    let before = roles.clone();
    let mut migrated = toml::Table::new();
    for (legacy, target) in crate::llm::model_config::LEGACY_ROLE_MIGRATIONS {
        // Current-role identities are not migrations: preserve them in place, including
        // explicit selections that must win over legacy aliases for the same role.
        if legacy == target {
            continue;
        }
        if let Some(value) = roles.remove(*legacy) {
            migrated.entry((*target).to_owned()).or_insert(value);
        }
    }
    roles.remove("vision");
    if let Some(default) = roles.remove("default") {
        for role in DEFAULT_FILLS {
            migrated
                .entry((*role).to_owned())
                .or_insert_with(|| default.clone());
        }
    }
    for (role, value) in migrated {
        roles.entry(role).or_insert(value);
    }
    changed | (*roles != before)
}

impl Config {
    /// Coding has no implicit learning lifecycle, even with preferences saved by older releases.
    /// The one opt-in is "Teach this model" ([`Self::teaching`]): verified turns of a local model
    /// are recorded and the latest admitted version serves. Background cycles stay off either way.
    /// This is a runtime view only; persisted learning configuration remains available to `dial improve`.
    pub fn for_coding(mut self) -> Self {
        if self.teaching() {
            self.improve.enabled = true;
            self.improve.learn_from_sessions = true;
        } else {
            self.improve.enabled = false;
            self.improve.learn_from_sessions = false;
            self.training.enabled = false;
        }
        self.improve.background = false;
        self.assistance.teacher_enabled = false;
        self.codebase.synthesize_architecture = false;
        self.laya.enabled = false;
        self.memory.autolearn = false;
        self.memory.autolearn_auto_continue = false;
        self
    }

    /// Layered configuration: `<user>/config.toml`, then `<state>/config.toml` (per project), then
    /// `overlay` (command-line flags). Both files live in private storage, excluded from agent
    /// tools, so the agent cannot edit its own sandbox, approval or gate settings.
    pub fn load(layout: &Layout, overlay: Option<toml::Table>) -> Result<Self> {
        layout.ensure_project_outside_storage()?;
        let mut layers: [toml::Table; 2] = Default::default();
        let mut migrations = Vec::new();
        for (index, path) in layout.config_files().into_iter().enumerate() {
            let Some(bytes) = crate::platform::atomic::read_private(&path)
                .with_context(|| format!("cannot read configuration {}", path.display()))?
            else {
                continue;
            };
            let text = std::str::from_utf8(&bytes)
                .with_context(|| format!("configuration is not UTF-8: {}", path.display()))?;
            let mut part: toml::Table = toml::from_str(text)
                .with_context(|| format!("invalid configuration {}", path.display()))?;
            if migrate_memory_tool_table(&mut part)
                | migrate_edit_configuration(&mut part)
                | migrate_model_role_table(&mut part)
                | migrate_removed_settings(&mut part)
            {
                migrations.push(path);
            }
            layers[index] = part;
        }
        // A half embedding override never worked and no longer validates; drop it with a notice so
        // saved settings from the old Settings picker still load.
        let mut half_override = Vec::new();
        if let Some((key, value)) = lone_embedding_override(&layers) {
            for (index, path) in layout.config_files().into_iter().enumerate() {
                if drop_semantic_key(&mut layers[index], key) {
                    if !migrations.contains(&path) {
                        migrations.push(path.clone());
                    }
                    half_override.push((path, key));
                }
            }
            eprintln!(
                "memory.semantic.{key} = {value:?} was set without its partner ({}), so it was never used; removed it. Assign the embedding role (Alt+M) or run /setup embeddings.",
                if key == "embedding_model" {
                    "embedding_base_url"
                } else {
                    "embedding_model"
                }
            );
        }
        let config =
            Self::from_layers(layout, &layers[0], &layers[1], &overlay.unwrap_or_default())?;
        // Migrate persisted sources only after the complete layered configuration validates.
        // Re-read under the same lock used by the settings editor to preserve concurrent edits.
        for path in migrations {
            let _lock = locked(&path)?;
            let Some(bytes) = crate::platform::atomic::read_private(&path)? else {
                continue;
            };
            let mut current: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)
                .with_context(|| format!("invalid configuration {}", path.display()))?;
            let half = half_override
                .iter()
                .find(|(dropped, _)| *dropped == path)
                .map(|(_, key)| *key);
            if migrate_memory_tool_table(&mut current)
                | migrate_edit_configuration(&mut current)
                | migrate_model_role_table(&mut current)
                | migrate_removed_settings(&mut current)
                | half.is_some_and(|key| drop_semantic_key(&mut current, key))
            {
                write_private(&path, toml::to_string_pretty(&current)?.as_bytes())?;
                eprintln!(
                    "Migrated configuration {} to the current settings format.",
                    path.display()
                );
            }
        }
        Ok(config)
    }

    /// Resolve an already-merged table; used to validate both edits and removals before saving.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_table(layout: &Layout, table: toml::Table) -> Result<Self> {
        let global = crate::config::settings_store::load_layer(
            layout,
            crate::config::settings_store::Scope::Global,
        )?;
        Self::from_table_with_policy(layout, table, &global)
    }

    pub fn from_layers(
        layout: &Layout,
        global: &toml::Table,
        project: &toml::Table,
        overlay: &toml::Table,
    ) -> Result<Self> {
        Self::from_session_layers(layout, global, project, overlay, None)
    }

    /// [`Self::from_layers`] with a running session's own model choices replacing the merged
    /// model keys, so another process's model change never retargets this session.
    pub fn from_session_layers(
        layout: &Layout,
        global: &toml::Table,
        project: &toml::Table,
        overlay: &toml::Table,
        models: Option<&session_models::SessionModels>,
    ) -> Result<Self> {
        ensure!(
            !project.contains_key("chatgpt_accounts") && !overlay.contains_key("chatgpt_accounts"),
            "chatgpt_accounts is a private global operator policy"
        );
        let mut table = merged_layers(global, project, overlay);
        if let Some(models) = models {
            models.apply(&mut table);
        }
        let mut config = Self::from_table_with_policy(layout, table, global)?;
        if let Some(mode) = overlay
            .get("edit")
            .and_then(toml::Value::as_table)
            .and_then(|edit| edit.get("mode"))
        {
            config.edit_mode_override =
                Some(mode.clone().try_into().context("invalid --edit-mode")?);
        }
        Ok(config)
    }

    fn from_table_with_policy(
        layout: &Layout,
        mut table: toml::Table,
        global: &toml::Table,
    ) -> Result<Self> {
        ensure!(
            !table.contains_key("edit_mode"),
            "edit_mode moved to [edit] mode; load saved configuration to migrate it"
        );
        ensure!(
            table.get("chatgpt_accounts").is_none()
                || table.get("chatgpt_accounts") == global.get("chatgpt_accounts"),
            "chatgpt_accounts is a private global operator policy"
        );
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Policy {
            allowed: BTreeSet<String>,
        }
        let allowed = global
            .get("chatgpt_accounts")
            .map(|v| {
                v.clone()
                    .try_into::<Policy>()
                    .context("invalid global chatgpt_accounts policy")
            })
            .transpose()?
            .map(|p| p.allowed);
        if let Some(ids) = &allowed {
            ensure!(
                ids.iter().all(|id| !id.trim().is_empty()
                    && id.len() <= 256
                    && id.bytes().all(|b| b.is_ascii_graphic())),
                "chatgpt_accounts.allowed requires safe nonempty account IDs"
            );
        }
        let mut pins = allowed.clone().unwrap_or_default();
        if allowed.is_none() {
            if global.get("provider").and_then(toml::Value::as_str) == Some("chatgpt")
                && let Some(id) = global
                    .get("endpoint")
                    .and_then(|v| v.get("account_id"))
                    .and_then(toml::Value::as_str)
            {
                pins.insert(id.to_owned());
            }
            for key in ["model_roles", "model_favorites"] {
                let values: Vec<&toml::Value> = match global.get(key) {
                    Some(toml::Value::Table(rows)) => rows.values().collect(),
                    Some(toml::Value::Array(rows)) => rows.iter().collect(),
                    _ => Vec::new(),
                };
                for row in values {
                    if row.get("provider").and_then(toml::Value::as_str) == Some("chatgpt")
                        && let Some(id) = row.get("account_id").and_then(toml::Value::as_str)
                    {
                        pins.insert(id.to_owned());
                    }
                }
            }
        }
        table.remove("chatgpt_accounts");
        layout.ensure_project_outside_storage()?;
        let mut config: Config = toml::Value::Table(table)
            .try_into()
            .context("invalid configuration")?;
        if let Some(mode) = std::env::var_os("DIAL_EDIT_VARIANT") {
            let mode = mode
                .into_string()
                .map_err(|_| anyhow::anyhow!("DIAL_EDIT_VARIANT must be UTF-8"))?;
            if mode != "auto" {
                config.edit_mode_override = Some(
                    toml::Value::String(mode)
                        .try_into()
                        .context("invalid DIAL_EDIT_VARIANT")?,
                );
            }
        }
        if let Ok(value) = std::env::var("DIAL_EDIT_FUZZY") {
            match value.as_str() {
                "1" | "true" => config.edit.fuzzy_match = true,
                "0" | "false" => config.edit.fuzzy_match = false,
                _ => {}
            }
        }
        if let Some(value) = std::env::var_os("DIAL_EDIT_FUZZY_THRESHOLD") {
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("DIAL_EDIT_FUZZY_THRESHOLD must be UTF-8"))?;
            config.edit.fuzzy_threshold =
                value.parse().context("invalid DIAL_EDIT_FUZZY_THRESHOLD")?;
        }
        config.chatgpt_allowed_accounts = allowed;
        config.chatgpt_pinned_accounts = pins;
        if let Some(endpoint) = &mut config.endpoint {
            endpoint.account_id = endpoint.account_id.take().filter(|id| !id.is_empty());
        }
        for selection in config
            .model_roles
            .values_mut()
            .chain(config.model_favorites.iter_mut())
        {
            selection.account_id = selection.account_id.take().filter(|id| !id.is_empty());
        }
        ensure!(
            config
                .endpoint
                .as_ref()
                .is_none_or(|e| e.account_id.is_none())
                || config.provider.as_deref() == Some("chatgpt"),
            "account_id is supported only for ChatGPT; Anthropic has one account"
        );
        config.model_profile = config
            .model_profile
            .take()
            .filter(|name| !name.trim().is_empty());
        types::migrate_memory_tools(&mut config.harness);
        for profile in config.model_profiles.values_mut() {
            if let Some(tools) = &mut profile.tools {
                types::migrate_memory_tool_names(tools);
            }
        }
        if matches!(
            config.provider.as_deref(),
            Some("mlx" | crate::llm::apple::PROVIDER)
        ) {
            config.endpoint = None;
        } else if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER)
            && config.endpoint.is_none()
        {
            config.endpoint = Some(types::EndpointConfig {
                headers: None,
                base_url: crate::llm::chatgpt::BASE_URL.into(),
                api_key_env: None,
                allow_insecure: false,
                protocol: None,
                account_id: None,
            });
        } else if config.endpoint.is_none()
            && let Some(spec) = config
                .provider
                .as_ref()
                .and_then(|id| config.providers.get(id))
        {
            config.endpoint = Some(types::EndpointConfig {
                headers: None,
                base_url: spec.base_url.clone(),
                api_key_env: spec.api_key_env.clone().or_else(|| {
                    config
                        .provider
                        .as_deref()
                        .and_then(crate::llm::catalog::provider_endpoint)
                        .and_then(|(_, env, _)| env.map(str::to_owned))
                }),
                allow_insecure: spec.allow_insecure,
                protocol: spec.protocol,
                account_id: None,
            });
        }
        if config.endpoint.is_none()
            && let Some(provider) = config.provider.as_deref()
            && let Some((url, env, protocol)) = crate::llm::catalog::provider_endpoint(provider)
        {
            config.endpoint = Some(types::EndpointConfig {
                headers: None,
                base_url: url.into(),
                api_key_env: env.map(str::to_owned),
                allow_insecure: false,
                protocol: Some(protocol),
                account_id: None,
            });
        }
        if config.endpoint.is_none() && config.provider.as_deref() == Some("ollama") {
            config.endpoint = Some(types::EndpointConfig {
                headers: None,
                base_url: "http://127.0.0.1:11434".into(),
                api_key_env: None,
                allow_insecure: false,
                protocol: Some(crate::llm::model_config::ApiProtocol::Ollama),
                account_id: None,
            });
        }
        if let Some(endpoint) = &mut config.endpoint
            && endpoint.protocol.is_none()
        {
            endpoint.protocol = config
                .provider
                .as_ref()
                .and_then(|id| config.providers.get(id))
                .and_then(|p| p.protocol);
        }
        if let Some(endpoint) = &mut config.endpoint {
            endpoint.api_key_env = endpoint
                .api_key_env
                .take()
                .filter(|name| !name.trim().is_empty());
        }
        if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER)
            && let Some(endpoint) = config.endpoint.as_mut()
            && legacy_chatgpt_api_url(&endpoint.base_url)
            && endpoint.api_key_env.is_none()
        {
            endpoint.base_url = crate::llm::chatgpt::BASE_URL.into();
        }
        config
            .model_roles
            .retain(|_, selection| selection.provider != "inherit");
        for selection in config.model_roles.values_mut() {
            if selection.provider == "mlx" || selection.provider == crate::llm::apple::PROVIDER {
                selection.base_url = None;
                selection.api_key_env = None;
                selection.allow_insecure = false;
            } else if selection.provider == crate::llm::chatgpt::PROVIDER
                && selection.base_url.is_none()
            {
                selection.base_url = Some(crate::llm::chatgpt::BASE_URL.into());
            }
            if let Some(spec) = config.providers.get(&selection.provider) {
                if selection.base_url.is_none() {
                    selection.base_url = Some(spec.base_url.clone());
                    selection.api_key_env = selection
                        .api_key_env
                        .clone()
                        .or_else(|| spec.api_key_env.clone());
                    selection.allow_insecure = spec.allow_insecure;
                }
                selection.protocol = selection.protocol.or(spec.protocol);
            }
            if selection.base_url.is_none() {
                if let Some((url, env, protocol)) =
                    crate::llm::catalog::provider_endpoint(&selection.provider)
                {
                    selection.base_url = Some(url.to_owned());
                    selection.api_key_env = selection
                        .api_key_env
                        .clone()
                        .or_else(|| env.map(str::to_owned));
                    selection.protocol = selection.protocol.or(Some(protocol));
                } else if selection.provider == "ollama" {
                    selection.base_url = Some("http://127.0.0.1:11434".into());
                    selection.protocol = selection
                        .protocol
                        .or(Some(crate::llm::model_config::ApiProtocol::Ollama));
                }
            }
            selection.api_key_env = selection
                .api_key_env
                .take()
                .filter(|name| !name.trim().is_empty());
            if selection.provider == crate::llm::chatgpt::PROVIDER
                && selection.api_key_env.is_none()
                && selection
                    .base_url
                    .as_deref()
                    .is_some_and(legacy_chatgpt_api_url)
            {
                selection.base_url = Some(crate::llm::chatgpt::BASE_URL.into());
            }
        }
        for (id, provider) in &mut config.providers {
            provider.api_key_env = provider
                .api_key_env
                .take()
                .filter(|name| !name.trim().is_empty());
            if id == crate::llm::chatgpt::PROVIDER
                && provider.api_key_env.is_none()
                && legacy_chatgpt_api_url(&provider.base_url)
            {
                provider.base_url = crate::llm::chatgpt::BASE_URL.into();
            }
        }
        config.project_root = layout.project_root.clone();
        config.user_dir = layout.user_dir.clone();
        let mut selection = config.selection();
        selection.context_window = config.context_limit.tokens();
        config.context_window = config.resolve_context_window(&selection);
        let python = layout.user_dir.join("python");
        if config.python.as_os_str().is_empty() {
            config.python = python.join(".venv/bin/python");
        }
        if config.worker.as_os_str().is_empty() {
            config.worker = python.join("dial_worker.py");
        }
        if config.use_case.trim().is_empty() {
            config.use_case = layout
                .project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".into());
        }
        if config.endpoint.is_none() && !config.model.is_empty() {
            let candidate = Path::new(&config.model);
            if candidate.exists() {
                config.model = fs::canonicalize(candidate)?.to_string_lossy().into_owned();
            }
        }
        // A named profile is a selector scoped to one exact provider/model. When settings layers or a
        // CLI/model-picker change leave that selector pointing at another model, ignore the stale
        // selector and use the normal harness; keep the profile definition for when its model is chosen again.
        if config.model_profile.as_ref().is_some_and(|name| {
            !config.model_profiles.get(name).is_some_and(|profile| {
                profile.provider == config.selection().provider
                    && profile.model == config.selection().model
            })
        }) {
            config.model_profile = None;
        }
        config.state_dir = config.learning_dir(layout);
        let grounded = config.state_dir.join("grounded");
        if config.training.train_data.as_os_str().is_empty() {
            config.training.train_data = grounded.join("train.jsonl");
        }
        if config.training.valid_data.as_os_str().is_empty() {
            config.training.valid_data = grounded.join("valid.jsonl");
        }
        let resolve = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = layout.state_dir.join(&*p);
            }
        };
        resolve(&mut config.training.train_data);
        resolve(&mut config.training.valid_data);
        if let Some(gate) = &mut config.gate {
            for suite in &mut gate.suites {
                resolve(&mut suite.path);
            }
        }
        for tool in config.tools.values_mut() {
            // A bare command name (`mylint`) is a PATH lookup, not a file under the state directory.
            if tool.program.components().count() > 1 {
                resolve(&mut tool.program);
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn require_model(&self) -> Result<()> {
        ensure!(
            !self.model.trim().is_empty(),
            "no model configured: run `dial setup --model <mlx-model>` or pass --model (add [endpoint] for an OpenAI-compatible server)"
        );
        Ok(())
    }

    /// Model shown for the coach role (self-improvement feedback and escalation); the session model when unassigned.
    pub fn coach_model(&self) -> &str {
        self.model_roles
            .get("coach")
            .map(|m| m.model.as_str())
            .unwrap_or(&self.model)
    }

    /// Provider/model/use-case-specific histories isolate adapters when the user switches models.
    pub fn learning_dir(&self, layout: &Layout) -> PathBuf {
        use sha2::{Digest, Sha256};
        let mut identity = serde_json::json!([
            self.model,
            self.endpoint
                .as_ref()
                .map(|e| e.base_url.trim_end_matches('/')),
            self.use_case
        ]);
        let builtin = crate::llm::model_config::ApiProtocol::builtin(
            self.provider.as_deref().unwrap_or("endpoint"),
        );
        if let Some(endpoint) = &self.endpoint {
            let protocol = endpoint.protocol.unwrap_or(builtin);
            if protocol != builtin
                || matches!(
                    protocol,
                    crate::llm::model_config::ApiProtocol::Gemini
                        | crate::llm::model_config::ApiProtocol::Ollama
                )
            {
                identity
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!(protocol));
            }
        }
        let digest = format!("{:x}", Sha256::digest(identity.to_string().as_bytes()));
        layout.state_dir.join("models").join(&digest[..16])
    }

    pub fn selection(&self) -> crate::llm::model_config::ModelSelection {
        let provider = self.provider.clone().unwrap_or_else(|| {
            self.endpoint
                .as_ref()
                .map(|endpoint| {
                    self.providers
                        .iter()
                        .find(|(_, p)| {
                            p.base_url.trim_end_matches('/')
                                == endpoint.base_url.trim_end_matches('/')
                        })
                        .map(|(id, _)| id.clone())
                        .unwrap_or_else(|| "endpoint".into())
                })
                .unwrap_or_else(|| "mlx".into())
        });
        crate::llm::model_config::ModelSelection {
            provider,
            model: self.model.clone(),
            base_url: self.endpoint.as_ref().map(|e| e.base_url.clone()),
            api_key_env: self.endpoint.as_ref().and_then(|e| e.api_key_env.clone()),
            allow_insecure: self.endpoint.as_ref().is_some_and(|e| e.allow_insecure),
            protocol: self.endpoint.as_ref().and_then(|e| e.protocol),
            headers: self.endpoint.as_ref().and_then(|e| e.headers.clone()),
            account_id: self.endpoint.as_ref().and_then(|e| e.account_id.clone()),
            context_window: Some(self.context_window),
        }
    }

    /// The request tier sent for this config. An explicit `service_tier` wins; otherwise `fast_mode`
    /// selects `priority`, then `slow_mode` selects `flex`; `none` sends no tier.
    pub fn effective_service_tier(&self) -> &str {
        if self.service_tier != "none" {
            &self.service_tier
        } else if self.fast_mode {
            "priority"
        } else if self.slow_mode {
            "flex"
        } else {
            "none"
        }
    }

    pub fn same_model_identity(&self, other: &Self) -> bool {
        self.model == other.model
            && self.provider == other.provider
            && self.endpoint == other.endpoint
            && self.context_window == other.context_window
            && self.request_headers() == other.request_headers()
    }

    pub fn selection_base_url<'a>(
        &'a self,
        selection: &'a crate::llm::model_config::ModelSelection,
    ) -> Option<std::borrow::Cow<'a, str>> {
        selection
            .base_url
            .as_deref()
            .map(std::borrow::Cow::Borrowed)
            .or_else(|| self.provider_base_url(&selection.provider))
    }

    fn provider_base_url(&self, provider: &str) -> Option<std::borrow::Cow<'_, str>> {
        use std::borrow::Cow;
        self.providers
            .get(provider)
            .map(|p| Cow::Borrowed(p.base_url.as_str()))
            .or(match provider {
                "chatgpt" => Some(Cow::Borrowed(crate::llm::chatgpt::BASE_URL)),
                "ollama" => Some(Cow::Borrowed("http://127.0.0.1:11434")),
                _ => None,
            })
            .or_else(|| {
                // Templated bundled endpoints take their account from config. Unset values stay as
                // placeholders; provider::endpoint_url refuses the request and names the setting.
                crate::llm::catalog::provider_endpoint(provider).map(|entry| {
                    Cow::Owned(crate::llm::catalog::fill_endpoint_template(
                        provider,
                        entry.0,
                        crate::llm::catalog::EndpointSettings {
                            snowflake_account: self.snowflake_account.as_deref(),
                            cloudflare_account_id: self.cloudflare_account_id.as_deref(),
                            cloudflare_gateway_id: self.cloudflare_gateway_id.as_deref(),
                        },
                    ))
                })
            })
    }

    pub fn request_headers(&self) -> &[crate::llm::model_config::RequestHeader] {
        let Some(endpoint) = &self.endpoint else {
            return &[];
        };
        endpoint
            .headers
            .as_deref()
            .or_else(|| {
                let spec = self
                    .provider
                    .as_ref()
                    .and_then(|id| self.providers.get(id))
                    .or_else(|| {
                        self.providers.values().find(|spec| {
                            spec.base_url.trim_end_matches('/')
                                == endpoint.base_url.trim_end_matches('/')
                        })
                    })?;
                (spec.base_url.trim_end_matches('/') == endpoint.base_url.trim_end_matches('/'))
                    .then_some(spec.headers.as_deref())
                    .flatten()
            })
            .unwrap_or_default()
    }

    pub fn selection_endpoint(
        &self,
        selection: &crate::llm::model_config::ModelSelection,
    ) -> Option<types::EndpointConfig> {
        let spec = self.providers.get(&selection.provider);
        self.selection_base_url(selection)
            .map(|url| types::EndpointConfig {
                headers: selection.headers.clone().or_else(|| {
                    spec.filter(|p| {
                        selection.base_url.is_none()
                            || p.base_url.trim_end_matches('/') == url.trim_end_matches('/')
                    })
                    .and_then(|p| p.headers.clone())
                }),
                base_url: url.to_string(),
                api_key_env: selection
                    .api_key_env
                    .clone()
                    .or_else(|| {
                        spec.filter(|p| {
                            selection.base_url.is_none()
                                || p.base_url.trim_end_matches('/') == url.trim_end_matches('/')
                        })
                        .and_then(|p| p.api_key_env.clone())
                    })
                    .or_else(|| {
                        crate::llm::catalog::provider_endpoint(&selection.provider)
                            .and_then(|(_, env, _)| env.map(str::to_owned))
                    }),
                allow_insecure: selection.allow_insecure
                    || (selection.base_url.is_none() && spec.is_some_and(|p| p.allow_insecure)),
                protocol: selection
                    .protocol
                    .or_else(|| spec.and_then(|p| p.protocol))
                    .or_else(|| {
                        crate::llm::catalog::provider_endpoint(&selection.provider)
                            .map(|entry| entry.2)
                    })
                    .or_else(|| {
                        Some(crate::llm::model_config::ApiProtocol::builtin(
                            &selection.provider,
                        ))
                    }),
                account_id: selection.account_id.clone().filter(|id| !id.is_empty()),
            })
    }

    pub fn with_selection(&self, selection: &crate::llm::model_config::ModelSelection) -> Self {
        let mut config = self.clone();
        config.model = selection.model.clone();
        config.model_profile = self.model_profile.clone().filter(|name| {
            self.model_profiles
                .get(name)
                .is_some_and(|p| p.provider == selection.provider && p.model == selection.model)
        });
        config.provider = Some(selection.provider.clone());
        config.endpoint = self.selection_endpoint(selection);
        // Role selections may carry an explicit cap; the main model's cap never leaks to them.
        config.context_limit = selection
            .context_window
            .map(types::ContextWindow::Tokens)
            .unwrap_or_default();
        config.context_window = config.resolve_context_window(selection);
        config
    }
    /// Resolve an explicitly named, operator-configured profile without accepting inline
    /// provider/model overrides from task input.
    pub fn with_named_model_profile(&self, name: &str) -> anyhow::Result<Self> {
        let profile = self
            .model_profiles
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown configured model profile {name:?}"))?;
        let selection = crate::llm::model_config::ModelSelection {
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            context_window: profile.context_window,
            ..Default::default()
        };
        let mut config = self.with_selection(&selection);
        config.model_profile = Some(name.to_owned());
        Ok(config)
    }

    fn family_context_limit(
        &self,
        selection: &crate::llm::model_config::ModelSelection,
    ) -> types::ContextWindow {
        let local = (selection.provider == "mlx" && selection.base_url.is_none())
            || selection.provider == crate::llm::apple::PROVIDER
            || selection
                .base_url
                .as_deref()
                .and_then(crate::llm::host_of)
                .is_some_and(|host| crate::llm::is_local_host(&host));
        if local {
            self.local_context_window
        } else {
            self.cloud_context_window
        }
    }

    /// Operator cap before capacity resolution, used when a local server reports a fresh loaded size.
    pub fn requested_context_window(&self) -> Option<u32> {
        self.active_model_profile()
            .and_then(|profile| profile.context_window)
            .or_else(|| self.context_limit.tokens())
            .or_else(|| self.family_context_limit(&self.selection()).tokens())
    }

    fn resolve_context_window(&self, selection: &crate::llm::model_config::ModelSelection) -> u32 {
        let family = self.family_context_limit(selection);
        let mut metadata = selection.clone();
        metadata.context_window = None;
        if metadata.headers.is_none() {
            metadata.headers = self
                .providers
                .get(&selection.provider)
                .filter(|spec| {
                    self.selection_base_url(selection).is_some_and(|url| {
                        url.trim_end_matches('/') == spec.base_url.trim_end_matches('/')
                    })
                })
                .and_then(|spec| spec.headers.clone());
        }
        let capacity = crate::llm::catalog::model_context_window(&self.user_dir, &metadata);
        let requested = self
            .active_model_profile()
            .filter(|profile| {
                profile.provider == selection.provider && profile.model == selection.model
            })
            .and_then(|profile| profile.context_window)
            .or(selection.context_window)
            .or_else(|| family.tokens());
        match (requested, capacity) {
            (Some(limit), Some(capacity)) => limit.min(capacity),
            (Some(limit), None) => limit,
            (None, Some(capacity)) => capacity,
            (None, None) => types::Config::default().context_window,
        }
    }

    /// Configuration for a model role. An assigned role resolves to its selection; an unassigned role
    /// (and `main`) resolves to the session model.
    pub fn for_role(&self, role: &str) -> Self {
        let mut resolved = match self.role_selection(role) {
            Some(selection) => {
                let mut resolved = self.with_selection(selection);
                if selection.provider == "chatgpt"
                    && selection.account_id.is_none()
                    && self.provider.as_deref() == Some("chatgpt")
                    && let Some(endpoint) = &mut resolved.endpoint
                {
                    endpoint.account_id = self.endpoint.as_ref().and_then(|e| e.account_id.clone());
                }
                resolved
            }
            None => self.clone(),
        };
        resolved.steering.enabled = false;
        resolved.steering.automatic = false;
        resolved.adaptive.enabled = false;
        resolved
    }

    /// The selection an assigned role resolves to. Callers apply their own fallback to the session model
    /// when this returns `None`.
    pub fn role_selection(&self, role: &str) -> Option<&crate::llm::model_config::ModelSelection> {
        self.model_roles.get(role)
    }

    /// Laya service settings: the assigned `decision` role supplies the checkpoint the service loads.
    pub fn laya_service(&self) -> crate::config::laya::Config {
        let mut laya = self.laya.clone();
        laya.checkpoint = self
            .role_selection("decision")
            .map(|selection| selection.model.clone());
        laya
    }

    /// Alt+M Roles rows in `ROLES` order: the model each role uses now and where that model comes from.
    pub fn role_statuses(&self) -> Vec<crate::llm::model_config::RoleStatus> {
        use crate::llm::model_config::{RoleSource, RoleStatus};
        let main = self.selection().display_name();
        crate::llm::model_config::ROLES
            .iter()
            .map(|&(role, _)| {
                let assigned = if role == "main" {
                    Some(self.selection())
                } else {
                    self.model_roles.get(role).cloned()
                };
                let (effective, source) = match (role, &assigned) {
                    ("main", _) => (Some(main.clone()), RoleSource::Main),
                    (_, Some(selection)) => (Some(selection.display_name()), RoleSource::Assigned),
                    ("decision", None) => {
                        match crate::config::laya::expected_checkpoint(&self.laya) {
                            Some(checkpoint) => (
                                Some(format!("Laya · {checkpoint}")),
                                RoleSource::LayaInstalled,
                            ),
                            None => (None, RoleSource::NotSet),
                        }
                    }
                    ("judge", None) => match self.model_roles.get("small") {
                        Some(small) => (Some(small.display_name()), RoleSource::FollowsSmall),
                        None => (Some(main.clone()), RoleSource::FollowsMain),
                    },
                    ("embedding", None) => (None, RoleSource::NotSet),
                    // The memory role never falls back to main: local and sharpshooter stay inactive
                    // and semantic pauses automatic fact saving until it is assigned.
                    ("memory", None) => (None, RoleSource::NotSet),
                    (_, None) => (Some(main.clone()), RoleSource::FollowsMain),
                };
                RoleStatus {
                    role,
                    assigned,
                    effective,
                    source,
                }
            })
            .collect()
    }

    /// Missing model roles leave memory inactive, without blocking access to its setup UI.
    pub fn memory_setup_error(&self) -> Option<String> {
        use crate::config::memory_backend::{MemoryBackendId, calls_model};
        if !calls_model(&self.memory) {
            return None;
        }
        let roles = match self.memory.backend {
            MemoryBackendId::Local => vec![
                self.memory.local.extraction_role.as_str(),
                self.memory.local.consolidation_role.as_str(),
            ],
            // A non-empty model override replaces the role (the driver resolves it directly).
            MemoryBackendId::Sharpshooter if self.memory.sharpshooter.model.trim().is_empty() => {
                vec![self.memory.sharpshooter.model_role.as_str()]
            }
            _ => Vec::new(),
        };
        roles
            .into_iter()
            .find(|role| self.role_selection(role).is_none())
            .map(|role| {
                format!(
                    "the {} backend needs a model, so assign the {role} role in /roles (Alt+M)",
                    self.memory.backend.as_str()
                )
            })
    }

    /// Configuration for architecture synthesis. An assigned `small` role wins; otherwise the Project
    /// understanding model (`codebase.architecture_model`, local MLX by default) runs synthesis when set,
    /// and an empty setting falls back to `main`.
    pub fn architecture_config(&self) -> Self {
        let model = self.codebase.architecture_model.trim();
        if self.model_roles.contains_key("small") || model.is_empty() {
            return self.for_role("small");
        }
        let mut resolved = self.with_selection(&crate::llm::model_config::ModelSelection {
            model: model.into(),
            context_window: Some(32768),
            ..Default::default()
        });
        resolved.steering.enabled = false;
        resolved.steering.automatic = false;
        resolved.adaptive.enabled = false;
        resolved
    }

    fn qualified_model_override<'a>(&self, name: &'a str) -> Option<(&'a str, &'a str)> {
        name.split_once('/').filter(|(provider, _)| {
            self.providers.contains_key(*provider)
                || matches!(
                    *provider,
                    "mlx" | "apple" | "openai" | "chatgpt" | "anthropic" | "gemini" | "ollama"
                )
        })
    }

    fn memory_role_is_local(&self, role: &str, model: Option<&str>) -> bool {
        let selection = self.role_selection(role);
        if selection.is_some_and(|model| model.provider == crate::config::laya::PROVIDER) {
            return false;
        }
        let provider = selection
            .map(|selection| selection.provider.as_str())
            .or(self.provider.as_deref())
            .unwrap_or("mlx");
        // The system model runs in an OS service, not in Dial's local-compute lease.
        if provider == crate::llm::apple::PROVIDER && model.is_none() {
            return false;
        }
        let base = if let Some((overridden, _)) = model
            .map(str::trim)
            .and_then(|name| self.qualified_model_override(name))
            .filter(|(overridden, _)| *overridden != provider)
        {
            self.provider_base_url(overridden)
        } else if let Some(selection) = selection {
            self.selection_base_url(selection)
        } else {
            self.endpoint
                .as_ref()
                .map(|endpoint| std::borrow::Cow::Borrowed(endpoint.base_url.as_str()))
        };
        base.is_none_or(|url| {
            crate::llm::host_of(&url)
                .as_deref()
                .is_some_and(crate::llm::is_local_host)
        })
    }

    /// Selected memory work participates in the same local-compute lease as foreground turns.
    /// This route inspection borrows selectors; it never constructs a backend or clones Config.
    pub fn memory_uses_local_compute(&self) -> bool {
        use crate::config::memory_backend::MemoryBackendId;
        match self.memory.backend {
            MemoryBackendId::Local => {
                self.memory_role_is_local(&self.memory.local.extraction_role, None)
                    || self.memory_role_is_local(&self.memory.local.consolidation_role, None)
            }
            MemoryBackendId::Sharpshooter => self.memory_role_is_local(
                &self.memory.sharpshooter.model_role,
                Some(&self.memory.sharpshooter.model),
            ),
            // Automatic fact saving runs on the memory role and pauses while it is unassigned.
            MemoryBackendId::Semantic => {
                self.memory.semantic.auto_retain
                    && self.role_selection("memory").is_some()
                    && self.memory_role_is_local("memory", None)
            }
            _ => false,
        }
    }

    /// Shared role-model resolver for private model workers (memory backends, subagents).
    /// Applies the same selector/header/account conventions as `for_role`, then an optional
    /// operator/model override. A bare name — including a local repository name that
    /// contains a slash — keeps the role's provider; a qualified "provider/model" changes
    /// the provider only when it is a builtin or configured provider.
    pub fn memory_role_config(
        &self,
        role: &str,
        model_override: Option<&str>,
    ) -> anyhow::Result<Self> {
        let Some(name) = model_override
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Ok(self.for_role(role));
        };
        anyhow::ensure!(
            name.len() <= 512,
            "model override for {role:?} exceeds 512 bytes"
        );
        let mut selection = self
            .role_selection(role)
            .cloned()
            .unwrap_or_else(|| self.selection());
        if let Some((provider, model)) = self.qualified_model_override(name) {
            if selection.provider != provider {
                selection = crate::llm::model_config::ModelSelection {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    ..Default::default()
                };
            } else {
                selection.model = model.to_owned();
            }
        } else {
            selection.model = name.to_owned();
        }
        if !self.model_roles.contains_key(role) && selection.model != self.model {
            selection.context_window = None;
        }
        anyhow::ensure!(
            !selection.model.trim().is_empty() && selection.model.trim().len() <= 512,
            "model override {name:?} for {role:?} does not name a model"
        );
        let mut resolved = self.with_selection(&selection);
        resolved.steering.enabled = false;
        resolved.steering.automatic = false;
        resolved.adaptive.enabled = false;
        if selection.provider == "chatgpt"
            && selection.account_id.is_none()
            && self.provider.as_deref() == Some("chatgpt")
            && let Some(endpoint) = &mut resolved.endpoint
        {
            endpoint.account_id = self.endpoint.as_ref().and_then(|e| e.account_id.clone());
        }
        let resolvable = selection.base_url.is_some()
            || self.providers.contains_key(&selection.provider)
            || matches!(selection.provider.as_str(), "chatgpt" | "gemini" | "ollama")
            || selection.provider == "mlx"
            || selection.provider == crate::llm::apple::PROVIDER;
        anyhow::ensure!(
            resolvable,
            "model override {name:?} for {role:?} names provider {:?} without a configured endpoint",
            selection.provider
        );
        Ok(resolved)
    }

    pub fn active_model_profile(&self) -> Option<&crate::llm::profiles::ModelProfile> {
        let selection = self.selection();
        if let Some(name) = &self.model_profile {
            return self
                .model_profiles
                .get(name)
                .filter(|p| p.provider == selection.provider && p.model == selection.model);
        }
        let mut matches = self
            .model_profiles
            .values()
            .filter(|p| p.provider == selection.provider && p.model == selection.model);
        let first = matches.next();
        if matches.next().is_none() {
            first
        } else {
            None
        }
    }

    /// Explicit edit grammar pins, strongest first: the process override, the active model profile,
    /// the harness, then the longest matching `[edit] model_variants` selector.
    pub fn pinned_edit_mode(&self, harness: &Harness) -> Option<types::EditMode> {
        self.edit_mode_override
            .or_else(|| {
                self.active_model_profile()
                    .and_then(|profile| profile.edit_mode)
            })
            .or(harness.edit_mode)
            .or_else(|| {
                self.edit.variant_mode(&format!(
                    "{}/{}",
                    self.provider.as_deref().unwrap_or("mlx"),
                    self.model
                ))
            })
    }

    /// The edit grammar in effect: a pin, else the local-route default (`assistance.local.edit_mode`),
    /// else `[edit] mode`.
    pub fn resolved_edit_mode(&self, harness: &Harness) -> types::EditMode {
        self.pinned_edit_mode(harness)
            .or_else(|| self.local_edit_mode())
            .unwrap_or(self.edit.mode)
    }

    /// The edit grammar without the local-route default, for hermetic evaluation.
    pub fn base_edit_mode(&self, harness: &Harness) -> types::EditMode {
        self.pinned_edit_mode(harness).unwrap_or(self.edit.mode)
    }

    /// `assistance.local.edit_mode` when the local profile applies to the main route.
    fn local_edit_mode(&self) -> Option<types::EditMode> {
        self.local_profile_applies()
            .then_some(self.assistance.local.edit_mode.0)
            .flatten()
    }

    /// The `[assistance.local]` profile is on and the selected main model is served locally.
    pub fn local_profile_applies(&self) -> bool {
        self.assistance.local.enabled && !self.model.is_empty() && self.local_main_model()
    }

    /// Resolve coding controls without changing the learned harness on disk.
    pub fn effective_harness(&self, harness: &Harness) -> Result<Harness> {
        let mut effective = harness.clone();
        // Operator `[harness]` values that differ from the defaults are pins and win over the stored harness.
        let defaults = Harness::default();
        let pins = &self.harness;
        if pins.system_prompt != defaults.system_prompt {
            effective.system_prompt = pins.system_prompt.clone();
        }
        if pins.tools != defaults.tools {
            effective.tools = pins.tools.clone();
        }
        if pins.memory != defaults.memory {
            effective.memory = pins.memory.clone();
        }
        if pins.subagents != defaults.subagents {
            effective.subagents = pins.subagents.clone();
        }
        if pins.max_steps != defaults.max_steps {
            effective.max_steps = pins.max_steps;
        }
        if pins.temperature != defaults.temperature {
            effective.temperature = pins.temperature;
        }
        if pins.max_tokens != defaults.max_tokens {
            effective.max_tokens = pins.max_tokens;
        }
        if pins.edit_mode != defaults.edit_mode {
            effective.edit_mode = pins.edit_mode;
        }
        if let Some(profile) = self.active_model_profile() {
            profile.validate()?;
            if let Some(t) = profile.temperature {
                effective.temperature = t;
            }
            if let Some(n) = profile.max_tokens {
                effective.max_tokens = n;
            }
            if let Some(mode) = profile.edit_mode {
                effective.edit_mode = Some(mode);
            }
            if let Some(tools) = &profile.tools {
                effective.tools = tools.clone();
            } else if profile.capabilities.tools == Some(false) {
                effective.tools.clear();
            }
        }
        Ok(effective)
    }

    /// "Teach this model" is on and the main model runs locally: Dial's MLX runtime, or a server on
    /// this machine or the local network (Ollama, LM Studio). Hosted models are never taught.
    pub fn teaching(&self) -> bool {
        self.improve.teach && !self.model.is_empty() && self.local_main_model()
    }

    /// The main model runs on this machine or the local network: Dial's MLX/GGUF runtime (no
    /// endpoint), or an endpoint on a loopback, private-range or `.local` host (Ollama, LM Studio,
    /// llama.cpp). The single classification every local-model policy uses.
    pub fn local_main_model(&self) -> bool {
        self.endpoint.as_ref().is_none_or(|endpoint| {
            crate::llm::host_of(&endpoint.base_url)
                .is_some_and(|host| crate::llm::is_local_host(&host))
        })
    }

    /// Whether the main model runs on Dial's supervised MLX/GGUF runtime (no endpoint and not the
    /// Apple system model). Training, adapters, steering and the Python runtime apply only here.
    pub fn runs_dial_mlx(&self) -> bool {
        self.endpoint.is_none() && self.provider.as_deref() != Some(crate::llm::apple::PROVIDER)
    }

    /// Whether this session needs the shared local-compute lock: a local main model or any explicit local role.
    /// The Apple system model runs in an OS service and does not take the lease.
    pub fn uses_local_compute(&self) -> bool {
        (!self.model.is_empty() && self.runs_dial_mlx())
            || self.model_roles.values().any(|model| {
                model.provider != crate::config::laya::PROVIDER
                    && model.provider != crate::llm::apple::PROVIDER
                    && model.base_url.is_none()
            })
            || self.memory_uses_local_compute()
    }

    /// Lock policy is independent of route classification and model-cost accounting.
    pub fn requires_compute_lock(&self) -> bool {
        self.compute_locks && self.uses_local_compute()
    }

    pub fn validate(&self) -> Result<()> {
        let values = toml::Table::try_from(self)?;
        for setting in schema::registry() {
            if let Some(value) = schema::get(&values, setting.path) {
                schema::check_value_for(&values, setting.path, value)
                    .map_err(anyhow::Error::msg)?;
            }
        }
        self.edit.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            SERVICE_TIERS.contains(&self.service_tier.as_str()),
            "service_tier must be one of: {}",
            SERVICE_TIERS.join(", ")
        );
        self.compaction.validate()?;
        for selection in std::iter::once(self.selection())
            .chain(self.model_roles.values().cloned())
            .chain(self.model_favorites.iter().cloned())
        {
            crate::llm::model_config::validate_headers(
                selection.headers.as_deref().unwrap_or_default(),
            )?;
            ensure!(
                selection.headers.as_ref().is_none_or(|h| h.is_empty())
                    || selection.base_url.is_some()
                    || self.providers.contains_key(&selection.provider)
                    || matches!(selection.provider.as_str(), "gemini" | "ollama" | "chatgpt"),
                "request headers require an HTTP endpoint"
            );
            let base = self.selection_base_url(&selection);
            let headers = selection
                .headers
                .as_deref()
                .or_else(|| {
                    self.providers
                        .get(&selection.provider)
                        .filter(|spec| {
                            base.as_ref().is_some_and(|url| {
                                url.trim_end_matches('/') == spec.base_url.trim_end_matches('/')
                            })
                        })
                        .and_then(|spec| spec.headers.as_deref())
                })
                .unwrap_or_default();
            crate::llm::model_config::validate_headers(headers)?;
            ensure!(
                headers.is_empty()
                    || (selection.provider != "chatgpt"
                        && !(selection.provider == "anthropic"
                            && base.as_deref() == Some(crate::llm::anthropic::BASE_URL)
                            && crate::llm::anthropic_oauth::signed_in(&self.user_dir))),
                "subscription providers do not permit custom request headers"
            );
            ensure!(
                headers.is_empty() || base.is_some(),
                "request headers require an HTTP endpoint"
            );
            if let Some(id) = &selection.account_id {
                ensure!(
                    selection.provider == "chatgpt",
                    "account_id is supported only for ChatGPT; Anthropic has one account"
                );
                ensure!(
                    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic()),
                    "account_id requires a safe nonempty workspace ID"
                );
                ensure!(
                    self.chatgpt_pinned_accounts.contains(id),
                    "ChatGPT account_id is not authorized by private global policy"
                );
            }
        }
        for (id, spec) in &self.providers {
            let headers = spec.headers.as_deref().unwrap_or_default();
            crate::llm::model_config::validate_headers(headers)?;
            ensure!(
                headers.is_empty()
                    || (id != "chatgpt"
                        && !(id == "anthropic"
                            && spec.base_url == crate::llm::anthropic::BASE_URL
                            && crate::llm::anthropic_oauth::signed_in(&self.user_dir))),
                "subscription providers do not permit custom request headers"
            );
        }
        self.steering.validate()?;
        self.adaptive.validate()?;
        self.reliability.validate()?;
        ensure!(
            (1..=32).contains(&self.subagent_max_live_jobs),
            "subagent_max_live_jobs must be 1..32"
        );
        ensure!(
            (1..=64).contains(&self.portable_rules.max_regenerations),
            "portable_rules.max_regenerations must be 1..64"
        );
        ensure!(
            self.training.learn_from_max_percent <= 50,
            "training.learn_from_max_percent must be 0..50"
        );
        ensure!(
            (self.efficiency.ambient_context_bytes == 0
                || (1024..=8192).contains(&self.efficiency.ambient_context_bytes))
                && (self.efficiency.tool_preview_bytes == 0
                    || (1024..=16384).contains(&self.efficiency.tool_preview_bytes))
                && self.efficiency.lightweight_requests <= 8
                && (50..=79).contains(&self.efficiency.cache_pressure_percent)
                && (self.efficiency.history_token_cap == 0
                    || (8192..=2_000_000).contains(&self.efficiency.history_token_cap)),
            "invalid efficiency context, preview, lightweight, cache-pressure or history-cap bounds"
        );
        self.assistance
            .local
            .validate()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            !self.runs_dial_mlx()
                || !self
                    .active_model_profile()
                    .is_some_and(|p| p.capabilities.strict_tools),
            "Dial-owned MLX applies schema-constrained tool decoding automatically for supported JSON tool templates; remove capabilities.strict_tools from this profile. Runtime schema validation remains active"
        );
        ensure!(
            self.assistance.initial_tool_limit <= 64,
            "assistance.initial_tool_limit must be 0 (unlimited) or 1..64"
        );
        ensure!(
            self.assistance.context_max_bytes == 0
                || (512..=8192).contains(&self.assistance.context_max_bytes),
            "assistance.context_max_bytes must be 0 (unlimited) or 512..8192"
        );
        ensure!(
            self.assistance.teacher_timeout_secs <= 30,
            "assistance.teacher_timeout_secs must be 0 (unlimited) or 1..30"
        );
        ensure!(
            self.verification.python_command.len() <= 32
                && self.verification.python_command.iter().all(|arg| {
                    !arg.trim().is_empty() && arg.len() <= 4096 && !arg.contains(['\0', '\n', '\r'])
                }),
            "verification.python_command must be an argv list (or [] for autodetection), without control characters"
        );
        self.solve.validate()?;
        self.escalation.validate()?;
        if self.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER) {
            ensure!(
                self.endpoint
                    .as_ref()
                    .is_none_or(|e| e.base_url == crate::llm::chatgpt::BASE_URL
                        && e.api_key_env.is_none()
                        && e.protocol
                            .is_none_or(|p| p == crate::llm::model_config::ApiProtocol::Responses)),
                "ChatGPT subscription uses official OAuth credentials only"
            );
        }
        if let Some(spec) = self.providers.get(crate::llm::chatgpt::PROVIDER) {
            ensure!(
                spec.base_url == crate::llm::chatgpt::BASE_URL
                    && spec.api_key_env.is_none()
                    && spec
                        .protocol
                        .is_none_or(|p| p == crate::llm::model_config::ApiProtocol::Responses),
                "ChatGPT provider cannot override its OAuth endpoint"
            );
        }
        if self.provider.as_deref() == Some("anthropic")
            && crate::llm::anthropic_oauth::signed_in(&self.user_dir)
            && let Some(endpoint) = &self.endpoint
            && endpoint.base_url == crate::llm::anthropic::BASE_URL
        {
            ensure!(
                endpoint
                    .protocol
                    .is_none_or(|p| p == crate::llm::model_config::ApiProtocol::Anthropic),
                "Claude subscription requires Anthropic protocol"
            );
        }
        ensure!(
            self.worker_timeout_secs <= 86400,
            "worker_timeout_secs must be 0 (unlimited) or 1..86400"
        );
        ensure!(
            self.remote_model_idle_timeout_secs <= 86400,
            "remote_model_idle_timeout_secs must be 0 (unlimited) or 1..86400"
        );
        ensure!(
            self.reasoning_guard.max_pre_action_secs <= 86400,
            "reasoning_guard.max_pre_action_secs must be 0 (unlimited) or 1..86400"
        );
        ensure!(
            self.reasoning_guard.max_reasoning_bytes == 0
                || (256..=1_048_576).contains(&self.reasoning_guard.max_reasoning_bytes),
            "reasoning_guard.max_reasoning_bytes must be 0 (unlimited) or 256..1048576"
        );
        ensure!(
            (1..=50).contains(&self.retry.max_attempts)
                && (1..=3600).contains(&self.retry.max_delay_secs),
            "retry.max_attempts must be 1..50 and retry.max_delay_secs 1..3600"
        );
        ensure!(
            self.codebase.context_max_bytes == 0
                || (1024..=24000).contains(&self.codebase.context_max_bytes),
            "codebase.context_max_bytes must be 0 (unlimited) or 1024..24000"
        );
        self.laya.validate()?;
        for (name, mode) in [
            ("context_window", self.context_limit),
            ("cloud_context_window", self.cloud_context_window),
            ("local_context_window", self.local_context_window),
        ] {
            ensure!(
                mode.tokens()
                    .is_none_or(|n| (2048..=types::MAX_CONTEXT_WINDOW).contains(&n)),
                "{name} must be auto or 2048..2097152 tokens"
            );
        }
        ensure!(
            (2048..=types::MAX_CONTEXT_WINDOW).contains(&self.context_window),
            "context_window must be 2048..2097152 tokens"
        );
        ensure!(
            self.ui.history_limit <= 10000,
            "ui.history_limit must be 0 (unlimited) or 1..10000"
        );
        crate::config::memory_backend::validate(&self.memory)?;
        ensure!(
            self.browser.timeout_secs <= 3600 && !self.browser.node.trim().is_empty(),
            "invalid browser program or timeout"
        );
        ensure!(
            self.browser.playwright_module.is_empty()
                || self.browser.playwright_module.trim() == self.browser.playwright_module,
            "browser.playwright_module must be empty (built-in runtime) or an unpadded module path"
        );
        self.browser
            .validate_allowed_hosts()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            self.eval.timeout_secs <= 3600 && !self.eval.node.trim().is_empty(),
            "invalid eval program or timeout"
        );
        ensure!(
            self.kernel.timeout_secs <= 3600 && !self.kernel.python.trim().is_empty(),
            "invalid kernel program or timeout"
        );
        for (name, profile) in &self.model_profiles {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid model profile name"
            );
            profile
                .validate()
                .with_context(|| format!("invalid model profile {name}"))?;
            if let Some(tools) = &profile.tools {
                let mut names = BTreeSet::new();
                for tool in tools {
                    ensure!(
                        names.insert(tool) && self.known_tool(tool),
                        "unknown or duplicate model-profile tool {tool}"
                    );
                }
            }
        }
        if let Some(name) = &self.model_profile {
            let profile = self
                .model_profiles
                .get(name)
                .with_context(|| format!("unknown model profile {name}"))?;
            ensure!(
                profile.provider == self.selection().provider
                    && profile.model == self.selection().model,
                "model_profile {name:?} is stale for the active model; clear it or choose its provider/model"
            );
        }
        let assignable: Vec<&str> = crate::llm::model_config::ROLES
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| *name != "main")
            .collect();
        for (role, selection) in &self.model_roles {
            ensure!(
                assignable.contains(&role.as_str()),
                "unknown model role {role}; valid roles are {}",
                assignable.join(", ")
            );
            ensure!(
                !selection.model.trim().is_empty(),
                "model role {role} has no model"
            );
            ensure!(
                role != "decision" || selection.provider == crate::config::laya::PROVIDER,
                "the decision role is the Laya checkpoint; assign it with /roles or with dial laya install --checkpoint"
            );
            ensure!(
                role != "judge"
                    || selection.provider != crate::config::laya::PROVIDER
                    || selection.model == crate::config::laya::JUDGE_MODEL,
                "a Laya judge uses the decision role's checkpoint; set model_roles.judge.model = {:?} and choose the checkpoint with the decision role",
                crate::config::laya::JUDGE_MODEL
            );
            if selection.provider == crate::llm::chatgpt::PROVIDER {
                ensure!(
                    selection
                        .base_url
                        .as_deref()
                        .is_none_or(|url| url == crate::llm::chatgpt::BASE_URL)
                        && selection.api_key_env.is_none()
                        && selection
                            .protocol
                            .is_none_or(|p| p == crate::llm::model_config::ApiProtocol::Responses),
                    "ChatGPT roles use official OAuth credentials only"
                );
            }
            if selection.provider == "anthropic"
                && selection.base_url.as_deref() == Some(crate::llm::anthropic::BASE_URL)
                && crate::llm::anthropic_oauth::signed_in(&self.user_dir)
            {
                ensure!(
                    selection
                        .protocol
                        .is_none_or(|p| p == crate::llm::model_config::ApiProtocol::Anthropic),
                    "Claude subscription roles require Anthropic protocol"
                );
            }
            if let Some(url) = &selection.base_url {
                crate::llm::provider::endpoint_url(url, selection.allow_insecure)?;
            }
            ensure!(
                selection
                    .context_window
                    .is_none_or(|w| (2048..=types::MAX_CONTEXT_WINDOW).contains(&w)),
                "invalid role context window"
            );
        }
        for (key, chain) in &self.retry.fallback_chains {
            ensure!(
                !key.trim().is_empty() && key.len() <= 512,
                "invalid fallback-chain key"
            );
            ensure!(
                chain.len() <= 32,
                "fallback chain {key:?} exceeds 32 entries"
            );
            for selector in chain {
                ensure!(
                    !selector.trim().is_empty() && selector.trim().len() <= 512,
                    "invalid model selector in fallback chain {key:?}"
                );
            }
        }
        for (id, provider) in &self.providers {
            ensure!(
                !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid provider id"
            );
            ensure!(
                id != "mlx" && id != crate::llm::apple::PROVIDER,
                "{id} is reserved for a built-in local provider"
            );
            crate::llm::provider::endpoint_url(&provider.base_url, provider.allow_insecure)?;
        }
        let i = &self.improve;
        ensure!(
            (1..=604800).contains(&i.interval_secs) && (1..=10000).contains(&i.min_new_turns),
            "invalid automatic cycle interval or turn threshold"
        );
        ensure!(
            i.selfplay_tasks <= 1000
                && (6..=1000).contains(&i.gate_primary_cases)
                && (4..=1000).contains(&i.gate_guard_cases),
            "invalid self-play or gate case count"
        );
        ensure!(
            i.alpha.is_finite() && i.alpha > 0.0 && i.alpha <= 1.0,
            "improve.alpha must be in (0,1]"
        );
        ensure!(
            i.max_candidates <= 10 && i.min_net_wins <= i.gate_primary_cases,
            "invalid candidate count or net-win threshold"
        );
        ensure!(
            i.drift_tolerance.is_finite() && (0.0..=3.0).contains(&i.drift_tolerance),
            "drift_tolerance must be 0..3 percentage points"
        );
        ensure!(
            i.monitor_drop.is_finite()
                && i.monitor_drop > 0.0
                && i.monitor_drop <= 100.0
                && i.monitor_min_turns >= 2,
            "invalid matched live-monitor threshold"
        );
        ensure!(
            self.improve.idle_minutes <= 1440 && self.improve.daily_compute_minutes <= 1440,
            "idle_minutes and daily_compute_minutes must be at most 1440"
        );
        ensure!(
            i.max_daily_usd.is_finite() && i.max_daily_usd >= 0.0,
            "improve.max_daily_usd must be a nonnegative USD amount; zero disables background endpoint-model cycles"
        );
        for cap in [
            self.budget.max_session_cost_usd,
            self.budget.max_turn_cost_usd,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                cap.is_finite() && cap >= 0.0,
                "budget caps must be nonnegative numbers of USD; zero is unlimited"
            );
        }
        ensure!(self.hooks.len() <= 32, "at most 32 hooks are allowed");
        for hook in &self.hooks {
            crate::config::hooks::validate(hook).map_err(|e| anyhow::anyhow!(e))?;
        }
        ensure!(
            self.verification.commands.len() <= 16,
            "at most 16 verification commands are allowed"
        );
        for check in &self.verification.commands {
            ensure!(valid_name(&check.name), "invalid verification command name");
            ensure!(
                !check.command.trim().is_empty() && check.command.len() <= 8192,
                "verification command must be 1..8192 bytes"
            );
            ensure!(
                check.timeout_secs <= 600,
                "verification command timeout must be 0 (unlimited) or 1..600 seconds"
            );
            ensure!(
                check.file_extensions.len() <= 32
                    && check
                        .file_extensions
                        .iter()
                        .all(|extension| !extension.is_empty()
                            && extension
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')),
                "verification file extensions must be simple language suffixes"
            );
        }
        ensure!(
            self.verification.behavioral_checks.len() <= 16
                && self.verification.behavioral_checks.iter().all(|name| self
                    .verification
                    .commands
                    .iter()
                    .filter(|check| &check.name == name)
                    .count()
                    == 1),
            "behavioral_checks must name unique configured verification commands"
        );
        ensure!(
            self.verification.requirements.len() <= 50,
            "at most 50 verification requirements are allowed"
        );
        for (name, requirement) in &self.verification.requirements {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid verification requirement name"
            );
            ensure!(
                requirement.visual || !requirement.checks.is_empty(),
                "verification requirement {name} needs checks or visual evidence"
            );
            let unique: std::collections::HashSet<_> = requirement.checks.iter().collect();
            ensure!(
                requirement.checks.len() <= 16
                    && unique.len() == requirement.checks.len()
                    && requirement.checks.iter().all(|name| self
                        .verification
                        .commands
                        .iter()
                        .filter(|check| &check.name == name)
                        .count()
                        == 1),
                "verification requirement {name} must name unique configured checks"
            );
        }
        ensure!(
            i.lora_min_examples >= 1,
            "lora_min_examples must be positive"
        );
        if let Some(endpoint) = &self.endpoint {
            ensure!(
                endpoint.base_url.starts_with("http://")
                    || endpoint.base_url.starts_with("https://"),
                "endpoint.base_url must be http(s)"
            );
            ensure!(
                !self.model.trim().is_empty(),
                "endpoint requires `model` (the server's model name)"
            );
        }
        if let Some(gate) = &self.gate {
            ensure!(
                gate.max_regression.is_finite() && (0.0..=100.0).contains(&gate.max_regression),
                "max_regression must be 0..100 percentage points"
            );
            ensure!(
                gate.min_improvement.is_finite()
                    && gate.min_improvement > 0.0
                    && gate.min_improvement <= 100.0,
                "min_improvement must be >0 and <=100 percentage points"
            );
            ensure!(
                gate.suites.len() >= 2,
                "at least two independent evaluation suites are required"
            );
            ensure!(
                gate.suites.iter().filter(|s| s.primary).count() == 1,
                "exactly one primary evaluation suite is required"
            );
            let mut names = BTreeSet::new();
            for suite in &gate.suites {
                ensure!(
                    !suite.name.trim().is_empty() && names.insert(&suite.name),
                    "suite names must be nonempty and unique"
                );
            }
        }
        for (name, tool) in &self.tools {
            ensure!(valid_name(name), "invalid tool name");
            ensure!(
                !BUILTIN.contains(&name.as_str())
                    && !EXTRA_TOOLS.contains(&name.as_str())
                    && !INTEGRATION_TOOLS.contains(&name.as_str()),
                "operator tool name collides with a built-in"
            );
            ensure!(
                tool.timeout_secs <= 3600,
                "tool timeout must be 0 (unlimited) or 1..3600 seconds"
            );
        }
        let mut integrations = BTreeSet::new();
        let dap_adapters = crate::config::tools::dap::catalogue(&self.dap);
        for name in &self.integrations {
            let dap_tool = name
                .strip_prefix("dap_")
                .is_some_and(|id| dap_adapters.contains_key(id));
            ensure!(
                INTEGRATION_TOOLS.contains(&name.as_str()) || dap_tool,
                "unknown integration tool {name:?}; available: {}",
                INTEGRATION_TOOLS.join(", ")
            );
            ensure!(
                integrations.insert(name),
                "duplicate integration tool {name:?}"
            );
        }
        for (name, adapter) in &self.dap {
            ensure!(valid_name(name), "invalid DAP adapter id {name:?}");
            ensure!(
                adapter.timeout_secs <= 600,
                "DAP adapter timeout must be 0 (unlimited) or 1..600 seconds"
            );
            ensure!(
                !adapter.program.as_os_str().is_empty(),
                "DAP adapter program cannot be empty"
            );
        }
        for (name, server) in &self.mcp {
            crate::config::tools::mcp::validate_server(name, server)?;
        }
        for (name, prompt) in &self.subagents {
            ensure!(
                valid_name(name) && !prompt.trim().is_empty() && prompt.len() <= 16384,
                "invalid subagent specification"
            );
            ensure!(
                !crate::config::subagents::is_reserved(name),
                "subagent preset {name:?} collides with a built-in agent type"
            );
        }
        ensure!(
            self.agents.len() <= 32,
            "at most 32 custom agents are allowed"
        );
        for (name, agent) in &self.agents {
            ensure!(
                crate::config::agent_defs::valid_name(name),
                "invalid custom agent name {name:?}"
            );
            ensure!(
                !crate::config::subagents::is_reserved(name) && !self.subagents.contains_key(name),
                "custom agent {name:?} collides with a built-in agent or preset"
            );
            crate::config::agent_defs::validate_config_agent(name, agent)?;
        }
        self.task.validate()?;
        let t = &self.training;
        ensure!(
            t.interval_secs >= 1
                && (1..=10000).contains(&t.iters)
                && (1..=16).contains(&t.batch_size),
            "invalid training interval, iterations or batch size"
        );
        ensure!(
            (1..=256).contains(&t.num_layers)
                && (1..=128).contains(&t.rank)
                && (32..=8192).contains(&t.max_seq_length),
            "invalid LoRA layers, rank or sequence length"
        );
        self.validate_protected()?;
        self.validate_harness(&self.harness)
    }

    fn known_tool(&self, name: &str) -> bool {
        BUILTIN.contains(&name)
            || EXTRA_TOOLS.contains(&name)
            || INTEGRATION_TOOLS.contains(&name)
            || self.tools.contains_key(name)
            || name.strip_prefix("mcp__").is_some_and(|suffix| {
                self.mcp.keys().any(|server| {
                    suffix
                        .strip_prefix(server.as_str())
                        .and_then(|tail| tail.strip_prefix("__"))
                        .is_some_and(|tool| !tool.is_empty())
                })
            })
    }

    pub fn validate_harness(&self, h: &Harness) -> Result<()> {
        schema::check_value(
            "harness.max_steps",
            &toml::Value::Integer(i64::from(h.max_steps)),
        )
        .map_err(anyhow::Error::msg)?;
        schema::check_value("harness.temperature", &toml::Value::Float(h.temperature))
            .map_err(anyhow::Error::msg)?;
        let output_limit = self
            .active_model_profile()
            .and_then(|p| p.max_tokens)
            .unwrap_or(8192);
        ensure!(
            h.max_tokens == 0 || (1..=output_limit.max(8192)).contains(&h.max_tokens),
            "max_tokens exceeds configured output limit {}",
            output_limit.max(8192)
        );
        ensure!(
            h.memory.len() <= 32 && h.memory.iter().map(String::len).sum::<usize>() <= 16384,
            "memory exceeds allowed size"
        );
        let mut names = BTreeSet::new();
        for name in &h.tools {
            let known = self.known_tool(name);
            ensure!(known, "proposal selected an unapproved tool: {name}");
            ensure!(names.insert(name), "duplicate tool selection");
        }
        names.clear();
        for name in &h.subagents {
            ensure!(
                self.subagents.contains_key(name),
                "proposal selected an unapproved subagent: {name}"
            );
            ensure!(names.insert(name), "duplicate subagent selection");
        }
        Ok(())
    }

    /// Gate suites and training data must be outside editable paths. Private storage remains
    /// excluded from tools even when the workspace is its parent (for example HOME).
    fn validate_protected(&self) -> Result<()> {
        if self.project_root.as_os_str().is_empty() {
            return Ok(());
        }
        let mut protected = vec![
            self.state_dir.clone(),
            self.training.train_data.clone(),
            self.training.valid_data.clone(),
        ];
        if let Some(gate) = &self.gate {
            protected.extend(gate.suites.iter().map(|s| s.path.clone()));
        }
        for path in protected.into_iter().filter(|p| !p.as_os_str().is_empty()) {
            // Resolve through the nearest existing ancestor so not-yet-created paths are still checked.
            let mut probe = path.as_path();
            let resolved = loop {
                if let Ok(real) = fs::canonicalize(probe) {
                    break real.join(path.strip_prefix(probe).unwrap_or(Path::new("")));
                }
                probe = probe
                    .parent()
                    .context("protected path has no existing ancestor")?;
            };
            let private = [&self.user_dir, &self.state_dir]
                .into_iter()
                .any(|dir| !dir.as_os_str().is_empty() && resolved.starts_with(dir));
            ensure!(
                !resolved.starts_with(&self.project_root) || private,
                "protected path {} lies inside the editable project; keep it in private storage or outside the project",
                path.display()
            );
            ensure!(
                !self.project_root.starts_with(&resolved),
                "project lies inside protected path {}",
                path.display()
            );
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod harness_pin_tests;

#[cfg(test)]
mod teach_tests;

#[cfg(test)]
mod chatgpt_tests;

#[cfg(test)]
mod memory_migration_tests;

#[cfg(test)]
mod tier_precedence_tests;

#[cfg(test)]
mod hook_layer_tests;

#[cfg(test)]
mod model_role_tests;

#[cfg(test)]
mod requirement_tests;
