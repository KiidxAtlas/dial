//! The bounds of an explicit model diagnostic (the text response test and the coding/tool check):
//! one attempt on the configured route, a fixed budget, the request backend with its output clamped,
//! and one cancel flag that both an interrupt and the budget set. The diagnostics own their requests
//! and verdicts in the shell; this module owns only how they are bounded.

use crate::agent::make_backend;
use crate::config::types::Config;
use crate::llm::provider::Backend;
use anyhow::{Result, ensure};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(test)]
mod tests;

/// Wall-clock budget of one diagnostic, shown to the user in the disclosure.
pub const BUDGET: Duration = Duration::from_secs(BUDGET_SECS);
/// The same budget in the seconds the provider request and the worker timeout take.
pub const BUDGET_SECS: u64 = 20;

/// The backend for one diagnostic request. The request runs under one attempt, with no account
/// rotation, quota wait or fallback chain, and the diagnostic's own worker timeout. Its profile's
/// output limit is clamped to `max_tokens` (the limit the request itself sends), and it never
/// inherits a larger configured limit.
pub fn backend(config: &Config, max_tokens: u32) -> Result<Backend> {
    let mut config = config.clone();
    config.retry.max_attempts = 1;
    config.retry.rotate_accounts = false;
    config.retry.wait_for_quota_reset = false;
    config.retry.fallback_chains.clear();
    config.worker_timeout_secs = BUDGET_SECS;
    let backend = make_backend(&config)?
        .without_steering()
        .with_retry(config.retry.clone());
    let mut profile = backend.model_profile().cloned().unwrap_or_else(|| {
        crate::llm::profiles::ModelProfile::minimal(&config.selection().provider, &config.model)
    });
    profile.max_tokens = Some(
        profile
            .max_tokens
            .filter(|n| *n > 0)
            .unwrap_or(max_tokens)
            .min(max_tokens),
    );
    profile.inherit_output_limit = false;
    Ok(backend.with_profile(profile))
}

/// Runs `work` on this thread under `budget`. A watcher sets `cancel` when the budget expires
/// before `work` returns; an interrupt already set on `cancel` is observed by `work` through the
/// flag it receives. An expired budget is reported as a timeout and a cancelled flag as a
/// cancellation, ahead of any error `work` produced.
pub fn within_budget<T>(
    cancel: &Arc<AtomicBool>,
    budget: Duration,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T>,
) -> Result<T> {
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    let timed_out = Arc::new(AtomicBool::new(false));
    let result = std::thread::scope(|scope| {
        let (done, rx) = std::sync::mpsc::channel::<()>();
        let flag = cancel.clone();
        let deadline = timed_out.clone();
        scope.spawn(move || {
            if matches!(
                rx.recv_timeout(budget),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                deadline.store(true, Ordering::SeqCst);
                flag.store(true, Ordering::SeqCst);
            }
        });
        let result = work(cancel.clone());
        drop(done);
        result
    });
    ensure!(!timed_out.load(Ordering::SeqCst), "verification timed out");
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    result
}
