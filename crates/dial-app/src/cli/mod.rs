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
pub mod edit_bench;
pub mod fuzzy_match;
pub mod gc;
pub mod images;
pub mod launch;
pub mod mode_flags;
pub mod model_flags;
pub mod model_scope;
pub mod models_recommend;
pub mod provider_batches;
pub mod ps;
pub mod rules;
pub mod run;
pub mod search;
pub mod session_flags;
pub mod session_import;
mod table;
pub mod tool_flags;
pub mod watch;

/// `--help` sections shared by the root command and the flattened flag groups. Global flags keep
/// their section in every subcommand's help, so a subcommand's own options stay listed first.
pub mod help_heading {
    pub const RUN: &str = "Running a prompt";
    pub const SESSIONS: &str = "Sessions";
    pub const MODEL: &str = "Model and provider";
    pub const SAFETY: &str = "Permissions and isolation";
    pub const MODES: &str = "Work modes";
    pub const CONTEXT: &str = "Context, prompts and tools";
    pub const EXTENSIONS: &str = "Extensions and hooks";
    pub const PROTOCOL: &str = "Protocols and diagnostics";
}
