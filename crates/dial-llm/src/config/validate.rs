//! `Config::validate`: cross-field configuration checks, grouped by settings area.
//!
//! `validate` runs the groups in a fixed order, so the first reported error stays stable.

use super::{
    Config, Harness, SERVICE_TIERS, schema,
    tools::{BUILTIN, EXTRA_TOOLS, integrations::INTEGRATION_TOOLS},
    types,
};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeSet, fs, path::Path};

impl Config {
    pub fn validate(&self) -> Result<()> {
        self.validate_settings_schema()?;
        self.validate_request_headers()?;
        self.validate_feature_bounds()?;
        self.validate_subscription_endpoints()?;
        self.validate_timeouts_and_windows()?;
        self.validate_runtimes()?;
        self.validate_model_profiles()?;
        self.validate_model_roles()?;
        self.validate_fallback_chains()?;
        self.validate_providers()?;
        self.validate_improve()?;
        self.validate_budget_and_hooks()?;
        self.validate_verification_commands()?;
        self.validate_verification_inputs()?;
        self.validate_verification_requirements()?;
        self.validate_endpoint_and_gate()?;
        self.validate_tool_integrations()?;
        self.validate_agents()?;
        self.validate_task_and_training()?;
        self.validate_protected()?;
        self.validate_harness(&self.harness)
    }

    /// Registry-declared value checks plus sections that own their own validation.
    fn validate_settings_schema(&self) -> Result<()> {
        let values = toml::Table::try_from(self)?;
        for setting in schema::registry() {
            if let Some(value) = schema::get(&values, setting.path) {
                schema::check_value_for(&values, setting.path, value)
                    .map_err(anyhow::Error::msg)?;
            }
        }
        self.edit.validate().map_err(anyhow::Error::msg)?;
        self.secrets.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            SERVICE_TIERS.contains(&self.service_tier.as_str()),
            "service_tier must be one of: {}",
            SERVICE_TIERS.join(", ")
        );
        self.compaction.validate()?;
        Ok(())
    }

    /// Custom request headers and ChatGPT account pinning for every configured selection.
    fn validate_request_headers(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_feature_bounds(&self) -> Result<()> {
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
        Ok(())
    }

    /// Subscription providers must keep their official OAuth endpoints and protocols.
    fn validate_subscription_endpoints(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_timeouts_and_windows(&self) -> Result<()> {
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
        Ok(())
    }

    /// Memory backend and the browser, eval and kernel runtimes.
    fn validate_runtimes(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_model_profiles(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_model_roles(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_fallback_chains(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_providers(&self) -> Result<()> {
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
            ensure!(
                provider.api_key_envs.len() <= 16
                    && provider.api_key_envs.iter().all(|name| {
                        !name.trim().is_empty()
                            && name.len() <= 256
                            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    }),
                "provider {id} api_key_envs must list at most 16 environment variable names"
            );
        }
        Ok(())
    }

    fn validate_improve(&self) -> Result<()> {
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
        ensure!(
            i.lora_min_examples >= 1,
            "lora_min_examples must be positive"
        );
        Ok(())
    }

    fn validate_budget_and_hooks(&self) -> Result<()> {
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
        Ok(())
    }

    /// Verification commands, behavioral checks and runner declarations.
    fn validate_verification_commands(&self) -> Result<()> {
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
            self.verification.runners.len() <= 16,
            "at most 16 verification runner declarations are allowed"
        );
        for (name, runner) in &self.verification.runners {
            ensure!(
                self.verification
                    .commands
                    .iter()
                    .filter(|check| &check.name == name)
                    .count()
                    == 1,
                "verification runner must name one unique configured command"
            );
            ensure!(
                !runner.program.is_empty()
                    && runner.program.len() <= 1024
                    && !runner
                        .program
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace())
                    && runner
                        .module
                        .as_ref()
                        .is_none_or(|module| !module.is_empty()
                            && module.len() <= 256
                            && module
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))),
                "verification runner needs an executable path/name and optional dotted module name"
            );
        }
        Ok(())
    }

    fn validate_verification_inputs(&self) -> Result<()> {
        ensure!(
            self.verification.inputs.len() <= 16,
            "at most 16 verification input declarations are allowed"
        );
        for (name, inputs) in &self.verification.inputs {
            let Some(check) = self
                .verification
                .commands
                .iter()
                .find(|check| &check.name == name)
            else {
                bail!("verification.inputs must name a configured command");
            };
            ensure!(
                self.verification
                    .commands
                    .iter()
                    .filter(|other| &other.name == name || other.command == check.command)
                    .count()
                    == 1,
                "verification.inputs must name one command with a unique name and command string"
            );
            ensure!(
                (1..=64).contains(&inputs.paths.len())
                    && inputs.paths.iter().all(|path| {
                        !path.is_empty()
                            && path.len() <= 1024
                            && !path.contains('\\')
                            && std::path::Path::new(path)
                                .components()
                                .all(|part| matches!(part, std::path::Component::Normal(_)))
                            && !path
                                .chars()
                                .any(|c| c.is_control() || matches!(c, '*' | '?' | '[' | ']'))
                            && path
                                .split('/')
                                .all(|part| !part.is_empty() && part != "." && part != "..")
                    })
                    && inputs
                        .paths
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == inputs.paths.len(),
                "verification.inputs paths must be 1..64 unique workspace-relative normal paths without globs"
            );
            ensure!(
                inputs.environment.len() <= 64
                    && inputs.environment.iter().all(|var| {
                        var.len() <= 256
                            && var
                                .bytes()
                                .next()
                                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                            && var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    && inputs
                        .environment
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == inputs.environment.len(),
                "verification.inputs environment must be at most 64 unique portable variable names"
            );
        }
        Ok(())
    }

    /// Review policy and named verification requirements.
    fn validate_verification_requirements(&self) -> Result<()> {
        let review = &self.verification.review;
        ensure!(
            (1..=1000).contains(&review.min_changed_files) && (1..=3).contains(&review.max_rounds),
            "verification.review requires min_changed_files=1..1000 and max_rounds=1..3"
        );
        ensure!(
            review.sensitive_paths.len() <= 32
                && review.sensitive_paths.iter().all(|path| {
                    !path.is_empty()
                        && path.len() <= 256
                        && !path.starts_with('/')
                        && !path.contains('\\')
                        && !path.chars().any(char::is_control)
                        && path
                            .split('/')
                            .all(|part| !part.is_empty() && part != "." && part != "..")
                }),
            "verification.review.sensitive_paths must be bounded workspace-relative path prefixes"
        );
        ensure!(
            !self.verification.strict_requirements || self.verification.enabled,
            "strict requirement settlement requires verification.enabled"
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
        Ok(())
    }

    fn validate_endpoint_and_gate(&self) -> Result<()> {
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
        Ok(())
    }

    /// Operator tools, integration tools, DAP adapters and MCP servers.
    fn validate_tool_integrations(&self) -> Result<()> {
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
        Ok(())
    }

    /// Subagent presets and custom agent definitions.
    fn validate_agents(&self) -> Result<()> {
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
        Ok(())
    }

    fn validate_task_and_training(&self) -> Result<()> {
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
        Ok(())
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
    pub(super) fn validate_protected(&self) -> Result<()> {
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
