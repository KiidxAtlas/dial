//! Agent base: accessors, agent definitions, helpers and session lifecycle.

pub(in crate::agent) mod accessors;
pub use accessors::SessionWorkspace;
pub mod agent_defs;
pub(in crate::agent) mod helpers;
pub(in crate::agent) mod lifecycle;
