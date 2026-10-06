//! Configuration loading, layering and validation; the shared data types live in [`types`].

pub mod types;

use crate::{
    storage::paths::Layout,
    tools::{BUILTIN, EXTRA_TOOLS, INTEGRATION_TOOLS},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use types::{Config, Harness};

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

impl Config {
    /// Layered configuration: `<user>/config.toml`, then `<state>/config.toml` (per project), then
    /// `overlay` (command-line flags). Both files live outside the project, so the agent cannot
    /// edit its own sandbox, approval or gate settings.
    pub fn load(layout: &Layout, overlay: Option<toml::Table>) -> Result<Self> {
        layout.ensure_state_outside_project()?;
        let mut table = toml::Table::new();
        let mut migrations = Vec::new();
        for path in layout.config_files() {
            if let Ok(text) = fs::read_to_string(&path) {
                let mut part: toml::Table = toml::from_str(&text)
                    .with_context(|| format!("invalid configuration {}", path.display()))?;
                if migrate_memory_tool_table(&mut part) {
                    migrations.push(path);
                }
                merge(&mut table, part);
            }
        }
        if let Some(overlay) = overlay {
            merge(&mut table, overlay);
        }
        let config = Self::from_table(layout, table)?;
        // Migrate persisted sources only after the complete layered configuration validates.
        // Re-read under the same lock used by the settings editor to preserve concurrent edits.
        for path in migrations {
            use crate::storage::settings_store::{locked, read_private, write_private};
            let _lock = locked(&path)?;
            let Some(bytes) = read_private(&path)? else {
                continue;
            };
            let mut current: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)
                .with_context(|| format!("invalid configuration {}", path.display()))?;
            if migrate_memory_tool_table(&mut current) {
                write_private(&path, toml::to_string_pretty(&current)?.as_bytes())?;
            }
        }
        Ok(config)
    }

    /// Resolve an already-merged table; used to validate both edits and removals before saving.
    pub fn from_table(layout: &Layout, table: toml::Table) -> Result<Self> {
        layout.ensure_state_outside_project()?;
        let mut config: Config = toml::Value::Table(table)
            .try_into()
            .context("invalid configuration")?;
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
        if config.provider.as_deref() == Some("mlx") {
            config.endpoint = None;
        } else if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER)
            && config.endpoint.is_none()
        {
            config.endpoint = Some(types::EndpointConfig {
                base_url: crate::llm::chatgpt::BASE_URL.into(),
                api_key_env: None,
                allow_insecure: false,
            });
        } else if config.endpoint.is_none() {
            if let Some(spec) = config
                .provider
                .as_ref()
                .and_then(|id| config.providers.get(id))
            {
                config.endpoint = Some(types::EndpointConfig {
                    base_url: spec.base_url.clone(),
                    api_key_env: spec.api_key_env.clone(),
                    allow_insecure: spec.allow_insecure,
                });
            }
        }
        if let Some(endpoint) = &mut config.endpoint {
            endpoint.api_key_env = endpoint
                .api_key_env
                .take()
                .filter(|name| !name.trim().is_empty());
        }
        if config.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER) {
            if let Some(endpoint) = config.endpoint.as_mut() {
                if legacy_chatgpt_api_url(&endpoint.base_url) && endpoint.api_key_env.is_none() {
                    endpoint.base_url = crate::llm::chatgpt::BASE_URL.into();
                }
            }
        }
        config
            .model_roles
            .retain(|_, selection| selection.provider != "inherit");
        for selection in config.model_roles.values_mut() {
            if selection.provider == "mlx" {
                selection.base_url = None;
                selection.api_key_env = None;
                selection.allow_insecure = false;
            } else if selection.provider == crate::llm::chatgpt::PROVIDER
                && selection.base_url.is_none()
            {
                selection.base_url = Some(crate::llm::chatgpt::BASE_URL.into());
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
            resolve(&mut tool.program);
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

    pub fn feedback_model(&self) -> &str {
        self.model_roles
            .get("feedback")
            .map(|m| m.model.as_str())
            .unwrap_or(&self.model)
    }

    /// Provider/model/use-case-specific histories isolate adapters when the user switches models.
    pub fn learning_dir(&self, layout: &Layout) -> PathBuf {
        use sha2::{Digest, Sha256};
        let identity = serde_json::json!([
            self.model,
            self.endpoint
                .as_ref()
                .map(|e| e.base_url.trim_end_matches('/')),
            self.use_case
        ]);
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
            context_window: Some(self.context_window),
        }
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
        config.endpoint = selection
            .base_url
            .as_ref()
            .map(|url| types::EndpointConfig {
                base_url: url.clone(),
                api_key_env: selection.api_key_env.clone(),
                allow_insecure: selection.allow_insecure,
            });
        if let Some(window) = selection.context_window {
            config.context_window = window;
        }
        config
    }

    pub fn for_role(&self, role: &str) -> Self {
        if let Some(selection) = self.model_roles.get(role) {
            let mut resolved = self.with_selection(selection);
            resolved.steering.enabled = false;
            resolved.steering.automatic = false;
            resolved.adaptive.enabled = false;
            return resolved;
        }
        if role == "architecture" && !self.codebase.architecture_model.trim().is_empty() {
            let mut resolved = self.with_selection(&crate::llm::model_config::ModelSelection {
                model: self.codebase.architecture_model.trim().into(),
                context_window: Some(32768),
                ..Default::default()
            });
            resolved.steering.enabled = false;
            resolved.steering.automatic = false;
            resolved.adaptive.enabled = false;
            return resolved;
        }
        let mut resolved = self.clone();
        resolved.steering.enabled = false;
        resolved.steering.automatic = false;
        resolved.adaptive.enabled = false;
        resolved
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

    /// Resolve coding controls without changing the learned harness on disk.
    pub fn effective_harness(&self, harness: &Harness) -> Result<Harness> {
        let mut effective = harness.clone();
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

    pub fn uses_local_compute(&self) -> bool {
        (!self.model.is_empty() && self.endpoint.is_none())
            || self
                .model_roles
                .values()
                .any(|model| model.base_url.is_none())
    }

    pub fn validate(&self) -> Result<()> {
        self.steering.validate()?;
        self.adaptive.validate()?;
        self.reliability.validate()?;
        ensure!(
            !self.uses_local_compute()
                || !self
                    .active_model_profile()
                    .is_some_and(|p| p.capabilities.strict_tools),
            "Dial-owned MLX does not implement strict tool decoding; remove capabilities.strict_tools from this profile. Runtime schema validation remains active"
        );
        ensure!(
            (8..=64).contains(&self.assistance.initial_tool_limit),
            "assistance.initial_tool_limit must be 8..64"
        );
        ensure!(
            (512..=8192).contains(&self.assistance.context_max_bytes),
            "assistance.context_max_bytes must be 512..8192"
        );
        ensure!(
            (1..=30).contains(&self.assistance.teacher_timeout_secs),
            "assistance.teacher_timeout_secs must be 1..30"
        );
        self.solve.validate()?;
        self.escalation.validate()?;
        if self.provider.as_deref() == Some(crate::llm::chatgpt::PROVIDER) {
            ensure!(
                self.endpoint.as_ref().is_none_or(
                    |e| e.base_url == crate::llm::chatgpt::BASE_URL && e.api_key_env.is_none()
                ),
                "ChatGPT subscription uses official OAuth credentials only"
            );
        }
        if let Some(spec) = self.providers.get(crate::llm::chatgpt::PROVIDER) {
            ensure!(
                spec.base_url == crate::llm::chatgpt::BASE_URL && spec.api_key_env.is_none(),
                "ChatGPT provider cannot override its OAuth endpoint"
            );
        }
        ensure!(
            (1..=86400).contains(&self.worker_timeout_secs),
            "worker_timeout_secs must be 1..86400"
        );
        ensure!(
            (1..=86400).contains(&self.remote_model_idle_timeout_secs),
            "remote_model_idle_timeout_secs must be 1..86400"
        );
        ensure!(
            (1..=86400).contains(&self.reasoning_guard.max_pre_action_secs),
            "reasoning_guard.max_pre_action_secs must be 1..86400"
        );
        ensure!(
            (256..=1_048_576).contains(&self.reasoning_guard.max_reasoning_bytes),
            "reasoning_guard.max_reasoning_bytes must be 256..1048576"
        );
        ensure!(
            (1024..=24000).contains(&self.codebase.context_max_bytes),
            "codebase.context_max_bytes must be 1024..24000"
        );
        self.laya.validate()?;
        ensure!(
            (2048..=1_048_576).contains(&self.context_window),
            "context_window must be 2048..1048576 tokens"
        );
        ensure!(
            (1..=10000).contains(&self.ui.history_limit),
            "ui.history_limit must be 1..10000"
        );
        ensure!(
            (1..=50).contains(&self.memory.context_limit)
                && (256..=24000).contains(&self.memory.context_max_bytes),
            "invalid memory context bounds"
        );
        ensure!(
            (1..=3600).contains(&self.browser.timeout_secs)
                && !self.browser.node.trim().is_empty()
                && !self.browser.playwright_module.trim().is_empty(),
            "invalid browser program or timeout"
        );
        self.browser
            .validate_allowed_hosts()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            (1..=3600).contains(&self.kernel.timeout_secs) && !self.kernel.python.trim().is_empty(),
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
        for (role, selection) in &self.model_roles {
            ensure!(
                [
                    "feedback",
                    "subagent",
                    "compaction",
                    "architecture",
                    "experience",
                    "stronger"
                ]
                .contains(&role.as_str()),
                "unknown model role: {role}"
            );
            ensure!(
                !selection.model.trim().is_empty(),
                "model role {role} has no model"
            );
            if selection.provider == crate::llm::chatgpt::PROVIDER {
                ensure!(
                    selection
                        .base_url
                        .as_deref()
                        .is_none_or(|url| url == crate::llm::chatgpt::BASE_URL)
                        && selection.api_key_env.is_none(),
                    "ChatGPT roles use official OAuth credentials only"
                );
            }
            if let Some(url) = &selection.base_url {
                crate::llm::provider::endpoint_url(url, selection.allow_insecure)?;
            }
            ensure!(
                selection
                    .context_window
                    .is_none_or(|w| (2048..=1_048_576).contains(&w)),
                "invalid role context window"
            );
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
            ensure!(id != "mlx", "mlx is reserved for the local provider");
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
            (1..=10).contains(&i.max_candidates) && i.min_net_wins <= i.gate_primary_cases,
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
            "invalid live rollback threshold"
        );
        ensure!(
            self.improve.idle_minutes <= 1440 && self.improve.daily_compute_minutes <= 1440,
            "idle_minutes and daily_compute_minutes must be at most 1440"
        );
        for cap in [
            self.budget.max_session_cost_usd,
            self.budget.max_turn_cost_usd,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                cap.is_finite() && cap > 0.0,
                "budget caps must be positive numbers of USD"
            );
        }
        ensure!(self.hooks.len() <= 32, "at most 32 hooks are allowed");
        for hook in &self.hooks {
            crate::agent::hooks::validate(hook).map_err(|e| anyhow::anyhow!(e))?;
        }
        ensure!(
            self.verification.repair_attempts <= 2 && self.verification.commands.len() <= 16,
            "verification repair attempts must be 0..2 and at most 16 commands are allowed"
        );
        for check in &self.verification.commands {
            ensure!(valid_name(&check.name), "invalid verification command name");
            ensure!(
                !check.command.trim().is_empty() && check.command.len() <= 8192,
                "verification command must be 1..8192 bytes"
            );
            ensure!(
                (1..=600).contains(&check.timeout_secs),
                "verification command timeout must be 1..600 seconds"
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
                gate.max_regression.is_finite() && (0.0..=1.0).contains(&gate.max_regression),
                "max_regression must be 0..1 percentage points"
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
                (1..=3600).contains(&tool.timeout_secs),
                "tool timeout must be 1..3600 seconds"
            );
        }
        let mut integrations = BTreeSet::new();
        for name in &self.integrations {
            ensure!(
                INTEGRATION_TOOLS.contains(&name.as_str()),
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
                (1..=600).contains(&adapter.timeout_secs),
                "DAP adapter timeout must be 1..600 seconds"
            );
            ensure!(
                !adapter.program.as_os_str().is_empty(),
                "DAP adapter program cannot be empty"
            );
        }
        for (name, server) in &self.mcp {
            ensure!(
                server.url.is_some() != (!server.program.as_os_str().is_empty()),
                "mcp `{name}` needs exactly one of `program` and `url`"
            );
            if let Some(url) = &server.url {
                crate::tools::mcp::validate_url(url)
                    .map_err(|e| anyhow::anyhow!("mcp `{name}`: {e}"))?;
            } else {
                ensure!(
                    server.bearer_env.is_none() && server.headers.is_empty(),
                    "mcp `{name}`: bearer_env and headers apply only to url servers"
                );
            }
        }
        for (name, prompt) in &self.subagents {
            ensure!(
                valid_name(name) && !prompt.trim().is_empty() && prompt.len() <= 16384,
                "invalid subagent specification"
            );
            ensure!(
                !crate::agent::subagents::is_reserved(name),
                "subagent preset {name:?} collides with a built-in agent type"
            );
        }
        ensure!(
            self.agents.len() <= 32,
            "at most 32 custom agents are allowed"
        );
        for (name, agent) in &self.agents {
            ensure!(valid_name(name), "invalid custom agent name {name:?}");
            ensure!(
                !crate::agent::subagents::is_reserved(name) && !self.subagents.contains_key(name),
                "custom agent {name:?} collides with a built-in agent or preset"
            );
            let base = crate::agent::subagents::builtin(&agent.base).filter(|spec| !spec.mutates);
            ensure!(
                base.is_some(),
                "custom agent {name:?} must be based on scout, reviewer or planner"
            );
            ensure!(
                !agent.prompt.trim().is_empty() && agent.prompt.len() <= 16384,
                "custom agent {name:?} needs a prompt of 1..16384 bytes"
            );
            ensure!(
                agent.description.len() <= 200,
                "custom agent {name:?} description is too long"
            );
            ensure!(
                agent.max_steps.is_none_or(|n| (1..=40).contains(&n)),
                "custom agent {name:?} max_steps must be 1..=40"
            );
            for tool in agent.tools.iter().flatten() {
                ensure!(
                    base.is_some_and(|b| b.tools.contains(&tool.as_str())),
                    "custom agent {name:?}: tool {tool:?} is not available to a {} agent",
                    agent.base
                );
            }
        }
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
        ensure!(
            !h.system_prompt.trim().is_empty() && h.system_prompt.len() <= 16384,
            "system prompt must be 1..16384 bytes"
        );
        ensure!((1..=64).contains(&h.max_steps), "max_steps must be 1..64");
        ensure!(
            h.temperature.is_finite() && (0.0..=2.0).contains(&h.temperature),
            "temperature must be 0..2"
        );
        let output_limit = self
            .active_model_profile()
            .and_then(|p| p.max_tokens)
            .unwrap_or(8192);
        ensure!(
            (1..=output_limit.max(8192)).contains(&h.max_tokens),
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

    /// State, gate suites and training data must stay outside the project the agent can edit; this
    /// is what keeps sealed evaluation sealed from a tool-using agent.
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
            ensure!(
                !resolved.starts_with(&self.project_root),
                "protected path {} lies inside the project; keep it outside",
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
mod chatgpt_tests {
    use super::*;

    fn resolve(text: &str) -> Result<Config> {
        let project = tempfile::tempdir()?;
        let home = tempfile::tempdir()?;
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned())?;
        Config::from_table(&layout, toml::from_str(text)?)
    }

    #[test]
    fn subscription_main_and_roles_resolve_without_local_compute_or_api_keys() {
        let config = resolve("provider='chatgpt'\nmodel='account-model'\n[model_roles.compaction]\nprovider='chatgpt'\nmodel='account-small'\n[codebase]\narchitecture_model=''\n").unwrap();
        assert!(!config.uses_local_compute());
        for resolved in [config.clone(), config.for_role("compaction")] {
            let endpoint = resolved.endpoint.unwrap();
            assert_eq!(endpoint.base_url, crate::llm::chatgpt::BASE_URL);
            assert!(endpoint.api_key_env.is_none());
        }
    }
    #[test]
    fn migrates_saved_chatgpt_api_endpoint_to_codex_backend() {
        let main=resolve("provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://api.openai.com/v1'\n").unwrap();
        assert_eq!(
            main.endpoint.unwrap().base_url,
            crate::llm::chatgpt::BASE_URL
        );
        let provider =
            resolve("[providers.chatgpt]\nbase_url='https://api.openai.com/v1'\n").unwrap();
        assert_eq!(
            provider.providers["chatgpt"].base_url,
            crate::llm::chatgpt::BASE_URL
        );
        let role=resolve("model='local'\nprovider='mlx'\n[model_roles.compaction]\nprovider='chatgpt'\nmodel='account-model'\nbase_url='https://api.openai.com/v1'\n").unwrap();
        assert_eq!(
            role.model_roles["compaction"].base_url.as_deref(),
            Some(crate::llm::chatgpt::BASE_URL)
        );
    }

    #[test]
    fn budget_caps_must_be_positive_and_parse_from_toml() {
        let config = resolve("provider='chatgpt'\nmodel='m'\n[budget]\nmax_session_cost_usd=2.5\nmax_turn_cost_usd=0.5\n").unwrap();
        assert_eq!(
            (
                config.budget.max_session_cost_usd,
                config.budget.max_turn_cost_usd
            ),
            (Some(2.5), Some(0.5))
        );
        config.validate().unwrap();
        let mut bad = config;
        bad.budget.max_turn_cost_usd = Some(0.0);
        assert!(bad.validate().is_err());
        bad.budget.max_turn_cost_usd = Some(f64::NAN);
        assert!(bad.validate().is_err());
    }

    #[test]
    fn architecture_defaults_local_with_blank_fallback_and_explicit_role_override() {
        let mut config = resolve("provider='chatgpt'\nmodel='account-model'\n").unwrap();
        let local = config.for_role("architecture");
        assert_eq!(local.model, "mlx-community/Qwen2.5-3B-Instruct-4bit");
        assert_eq!(local.provider.as_deref(), Some("mlx"));
        assert!(local.endpoint.is_none());
        assert_eq!(local.context_window, 32768);
        // A cloud main model holds no local compute; the local architecture default must not make turns wait for training.
        assert!(!config.uses_local_compute());
        config.codebase.architecture_model.clear();
        assert_eq!(
            config.for_role("architecture").selection(),
            config.selection()
        );
        assert!(!config.uses_local_compute());
        config.codebase.architecture_model = "local-summary".into();
        config
            .model_roles
            .insert("architecture".into(), config.selection());
        assert_eq!(
            config.for_role("architecture").selection(),
            config.selection()
        );
        assert!(!config.uses_local_compute());
        config.model_roles.clear();
        config.codebase.synthesize_architecture = false;
        assert!(!config.uses_local_compute());
    }

    #[test]
    fn subscription_credentials_cannot_be_redirected_or_replaced_with_api_keys() {
        for text in [
            "provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://example.com/v1'\n",
            "provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://api.openai.com/v1'\napi_key_env='OPENAI_API_KEY'\n",
            "[model_roles.compaction]\nprovider='chatgpt'\nmodel='account-model'\nbase_url='https://example.com/v1'\n",
            "[model_roles.compaction]\nprovider='chatgpt'\nmodel='account-model'\napi_key_env='OPENAI_API_KEY'\n",
            "[providers.chatgpt]\nbase_url='https://example.com/v1'\n",
        ] {
            assert!(
                resolve(text).is_err(),
                "accepted unsafe subscription configuration"
            );
        }
    }
}

#[cfg(test)]
mod memory_migration_tests {
    use super::*;

    #[test]
    fn harness_migration_is_idempotent_and_keeps_unrelated_duplicates_for_validation() {
        let mut harness = Harness {
            tools: vec![
                "read".into(),
                "memory".into(),
                "retain".into(),
                "read".into(),
                "memory".into(),
            ],
            ..Default::default()
        };
        assert!(types::migrate_memory_tools(&mut harness));
        assert!(!harness.tools.iter().any(|tool| tool == "memory"));
        assert_eq!(
            harness
                .tools
                .iter()
                .filter(|tool| *tool == "retain")
                .count(),
            1
        );
        assert_eq!(
            harness.tools.iter().filter(|tool| *tool == "read").count(),
            2
        );
        for tool in [
            "recall",
            "retain",
            "memory_update",
            "forget",
            "memory_history",
            "memory_import",
        ] {
            assert!(harness.tools.iter().any(|selected| selected == tool));
        }
        let pointer = harness.tools.as_ptr();
        assert!(!types::migrate_memory_tools(&mut harness));
        assert_eq!(harness.tools.as_ptr(), pointer);
    }

    #[test]
    fn toml_migration_changes_harness_and_profiles_not_other_values_or_invalid_arrays() {
        let mut table: toml::Table = toml::from_str("[harness]\ntools=['memory','read']\n[model_profiles.deep]\ntools=['memory']\n[model_profiles.invalid]\ntools=['memory',17]\n[tools.operator]\nprogram='memory'\n").unwrap();
        assert!(migrate_memory_tool_table(&mut table));
        assert_eq!(
            table["tools"]["operator"]["program"].as_str(),
            Some("memory")
        );
        assert_eq!(
            table["model_profiles"]["invalid"]["tools"][0].as_str(),
            Some("memory")
        );
        assert!(
            table["harness"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.as_str() != Some("memory"))
        );
        assert!(
            table["model_profiles"]["deep"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.as_str() != Some("memory"))
        );
        assert!(!migrate_memory_tool_table(&mut table));
    }

    #[test]
    fn loading_persists_validated_memory_selector_cutover() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let path = layout.config_files()[0].clone();
        fs::write(
            &path,
            "use_case='preserved'\n[harness]\ntools=['memory','read']\n",
        )
        .unwrap();
        let config = Config::load(&layout, None).unwrap();
        assert!(!config.harness.tools.iter().any(|tool| tool == "memory"));
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["use_case"].as_str(), Some("preserved"));
        assert!(
            saved["harness"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.as_str() != Some("memory"))
        );
        assert_eq!(
            Config::load(&layout, None).unwrap().harness.tools,
            config.harness.tools
        );
    }

    #[test]
    fn invalid_configuration_is_not_rewritten_during_migration() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let path = layout.config_files()[0].clone();
        let original = "[harness]\ntools=['memory']\nmax_steps=0\n";
        fs::write(&path, original).unwrap();
        assert!(Config::load(&layout, None).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
    #[test]
    fn stale_selected_profile_is_ignored_without_deleting_its_definition() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
        let config_path = layout.config_files()[0].clone();
        fs::write(
            &config_path,
            r#"
provider = "mlx"
model = "current-model"
model_profile = "old"
[model_profiles.old]
provider = "mlx"
model = "previous-model"
max_tokens = 4096
"#,
        )
        .unwrap();
        let config = Config::load(&layout, None).unwrap();
        assert_eq!(
            config.model_profile, None,
            "stale profile selector is ignored"
        );
        assert!(
            config.model_profiles.contains_key("old"),
            "profile remains available when switching back"
        );
        assert_eq!(Config::load(&layout, None).unwrap().model_profile, None);
    }
}
