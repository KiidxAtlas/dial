//! A process-local pool of API keys for one endpoint.
//!
//! A provider may name several key variables (`api_key_env` plus `api_key_envs`). When the active
//! key is throttled or its quota is spent, it is blocked until the server's retry/reset deadline
//! (or [`DEFAULT_COOLDOWN`]) and the request moves to the next unblocked key. The working key
//! stays active across requests (affinity); a blocked key is not preferred again by flipping back.
//! Only variable *names* are tracked here, never key values, so nothing in this module can leak
//! a credential to a log.
use super::errors::ErrorClass;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

/// Block length when the provider gave no retry or reset time.
pub(crate) const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);
/// A provider-stated deadline beyond this is not trusted for blocking.
const MAX_COOLDOWN: Duration = Duration::from_secs(3600);

/// Deadlines before which a key (identified by endpoint scope and variable name) is not used.
#[derive(Default)]
pub(crate) struct Blocks(Mutex<HashMap<(String, String), Instant>>);

impl Blocks {
    pub(crate) fn block(&self, scope: &str, env: &str, until: Instant) {
        let mut blocked = self.0.lock();
        let slot = blocked
            .entry((scope.to_owned(), env.to_owned()))
            .or_insert(until);
        *slot = (*slot).max(until);
    }

    pub(crate) fn is_blocked(&self, scope: &str, env: &str, now: Instant) -> bool {
        let mut blocked = self.0.lock();
        blocked.retain(|_, until| *until > now);
        blocked.contains_key(&(scope.to_owned(), env.to_owned()))
    }
}

static PROCESS: LazyLock<Blocks> = LazyLock::new(Blocks::default);

/// Blocks shared by every backend in this process.
pub(crate) fn process_blocks() -> &'static Blocks {
    &PROCESS
}

/// Whether the variable is set to a nonempty value (so rotating to it can authenticate).
pub(crate) fn env_present(env: &str) -> bool {
    std::env::var(env).is_ok_and(|value| !value.is_empty())
}

/// How long to block a key that failed with `class`: the server's retry or reset deadline when it
/// gave one, otherwise [`DEFAULT_COOLDOWN`].
pub(crate) fn cooldown(
    class: &ErrorClass,
    hinted: Option<Duration>,
    now: SystemTime,
) -> Option<Duration> {
    let stated = match class {
        ErrorClass::RateLimited { retry_after } => hinted.max(*retry_after),
        ErrorClass::QuotaExhausted { reset_at } => reset_at
            .and_then(|at| at.duration_since(now).ok())
            .or(hinted),
        _ => return None,
    };
    Some(
        stated
            .filter(|wait| !wait.is_zero())
            .unwrap_or(DEFAULT_COOLDOWN)
            .min(MAX_COOLDOWN),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyPool {
    envs: Vec<String>,
    active: usize,
}

impl KeyPool {
    /// A pool of the distinct nonblank variables in order; fewer than two is not a pool.
    pub(crate) fn new(envs: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut unique: Vec<String> = Vec::new();
        for env in envs {
            let env = env.trim().to_owned();
            if !env.is_empty() && !unique.contains(&env) {
                unique.push(env);
            }
        }
        (unique.len() >= 2).then_some(Self {
            envs: unique,
            active: 0,
        })
    }

    /// The variable naming the key requests currently use.
    pub(crate) fn active(&self) -> &str {
        &self.envs[self.active]
    }

    fn next_available(
        &self,
        blocks: &Blocks,
        scope: &str,
        now: Instant,
        usable: &impl Fn(&str) -> bool,
    ) -> Option<usize> {
        (1..self.envs.len())
            .map(|offset| (self.active + offset) % self.envs.len())
            .find(|&index| {
                let env = &self.envs[index];
                !blocks.is_blocked(scope, env, now) && usable(env)
            })
    }

    /// Before a request: leave a key that another request already blocked.
    pub(crate) fn settle(
        &mut self,
        blocks: &Blocks,
        scope: &str,
        now: Instant,
        usable: impl Fn(&str) -> bool,
    ) {
        if blocks.is_blocked(scope, self.active(), now)
            && let Some(index) = self.next_available(blocks, scope, now, &usable)
        {
            self.active = index;
        }
    }

    /// Block the active key for `cooldown` and move to the next unblocked usable key. Returns the
    /// new variable name, or `None` (with the key still blocked) when every other key is unavailable.
    pub(crate) fn rotate(
        &mut self,
        blocks: &Blocks,
        scope: &str,
        now: Instant,
        cooldown: Duration,
        usable: impl Fn(&str) -> bool,
    ) -> Option<&str> {
        blocks.block(scope, self.active(), now + cooldown);
        let index = self.next_available(blocks, scope, now, &usable)?;
        self.active = index;
        Some(self.active())
    }
}

#[cfg(test)]
mod tests;
