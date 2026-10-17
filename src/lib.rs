pub mod acp;
pub mod app;
pub mod benchmark;
pub use dial_build_info::build_info;
pub mod changelog;
pub use dial_computer::computer;
pub use dial_llm::config;
pub(crate) mod git_panel;
pub use dial_llm::llm;

pub mod repl;
pub mod rpc;
pub mod schedule;
pub use dial_storage::storage;
pub mod ui;

pub mod cli;
pub mod collab;

pub(crate) use dial_agent::commit;
pub use dial_agent::{
    adaptive, agent, extensions, judgment, learn, pack, reliability, stats, steering, telemetry,
};
pub use dial_core::{flush_stdout_on_exit, install_tls_provider, platform, runtime};
pub(crate) use dial_core::{output, workspace_copy};
pub use dial_tools::{browser_relay, embedded_shell, security, setup, tools, voice};
