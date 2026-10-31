//! Session binding for the prompt-cache warmer.
//!
//! The warmer's worker thread owns a backend of its own, built for the armed route, so no
//! backend is shared across threads. Replays are real requests: their usage is recorded under
//! `Purpose::CacheWarm` in the session's usage scope, so they appear in cost accounting.

use crate::agent::*;
use crate::config::types::EndpointConfig;
use crate::llm::cache_warmer::worker::{Clock, Event, Factory, Handle, Replayer, Report, RunGuard};
use crate::llm::cache_warmer::{Mode, Stop};
use crate::llm::catalog::TokenPricing;
use crate::llm::model_config::{ModelSelection, RequestHeaders};
use crate::llm::usage::{Purpose, Scope};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Instant;

/// The live route a warmed prefix was settled on. A replay must reach the same provider, endpoint,
/// credentials, profile and request headers, so any change to these makes the armed worker stale.
/// Provider and profile settings enter as serialized snapshots of the entries the route uses, never
/// as `Debug` output and never as the whole configuration.
#[derive(Debug, PartialEq)]
struct WarmRoute {
    selection: ModelSelection,
    context_window: u32,
    endpoint: Option<EndpointConfig>,
    request_headers: RequestHeaders,
    service_tier: String,
    extended_context: bool,
    user_dir: PathBuf,
    replay_timeout_secs: u64,
    state_dir: PathBuf,
    provider_spec: Option<Value>,
    model_profile: Option<Value>,
}

impl WarmRoute {
    /// The route `config` resolves to. `config` is the live resolved configuration. The profile is
    /// the one the backend binds, including an auto-selected match, and the headers are the
    /// effective ones, including headers inherited from the provider entry.
    fn of(config: &Config) -> Result<Self> {
        let selection = config.selection();
        let provider_spec = config
            .providers
            .get(&selection.provider)
            .map(serde_json::to_value)
            .transpose()?;
        let model_profile = config
            .active_model_profile()
            .map(serde_json::to_value)
            .transpose()?;
        Ok(Self {
            selection,
            context_window: config.context_window,
            endpoint: config.endpoint.clone(),
            request_headers: config.request_headers().to_vec(),
            service_tier: config.effective_service_tier().to_owned(),
            extended_context: config.extended_context,
            user_dir: config.user_dir.clone(),
            state_dir: config.state_dir.clone(),
            replay_timeout_secs: replay_timeout(config.worker_timeout_secs),
            provider_spec,
            model_profile,
        })
    }
}

/// Armed-prefix state for one session. Exists only while `cache.warm` is not `off`.
pub(in crate::agent) struct CacheWarm {
    handle: Handle,
    mode: Mode,
    route: WarmRoute,
    pricing: Option<TokenPricing>,
}

impl CacheWarm {
    /// Brings the session's warmer in line with the live route `config`, creating, restarting or
    /// dropping it as needed. A route change or a stopped worker replaces the worker, so a replay
    /// never reuses a stale factory. The old worker is dropped before a replacement starts, and a
    /// replacement is only stored after it was built. Returns the worker's reports since the last
    /// call.
    pub(in crate::agent) fn sync(
        slot: &mut Option<CacheWarm>,
        config: &Config,
    ) -> Result<Vec<Stop>> {
        let mode = config.cache.warm;
        if mode == Mode::Off {
            *slot = None;
            return Ok(Vec::new());
        }
        let route = WarmRoute::of(config)?;
        let mut stops = Vec::new();
        if let Some(warm) = slot
            .as_ref()
            .filter(|warm| warm.route != route || !warm.handle.is_running())
        {
            stops.extend(stopped(warm.handle.drain()));
            *slot = None;
        }
        let Some(warm) = slot.as_mut() else {
            *slot = Some(Self::start(config, route)?);
            return Ok(stops);
        };
        if warm.mode != mode {
            warm.mode = mode;
            warm.handle.send(Event::ModeChanged(mode));
        }
        stops.extend(stopped(warm.handle.drain()));
        Ok(stops)
    }

