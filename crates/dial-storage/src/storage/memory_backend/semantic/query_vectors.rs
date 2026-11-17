//! Bounded, time-limited cache of query embeddings, keyed by the embedding route.
//!
//! A query vector depends only on the text and the embedding endpoint and model, never on the
//! stored facts, so it is not dropped by store writes. It is checked before any request is made,
//! so a repeated query costs no embedding call. Entries belong to one route at a time: when the
//! route (endpoint and model) changes, every entry is dropped. At most `MAX_QUERY_VECTORS` are
//! kept, and each expires after `TTL`. Failed embeddings are never stored.

use super::QueryVector;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most query vectors kept; the oldest is evicted first.
pub const MAX_QUERY_VECTORS: usize = 64;
/// Age after which a cached vector is no longer served.
pub const TTL: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct QueryVectorCache {
    route: Option<String>,
    /// Oldest first.
    entries: VecDeque<(String, Instant, QueryVector)>,
}

impl QueryVectorCache {
    /// The vector for `query` embedded on `route`, if cached and not expired.
    pub fn get(&mut self, route: &str, query: &str, now: Instant) -> Option<QueryVector> {
        self.sync(route);
        self.expire(now);
        self.entries
            .iter()
            .find(|(stored, _, _)| stored == query)
            .map(|(_, _, vector)| vector.clone())
    }

    /// Stores `vector` for `query` on `route`, evicting the oldest entries beyond the bound.
    pub fn put(&mut self, route: &str, query: &str, vector: QueryVector, now: Instant) {
        self.sync(route);
        self.expire(now);
        self.entries.retain(|(stored, _, _)| stored != query);
        self.entries.push_back((query.to_owned(), now, vector));
        while self.entries.len() > MAX_QUERY_VECTORS {
            self.entries.pop_front();
        }
    }

    /// Drops every entry when the route has changed since they were stored.
    fn sync(&mut self, route: &str) {
        if self.route.as_deref() != Some(route) {
            self.entries.clear();
            self.route = Some(route.to_owned());
        }
    }

    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|(_, at, _)| now.saturating_duration_since(*at) < TTL);
    }
}
