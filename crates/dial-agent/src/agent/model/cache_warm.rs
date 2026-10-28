//! Session binding for the prompt-cache warmer.
//!
//! The warmer's worker thread owns a backend of its own, built for the armed model, so no
//! backend is shared across threads. Replays are real requests: their usage is recorded under
//! `Purpose::CacheWarm` in the session's usage scope, so they appear in cost accounting.

use crate::agent::*;
use crate::llm::cache_warmer::worker::{Clock, Event, Factory, Handle, Replayer, Report, RunGuard};
use crate::llm::cache_warmer::{Mode, Stop, Tier};
use crate::llm::catalog::TokenPricing;
use crate::llm::usage::{Purpose, Scope};
use std::time::Instant;

/// Armed-prefix state for one session. Exists only while `cache.warm` is not `off`.
pub(in crate::agent) struct CacheWarm {
    handle: Handle,
    mode: Mode,
    model: String,
    pricing: Option<TokenPricing>,
}

impl CacheWarm {
    /// Brings the session's warmer in line with `config`, creating or dropping it as needed.
    /// Returns the worker's reports since the last call, for the caller to surface.
    pub(in crate::agent) fn sync(
        slot: &mut Option<CacheWarm>,
        config: &Config,
    ) -> Result<Vec<Stop>> {
        let mode = config.cache.warm;
        if mode == Mode::Off {
            *slot = None;
            return Ok(Vec::new());
        }
        let Some(warm) = slot.as_mut() else {
            *slot = Some(Self::start(config)?);
            return Ok(Vec::new());
        };
        if warm.mode != mode {
            warm.mode = mode;
            warm.handle.send(Event::ModeChanged(mode));
        }
        if warm.model != config.model {
            warm.model = config.model.clone();
            warm.pricing = pricing(config);
            warm.handle.send(Event::ModelChanged(warm.model.clone()));
        }
        Ok(warm
            .handle
            .drain()
            .into_iter()
            .filter_map(|report| match report {
                Report::Stopped(stop) => Some(stop),
                Report::Replayed { .. } => None,
            })
            .collect())
    }

    fn start(config: &Config) -> Result<Self> {
        let replay_config = config.clone();
        let factory: Factory = Box::new(move |model: &str| {
            let mut cfg = replay_config.clone();
            cfg.model = model.to_string();
            let backend = make_backend(&cfg)?;
            Ok(Box::new(BackendReplayer {
                backend,
                timeout_secs: cfg.worker_timeout_secs,
            }) as Box<dyn Replayer>)
        });
        let clock: Clock = Box::new(Instant::now);
        let handle = Handle::spawn(config.cache.warm, factory, clock)?;
        Ok(Self {
            handle,
            mode: config.cache.warm,
            model: config.model.clone(),
            pricing: pricing(config),
        })
    }

    /// Records the prefix a request just settled on. Skipped when the reply reports no prompt
    /// size, because the economics gate cannot be evaluated without it.
    pub(in crate::agent) fn settled(&self, chat: &Chat, prompt_tokens: Option<u64>, scope: &Scope) {
        let Some(prompt_tokens) = prompt_tokens else {
            return;
        };
        // Tier::Short is assumed: the default 5-minute cache. Anthropic's 1-hour writes are
        // priced higher, so the gate is conservative about them.
        self.handle.send(Event::Settled {
            chat: chat.clone(),
            model: self.model.clone(),
            prompt_tokens,
            pricing: self.pricing,
            tier: Tier::Short,
            scope: Some(scope.clone()),
        });
    }

    /// The conversation prefix was rewritten (compaction, rollover): disarm the armed prefix.
    pub(in crate::agent) fn context_changed(&self) {
        // A dropped event is harmless: the next settled request replaces the armed prefix.
        let _ = self.handle.send(Event::ContextChanged);
    }

    /// Guard that reports the end of the current agent run when dropped.
    pub(in crate::agent) fn run_guard(&self) -> RunGuard {
        self.handle.run_guard()
    }
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