    fn start(config: &Config, route: WarmRoute) -> Result<Self> {
        let replay_config = config.clone();
        let factory: Factory = Box::new(move |model: &str| {
            let mut cfg = replay_config.clone();
            cfg.model = model.to_string();
            // Warming is best-effort: a failed replay stops the warmer instead of waiting out
            // provider retries, so one replay never holds the worker for long.
            cfg.retry.max_attempts = 1;
            let backend = make_backend(&cfg)?;
            Ok(Box::new(BackendReplayer {
                backend,
                timeout_secs: replay_timeout(cfg.worker_timeout_secs),
            }) as Box<dyn Replayer>)
        });
        let clock: Clock = Box::new(Instant::now);
        let handle = Handle::spawn(config.cache.warm, factory, clock)?;
        Ok(Self {
            handle,
            mode: config.cache.warm,
            route,
            pricing: pricing(config),
        })
    }

    /// Records an accepted request's prefix. Unknown prompt size or retention disarms the
    /// previous prefix: neither its economics nor its expiry can safely be assumed.
    pub(in crate::agent) fn settled(&self, chat: &Chat, reply: &llm::Reply, scope: &Scope) {
        let (Some(prompt_tokens), Some(tier)) = (reply.prompt_tokens, reply.cache_warm_tier) else {
            self.context_changed();
            return;
        };
        self.handle.send(Event::Settled {
            chat: chat.clone(),
            model: self.route.selection.model.clone(),
            prompt_tokens,
            pricing: self.pricing,
            tier,
            scope: Some(scope.clone()),
        });
    }

    /// The conversation prefix was rewritten (compaction, rollover): disarm the armed prefix.
    pub(in crate::agent) fn context_changed(&self) {
        // Overflow fails closed in the handle, so a lost invalidation stops future replays.
        let _ = self.handle.send(Event::ContextChanged);
    }

    /// Guard that reports the end of the current agent run when dropped.
    pub(in crate::agent) fn run_guard(&self) -> RunGuard {
        self.handle.run_guard()
    }
}

/// Stop reports from a batch of worker reports, for the caller to surface.
fn stopped(reports: Vec<Report>) -> Vec<Stop> {
    reports
        .into_iter()
        .filter_map(|report| match report {
            Report::Stopped(stop) => Some(stop),
            Report::Replayed { .. } => None,
        })
        .collect()
}

/// Operator-facing reason a warmer stopped, shown once at the next turn.
pub(in crate::agent) fn describe(stop: Stop) -> &'static str {
    match stop {
        Stop::Mode => "cache.warm was turned off",
        Stop::Unpriced => "model pricing is unknown",
        Stop::NotWorthIt => "replaying the prefix would not save money",
        Stop::WindowElapsed => "the warming window ended",
        Stop::Miss => "a replay wrote the cache instead of reading it",
        Stop::RunEnded => "the streaming run ended",
        Stop::Changed => "the context changed",
        Stop::Unavailable => "the warming backend is unavailable",
    }
}

/// Model pricing from the catalog, read the same way the agent's backend reads it.
fn pricing(config: &Config) -> Option<TokenPricing> {
    let layout = Layout {
        project_root: config.project_root.clone(),
        project_dir: config.project_root.join(".dial"),
        user_dir: config.user_dir.clone(),
        state_dir: config.state_dir.clone(),
    };
    let mut selection = config.selection();
    if selection.headers.is_none() {
        selection.headers = Some(config.request_headers().to_vec());
    }
    llm::catalog::model_metadata(&layout, &selection).pricing
}

/// Longest a one-token replay may take; `worker_timeout_secs` may only shorten it.
const REPLAY_TIMEOUT_SECS: u64 = 120;

fn replay_timeout(worker_timeout_secs: u64) -> u64 {
    match worker_timeout_secs {
        0 => REPLAY_TIMEOUT_SECS,
        secs => secs.min(REPLAY_TIMEOUT_SECS),
    }
}

/// Replays a settled request on a backend owned by the worker thread.
struct BackendReplayer {
    backend: Backend,
    timeout_secs: u64,
}

impl Replayer for BackendReplayer {
    fn replay(&mut self, chat: &Chat, scope: Option<&Scope>) -> Result<Option<u64>> {
        let previous = self
            .backend
            .set_usage_scope(scope.map(|scope| scope.for_purpose(Purpose::CacheWarm)));
        let reply = self.backend.chat(None, chat, self.timeout_secs, None);
        self.backend.set_usage_scope(previous);
        Ok(reply?.cached_prompt_tokens)
    }
}

#[cfg(test)]
mod tests;
