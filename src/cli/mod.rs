//! Flag resolution and top-level subcommands that `main.rs` dispatches.
//!
//! `src/commands/` is the slash-command module (mounted privately by `app.rs`); CLI
//! subcommand and flag logic lives here instead.

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
pub mod tool_flags;
