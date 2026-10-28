//! Several stored OAuth grants per provider.
//!
//! A provider's accounts live in one owner-private JSON document: the active account key plus a
//! map of account key to grant. ChatGPT's `chatgpt-accounts.json` already uses this shape, so the
//! same [`Store`] serves both; single-grant `oauth-<id>.json` files load as one account keyed
//! `default` (see [`parse`]). Selection is sticky: a session keeps the account it started on and
//! rotates round-robin only when that account is cooling down after a rate limit, quota, or auth
//! failure ([`block_account`]).
use super::super::errors::ErrorClass;
use crate::llm::key_pool::{self, Blocks};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

/// Key given to a legacy single-grant document.
pub(crate) const LEGACY_KEY: &str = "default";

/// The accounts for one provider. `active` is the account a new session starts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Store<T> {
    pub active: Option<String>,
    pub accounts: BTreeMap<String, T>,
}

// Manual impl: an empty store needs no `T: Default`, which a derive would require.
impl<T> Default for Store<T> {
    fn default() -> Self {
        Self {
            active: None,
            accounts: BTreeMap::new(),
        }
    }
}

impl<T> Store<T> {
    /// Add or replace the grant under `key`. The first account added becomes active.
    pub fn insert(&mut self, key: String, value: T) {
        if self.active_key().is_none() {
            self.active = Some(key.clone());
        }
        self.accounts.insert(key, value);
    }

    /// Remove `key`, moving `active` to the first remaining account when it was the active one.
    pub fn remove(&mut self, key: &str) -> Option<T> {
        let removed = self.accounts.remove(key);
        if self.active.as_deref() == Some(key) {
            self.active = self.accounts.keys().next().cloned();
        }
        removed
    }

    /// Make `key` the active account. Returns false when no such account exists.
    pub fn select(&mut self, key: &str) -> bool {
        if self.accounts.contains_key(key) {
            self.active = Some(key.to_owned());
            true
        } else {
            false
        }
    }

    /// The active account, falling back to the first one when `active` is missing or stale.
    pub fn active_key(&self) -> Option<&str> {
        self.active
            .as_deref()
            .filter(|key| self.accounts.contains_key(*key))
            .or_else(|| self.accounts.keys().next().map(String::as_str))
    }

    pub fn active(&self) -> Option<&T> {
        self.active_key().and_then(|key| self.accounts.get(key))
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}

/// Parse a stored document. A document without an `accounts` key is a legacy single grant,
/// wrapped as the [`LEGACY_KEY`] account.
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<Store<T>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("stored OAuth accounts are not valid JSON")?;
    if value.get("accounts").is_some() {
        serde_json::from_value(value).context("stored OAuth accounts are invalid; sign in again")
    } else {
        let grant: T = serde_json::from_value(value)
            .context("stored OAuth credentials are invalid; sign in again")?;
        let mut store = Store::default();
        store.insert(LEGACY_KEY.to_owned(), grant);
        Ok(store)
    }
}

/// The first account after `current` in key order that `skip` does not reject. Round-robin: the
/// search starts after the current account and wraps, so a rotation never returns to the account
/// it just left unless every other one is skipped.
pub fn next_account<T>(
    store: &Store<T>,
    current: Option<&str>,
    skip: impl Fn(&str) -> bool,
) -> Option<String> {
    let keys: Vec<&String> = store.accounts.keys().collect();
    if keys.is_empty() {
        return None;
    }
    let start = current
        .and_then(|current| keys.iter().position(|key| key.as_str() == current))
        .map_or(0, |index| index + 1);
    (0..keys.len())
        .map(|offset| keys[(start + offset) % keys.len()])
        .find(|key| !skip(key))
        .cloned()
}

/// Block scope for a provider's accounts under one user directory (tests use their own).
pub(crate) fn scope(id: &str, user_dir: &Path) -> String {
    format!("oauth:{id}:{}", user_dir.display())
}

/// Block `key` after a failure of `class`: rate limits and quota exhaustion for the cooldown the
/// provider stated (or the default), and rejected credentials for the default cooldown. Other
/// failures leave the account usable and return false.
pub(crate) fn block_account(
    blocks: &Blocks,
    scope: &str,
    key: &str,
    class: &ErrorClass,
    hinted: Option<Duration>,
    now: SystemTime,
) -> bool {
    let rotates = matches!(
        class,
        ErrorClass::RateLimited { .. } | ErrorClass::QuotaExhausted { .. } | ErrorClass::Auth
    );
    if !rotates {
        return false;
    }
    let cooldown = key_pool::cooldown(class, hinted, now).unwrap_or(key_pool::DEFAULT_COOLDOWN);
    blocks.block(scope, key, Instant::now() + cooldown);
    true
}

/// Whether `key` is cooling down in `blocks`.
pub(crate) fn is_blocked(blocks: &Blocks, scope: &str, key: &str) -> bool {
    blocks.is_blocked(scope, key, Instant::now())
}

/// Per-account refresh lock path, derived from the document path and a digest of the account
/// key so no key text reaches the file system. Two processes refreshing the same account contend
/// on this lock; other accounts refresh in parallel.
pub(crate) fn refresh_lock_path(user_dir: &Path, id: &str, key: &str) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    user_dir.join(format!("oauth-{id}.{hex}.refresh"))
}

#[cfg(test)]
mod tests;
