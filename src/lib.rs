pub mod acp;
pub mod agent;
pub mod app;
pub mod benchmark;
pub mod browser_relay;
pub mod build_info;
pub mod changelog;
pub(crate) mod commit;
pub mod computer;
pub mod config;
pub mod extensions;
pub(crate) mod git_panel;
pub mod learn;
pub mod llm;
pub(crate) mod output;

/// Waits (bounded) until buffered stdout has been written. Binaries call this on every exit path.
pub fn flush_stdout_on_exit() -> std::io::Result<()> {
    output::drain_stdout(Some(std::time::Duration::from_secs(5)))
}

/// Selects rustls's process-wide crypto provider. If a dependency enables a second provider
/// (`aws-lc-rs`), rustls refuses to choose and panics on the first TLS handshake. The AWS SDK's
/// HTTP client is built on `ring` as well, so the default graph has one provider. Idempotent;
/// binaries call it first, and every HTTP client builder calls it too so library and test entry
/// points are covered.
pub fn install_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means another provider was installed first, which is equally usable.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Lowercase hexadecimal encoding of `bytes`, two digits per byte.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}
pub mod embedded_shell;
pub mod platform;
pub mod repl;
pub mod rpc;
pub mod runtime;
pub mod schedule;
pub mod security;
pub mod setup;
pub mod storage;
pub mod tools;
pub mod ui;
pub(crate) mod workspace_copy;

pub mod pack;
pub mod steering;

pub mod adaptive;
pub mod cli;
pub mod collab;
pub mod judgment;
pub mod reliability;
pub mod stats;
pub mod telemetry;
pub mod voice;

#[cfg(test)]
mod tls_provider_tests;
