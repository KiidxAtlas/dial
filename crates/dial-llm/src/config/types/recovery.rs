//! Operator-owned request limits and recovery: the reasoning guard, silent-request recovery and retry.
use super::*;

/// Operator-owned limits; harness improvement cannot increase them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningGuardConfig {
    pub enabled: bool,
    pub max_pre_action_secs: u64,
    /// Bytes of streamed reasoning, not an estimate of hidden reasoning tokens.
    pub max_reasoning_bytes: usize,
    /// After a gate hit with no open todo, only `todo` is offered until a plan exists.
    pub require_plan: bool,
}

impl Default for ReasoningGuardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_pre_action_secs: 0,
            max_reasoning_bytes: 0,
            require_plan: false,
        }
    }
}

/// Operator-owned recovery for a silent inference attempt, not a task/step limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestRecoveryConfig {
    /// Seconds without a model-worker event before cancelling just that attempt; 0 disables the watchdog.
    pub silence_secs: u64,
    /// Retry the same request once automatically; subsequent recovery needs user direction.
    pub retry_once: bool,
}
impl Default for RequestRecoveryConfig {
    fn default() -> Self {
        Self {
            silence_secs: 120,
            retry_once: true,
        }
    }
}

/// Provider-request retry policy (`[retry]`). Applies to transient transport failures, throttles and
/// incomplete streams within one model request; quota exhaustion is never waited out unless opted in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    /// Attempts per model request, including the first (1..=50).
    pub max_attempts: u32,
    /// Longest single server-requested or backoff wait honoured before giving up (1..=3600 s).
    pub max_delay_secs: u64,
    /// Wait for a usage-quota window to reset (bounded by `max_delay_secs`) instead of failing fast.
    pub wait_for_quota_reset: bool,
    /// After a refreshed subscription credential is still rejected (or its quota is exhausted), retry
    /// the request once with another signed-in, policy-allowed account of the same provider.
    pub rotate_accounts: bool,
    /// Ordered selectors attempted after a terminal provider failure. Keys are role names or
    /// exact `provider/model` identities, or `provider/*` wildcards (matched last); values may be
    /// bare models, qualified models, or `@role`.
    pub fallback_chains: std::collections::BTreeMap<String, Vec<String>>,
    /// Which terminal failures move a request on to its fallback chain.
    pub fallback_on: FallbackOn,
    /// When a session returns to its configured primary after failing over to a fallback.
    pub fallback_revert: FallbackRevert,
}

/// Failure classes that trigger `[retry.fallback_chains]` (`retry.fallback_on`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackOn {
    /// Throttling, quota exhaustion, transient failures and rejected credentials.
    #[default]
    Any,
    /// Only throttling and quota exhaustion.
    UsageLimit,
    /// Transient failures and rejected credentials, never usage limits.
    ExceptUsageLimit,
}

/// When a session returns to its configured primary after it failed over to a fallback model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackRevert {
    /// Return to the primary at the first request boundary after the fallback's cooldown elapses.
    #[default]
    CooldownExpiry,
    /// Stay on the fallback until the user switches models.
    Never,
}
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            max_delay_secs: 300,
            wait_for_quota_reset: false,
            rotate_accounts: true,
            fallback_chains: std::collections::BTreeMap::new(),
            fallback_on: FallbackOn::default(),
            fallback_revert: FallbackRevert::default(),
        }
    }
}
