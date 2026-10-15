//! Model calls for commit generation: role selection, bounded retries, and reply parsing.

use std::thread;
use std::time::Duration;

use serde_json::json;

use super::cache::{
    self, CacheEntry, CacheKeyMaterial, ConventionalCache, FailureEntry, UsageRecord,
};
use super::config::GenerationConfig;
use super::run::{CommitBackend, ProviderBackend};
use crate::config::types::Config;
use crate::llm::profiles::ReasoningEffort;
use crate::llm::{Chat, Reply};

/// Output budget for one commit-analysis completion.
const MAX_REPLY_TOKENS: u32 = 4096;
const TEMPERATURE: f64 = 0.0;

/// Which configured model role serves a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Final synthesis and single-prompt analysis; uses the `small` role.
    Primary,
    /// Per-file map observations; uses the `small` role.
    Fast,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Primary => "primary",
            Tier::Fast => "fast",
        }
    }
}

/// Runs commit-generation completions against the configured model roles.
pub(crate) struct Inference<'a> {
    config: &'a Config,
    generation: &'a GenerationConfig,
    backend: &'a dyn CommitBackend,
    cache: Option<ConventionalCache>,
}

impl<'a> Inference<'a> {
    pub(crate) fn new(config: &'a Config, generation: &'a GenerationConfig) -> Self {
        Self::with_backend(&ProviderBackend, config, generation)
    }

    /// Like [`Inference::new`], with completions routed through `backend`.
    pub(crate) fn with_backend(
        backend: &'a dyn CommitBackend,
        config: &'a Config,
        generation: &'a GenerationConfig,
    ) -> Self {
        let cache = generation
            .cache_enabled
            .then(|| cache::default_path(&config.user_dir))
            .flatten()
            .and_then(|path| ConventionalCache::open(&path, generation.cache_ttl_days));
        Self {
            config,
            generation,
            backend,
            cache,
        }
    }

    /// Send `system`/`user` to the tier's model and parse the reply.
    ///
    /// Empty replies and parse failures count as attempts. Up to `max_retries` attempts run,
    /// with exponential backoff starting at `initial_backoff_ms`.
    pub(crate) fn complete<T>(
        &self,
        tier: Tier,
        system: &str,
        user: &str,
        parse: impl Fn(&Reply) -> Result<T, String>,
    ) -> Result<T, String> {
        let cfg = self.config_for();
        let operation = tier.label();
        let model = format!(
            "{}/{}",
            cfg.provider.as_deref().unwrap_or_default(),
            cfg.model
        );
        let api_mode = self.api_mode_for();
        let reasoning_effort = effective_reasoning_effort(&cfg);
        let key = cache::key_for(&CacheKeyMaterial {
            operation,
            model: &model,
            api_mode: &api_mode,
            tool_name: "",
            system_prompt: system,
            user_prompt: user,
            reasoning_effort: reasoning_effort.as_deref(),
        });
        if let Some(hit) = self.cache.as_ref().and_then(|cache| cache.get(&key)) {
            let reply = Reply {
                text: hit.text,
                finish_reason: hit.finish_reason,
                ..Reply::default()
            };
            if let Ok(value) = parse(&reply) {
                return Ok(value);
            }
        }
        let request_json = json!({"model": model, "system": system, "user": user}).to_string();
        let request = Chat {
            messages: vec![
                json!({"role": "system", "content": system}),
                json!({"role": "user", "content": user}),
            ],
            tools: vec![],
            temperature: TEMPERATURE,
            max_tokens: MAX_REPLY_TOKENS,
            seed: None,
        };
        let attempts = self.generation.max_retries.max(1);
        let mut last_error = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                let exponent = (attempt - 1).min(16);
                let backoff = self
                    .generation
                    .initial_backoff_ms
                    .saturating_mul(1u64 << exponent);
                thread::sleep(Duration::from_millis(backoff));
            }
            let completion = match self.backend.complete(&cfg, &request) {
                Ok(completion) => completion,
                Err(error) => {
                    self.record_failure(&key, &model, operation, &request_json, "", &error);
                    last_error = error;
                    continue;
                }
            };
            let cost_usd = completion.cost_usd;
            let reply = completion.reply;
            match parse(&reply) {
                Ok(value) => {
                    if let Some(cache) = &self.cache {
                        cache.put(&CacheEntry {
                            key: &key,
                            model: &model,
                            operation,
                            request: &request_json,
                            text: &reply.text,
                            finish_reason: &reply.finish_reason,
                            cost_usd,
                        });
                        cache.record_usage(&UsageRecord {
                            model: &model,
                            operation,
                            input_tokens: reply.prompt_tokens,
                            output_tokens: reply.completion_tokens,
                            cache_read_tokens: reply.cached_prompt_tokens,
                            cache_write_tokens: reply.cache_creation_tokens,
                            cost_usd,
                        });
                    }
                    return Ok(value);
                }
                Err(error) => {
                    self.record_failure(
                        &key,
                        &model,
                        operation,
                        &request_json,
                        &reply.text,
                        &error,
                    );
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    fn config_for(&self) -> Config {
        self.role_config("small")
            .unwrap_or_else(|| self.config.clone())
    }

    fn role_config(&self, role: &str) -> Option<Config> {
        self.config
            .model_roles
            .get(role)
            .map(|selection| self.config.with_selection(selection))
    }

    /// Wire protocol of the `small` role's selected model, for the cache key; `default` when unset.
    fn api_mode_for(&self) -> String {
        let selection = self.config.model_roles.get("small");
        selection
            .and_then(|selection| selection.protocol)
            .map_or_else(|| "default".to_owned(), |protocol| format!("{protocol:?}"))
    }

    /// Record a failed attempt in the cache; a no-op when caching is disabled.
    fn record_failure(
        &self,
        key: &str,
        model: &str,
        operation: &str,
        request: &str,
        response: &str,
        error: &str,
    ) {
        if let Some(cache) = &self.cache {
            cache.put_failure(&FailureEntry {
                key,
                model,
                operation,
                request,
                response,
                error,
            });
        }
    }
}

/// Reasoning effort the provider receives for `cfg`'s active profile, for the cache key.
/// Mirrors the request builders: an explicit effort wins; `thinking = false` means `none`.
fn effective_reasoning_effort(cfg: &Config) -> Option<String> {
    let profile = cfg.active_model_profile()?;
    profile
        .reasoning_effort
        .or_else(|| (profile.thinking == Some(false)).then_some(ReasoningEffort::None))
        .map(|effort| effort.as_str().to_owned())
}
