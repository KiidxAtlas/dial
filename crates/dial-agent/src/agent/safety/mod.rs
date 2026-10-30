//! Safety gates: approval rules, guards, security checks, stream guards and lifecycle hooks.

pub mod approval;
pub mod guard;
pub mod hooks;
pub mod rule_inspect;
pub(in crate::agent) mod rules;
pub(in crate::agent) mod security_tool;
pub(in crate::agent) mod stream_guards;
