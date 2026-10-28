//! Bounded, time-limited cache of fused recall results for one fact store.
//!
//! An entry is served only while the store's write token is unchanged, so any write, from this
//! connection or another, drops every cached result before it can be returned. The cache holds
//! at most `MAX_ENTRIES` results and each expires after `TTL`.

use super::Recall;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most results one store keeps; the oldest is evicted first.
pub const MAX_ENTRIES: usize = 32;
/// Age after which a cached result is no longer served.
pub const TTL: Duration = Duration::from_secs(300);

/// A store's write token: see `Store::write_token`.
pub type WriteToken = (i64, u64);

/// Everything that determines a recall result besides the store contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub query: String,
    pub limit: usize,
    pub token_budget: usize,
    /// The embedding model and the query vector's exact bits, so one query never reuses a
    /// result fused from a different vector. `None` for a lexical-only recall.
    pub vector: Option<(String, Vec<u32>)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Default)]
pub struct RecallCache {
    token: Option<WriteToken>,
    /// Oldest first; a hit moves its entry to the back.
    entries: VecDeque<(Key, Instant, Recall)>,
    hits: u64,
    misses: u64,
}

impl RecallCache {
    /// The cached result for `key`, if the store is unchanged since it was stored and it has not
    /// expired.
    pub fn get(&mut self, key: &Key, token: WriteToken, now: Instant) -> Option<Recall> {
        self.sync(token);
        self.expire(now);
        let Some(index) = self.entries.iter().position(|(stored, _, _)| stored == key) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        let entry = self.entries.remove(index)?;
        let recall = entry.2.clone();
        self.entries.push_back(entry);
        Some(recall)
    }

    /// Stores `recall` for `key`, evicting the oldest entries beyond the bound.
    pub fn put(&mut self, key: Key, recall: Recall, token: WriteToken, now: Instant) {
        self.sync(token);
        self.expire(now);
        self.entries.retain(|(stored, _, _)| stored != &key);
        self.entries.push_back((key, now, recall));
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// Drops every entry when the store's write token has moved.
    fn sync(&mut self, token: WriteToken) {
        if self.token != Some(token) {
            self.entries.clear();
            self.token = Some(token);
        }
    }

    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|(_, at, _)| now.saturating_duration_since(*at) < TTL);
    }
}

#[cfg(test)]
mod tests;
