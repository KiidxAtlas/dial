//! Prompt-cache warming for native Anthropic prefixes. A cache entry expires after its lifetime;
//! replaying the same request shortly before expiry reads the whole prefix at the cheap read rate.
//! That is worth paying only when another real turn is likely and the miss it prevents costs more
//! than the replay. This module is pure policy: the caller owns the clock, the replay request and
//! the session's cost accounting.
use super::Chat;
use super::catalog::{ONE_HOUR_WRITE_INPUT_MULTIPLIER, TokenPricing};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub mod worker;

/// Anthropic's default cache lifetime.
pub const SHORT_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Anthropic's extended (`ttl: "1h"`) cache lifetime.
pub const LONG_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// Replays land this long before expiry, so a slow request cannot let the entry lapse first.
const REFRESH_LEAD: Duration = Duration::from_secs(60);
/// Floor between replays, so a short lifetime cannot turn warming into a loop.
const MIN_REFRESH_GAP: Duration = Duration::from_secs(30);
/// Probability that another real turn follows while a run is streaming.
const STREAMING_CONTINUATION: f64 = 0.9;
/// Probability that the user returns to an idle session at all.
const IDLE_CONTINUATION: f64 = 0.2;
/// Replays stop this long after the last real turn, however profitable each one looks.
const STREAMING_WINDOW: Duration = Duration::from_secs(30 * 60);
const IDLE_WINDOW: Duration = Duration::from_secs(60 * 60);
/// A replay must be expected to save at least this many USD to be sent.
pub const MIN_NET_SAVINGS_USD: f64 = 0.001;

/// When warming runs: never, while a run is streaming, or also while the session is idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Streaming,
    Idle,
}

impl Mode {
    fn continuation(self) -> f64 {
        match self {
            Self::Off | Self::Streaming => STREAMING_CONTINUATION,
            Self::Idle => IDLE_CONTINUATION,
        }
    }

    fn window(self) -> Duration {
        match self {
            Self::Off | Self::Streaming => STREAMING_WINDOW,
            Self::Idle => IDLE_WINDOW,
        }
    }
}

/// Which Anthropic cache lifetime the armed request was written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Short,
    Long,
}

impl Tier {
    pub fn lifetime(self) -> Duration {
        match self {
            Self::Short => SHORT_LIFETIME,
            Self::Long => LONG_LIFETIME,
        }
    }
}

/// USD cost of one replay, and the extra USD a cache miss on the next real turn would add.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Economics {
    pub replay_usd: f64,
    pub miss_usd: f64,
}

/// Prices one replay of a `prompt_tokens` prefix (a cached read plus one output token) against
/// the miss it prevents (re-writing the prefix). `None` when the card lacks the needed rates.
pub fn economics(pricing: TokenPricing, prompt_tokens: u64, tier: Tier) -> Option<Economics> {
    let read = pricing.cache_read?;
    let write = match tier {
        Tier::Short => pricing.cache_write?,
        Tier::Long => pricing.prompt * ONE_HOUR_WRITE_INPUT_MULTIPLIER,
    };
    let tokens = prompt_tokens as f64;
    let economics = Economics {
        replay_usd: tokens * read + pricing.completion,
        miss_usd: (write - read).max(0.0) * tokens,
    };
    (economics.replay_usd.is_finite() && economics.miss_usd.is_finite()).then_some(economics)
}

/// Whether a replay is expected to pay for itself under the mode's continuation odds.
pub fn worth_replaying(economics: Economics, mode: Mode) -> bool {
    mode.continuation() * economics.miss_usd - economics.replay_usd >= MIN_NET_SAVINGS_USD
}

/// Why warming stopped or refused to start. Each stop disarms the warmer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The mode is `off`.
    Mode,
    /// The price card lacks cache read or write rates, so nothing can be priced.
    Unpriced,
    /// Replays no longer pay for themselves.
    NotWorthIt,
    /// The window after the last real turn has elapsed.
    WindowElapsed,
    /// A replay read nothing from the cache, so the prefix is gone and replays would only pay.
    Miss,
    /// A `streaming` warmer ends with the run that armed it.
    RunEnded,
    /// The context or model changed, so the armed prefix no longer matches the next request.
    Changed,
    /// The replay backend could not be built or failed, so warming cannot continue.
    Unavailable,
}

