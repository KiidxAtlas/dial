//! Reusable flag resolution and subcommands for the binary's dispatcher.
//!
//! `app/commands/` owns private interactive slash-command handlers. CLI subcommand and
//! flag logic lives here instead. Binary-only benchmark, improvement and update handlers
//! live in the root package's `src/cmd/`, not in this library module's public API.

pub mod annotate;
pub mod anonymize;
pub mod browser_relay;
pub mod cli_modes;
pub mod commit;
pub mod config_models;
pub mod fuzzy_match;
pub mod gc;
pub mod images;
pub mod launch;
pub mod mode_flags;
pub mod model_flags;
pub mod model_scope;
pub mod models_recommend;
pub mod plugin;
pub mod ps;
pub mod search;
pub mod session_flags;
pub mod session_import;
mod table;
pub mod tool_flags;
