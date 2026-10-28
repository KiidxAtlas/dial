//! `[cache]` settings for provider prompt caching.

use crate::llm::cache_warmer::Mode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// When cache warming replays the settled prefix before its lifetime ends.
    pub warm: Mode,
}
