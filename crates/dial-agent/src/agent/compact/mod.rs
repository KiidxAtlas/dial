//! Context compaction: summaries, compaction runtime and snapshot compaction.

pub mod compaction;
pub(in crate::agent) mod compaction_ops;
pub mod compaction_runtime;
pub mod snapcompact;
