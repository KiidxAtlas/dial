//! Optional-mode instruction sources, grouped by runtime owner.
//!
//! Selection, state transitions, rendering, and enforcement stay in `agent::modes`.
//! The prompt-only cleanse renderer remains at `agent/modes/cleanse/prompts.rs` alongside
//! its assignment types and discovery schema. The compile-time `/rule` Markdown resource
//! remains at `agent/modes/resources/rule_author.md` with its owner.

pub(crate) mod advisor;
pub(crate) mod external_thinking;
pub(crate) mod goal;
pub(crate) mod prewalk;
pub(crate) mod tan;
