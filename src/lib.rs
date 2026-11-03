pub use dial_build_info::build_info;

/// Registers this executable's build identity for the library crates that report it (tool issue
/// reports, the browser relay and the stats server) without linking `dial-build-info` themselves.
pub fn install_build_identity() {
    dial_core::build_identity::install(dial_core::build_identity::Identity {
        version: build_info::VERSION,
        report: build_info::report,
    });
}
pub use dial_computer::computer;
pub use dial_llm::config;
pub use dial_llm::llm;

pub use dial_storage::storage;

pub use dial_agent::{
    adaptive, agent, extensions, judgment, learn, pack, reliability, stats, steering, telemetry,
    weights,
};
pub use dial_app::{acp, app, benchmark, changelog, cli, collab, release, repl, rpc, schedule};
pub use dial_core::{flush_stdout_on_exit, install_tls_provider, platform, runtime};
pub use dial_tools::{browser_relay, embedded_shell, security, setup, tools, voice};
pub use dial_ui::ui;