struct Armed {
    chat: Chat,
    mode: Mode,
    tier: Tier,
    economics: Economics,
    settled_at: Instant,
    last_refresh: Instant,
}

impl Armed {
    /// The instant the replay falls due: lifetime minus the lead, with a floor between replays.
    fn refresh_at(&self) -> Instant {
        let lead = self.tier.lifetime().saturating_sub(REFRESH_LEAD);
        self.last_refresh + lead.max(MIN_REFRESH_GAP)
    }
}

/// Tracks the most recent settled request and decides when its cache prefix should be replayed.
#[derive(Default)]
pub struct Warmer {
    armed: Option<Armed>,
}

impl Warmer {
    /// Arms warming for the request that just settled. A new real turn always replaces the last.
    pub fn arm(
        &mut self,
        chat: &Chat,
        prompt_tokens: u64,
        pricing: Option<TokenPricing>,
        tier: Tier,
        mode: Mode,
        now: Instant,
    ) -> Result<(), Stop> {
        self.armed = None;
        if mode == Mode::Off {
            return Err(Stop::Mode);
        }
        let economics = pricing
            .and_then(|pricing| economics(pricing, prompt_tokens, tier))
            .ok_or(Stop::Unpriced)?;
        if !worth_replaying(economics, mode) {
            return Err(Stop::NotWorthIt);
        }
        self.armed = Some(Armed {
            chat: chat.clone(),
            mode,
            tier,
            economics,
            settled_at: now,
            last_refresh: now,
        });
        Ok(())
    }

    /// When the next replay is due, or `None` while disarmed.
    pub fn due(&self) -> Option<Instant> {
        self.armed.as_ref().map(Armed::refresh_at)
    }

    /// The replay to send at `now`, or `Ok(None)` when none is due. The replay is the settled
    /// request capped at one output token, so it reads the prefix and writes nothing new.
    pub fn replay(&mut self, now: Instant) -> Result<Option<Chat>, Stop> {
        let Some(armed) = self.armed.as_mut() else {
            return Ok(None);
        };
        if now.duration_since(armed.settled_at) >= armed.mode.window() {
            self.armed = None;
            return Err(Stop::WindowElapsed);
        }
        if now < armed.refresh_at() {
            return Ok(None);
        }
        armed.last_refresh = now;
        let mut replay = armed.chat.clone();
        replay.max_tokens = 1;
        Ok(Some(replay))
    }

    /// Stops warming without waiting for a stop condition, e.g. when the session ends.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    /// Applies a mode change to the armed prefix. `Off` disarms; a mode that no longer pays for
    /// the replays disarms too.
    pub fn set_mode(&mut self, mode: Mode) -> Option<Stop> {
        if mode == Mode::Off {
            self.armed = None;
            return Some(Stop::Mode);
        }
        let armed = self.armed.as_mut()?;
        if !worth_replaying(armed.economics, mode) {
            self.armed = None;
            return Some(Stop::NotWorthIt);
        }
        armed.mode = mode;
        None
    }

    /// The agent run ended. A `streaming` warmer stops here; an `idle` warmer keeps going.
    pub fn run_ended(&mut self) -> Option<Stop> {
        if self.armed.as_ref()?.mode == Mode::Streaming {
            self.armed = None;
            return Some(Stop::RunEnded);
        }
        None
    }

    /// Records a replay's cache read. Reading nothing means the prefix was evicted or changed,
    /// so replaying further only pays for misses; the warmer disarms.
    pub fn record_replay(&mut self, cache_read_tokens: u64) -> Option<Stop> {
        if cache_read_tokens == 0 {
            self.armed = None;
            return Some(Stop::Miss);
        }
        None
    }
}

#[cfg(test)]
mod tests;
