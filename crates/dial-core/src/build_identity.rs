//! The running executable's build identity, registered by the binary at startup.
//!
//! `dial-build-info` embeds the revision, dirty state and build time, so it is recompiled whenever
//! any workspace source changes. Library crates below the application read the identity through
//! this registry instead of linking that crate, so editing one crate no longer recompiles the
//! unrelated crates beneath it. Until a binary registers, the package version is reported alone.
use std::sync::OnceLock;

/// What the binary registers: the full version string and its machine-readable report.
pub struct Identity {
    pub version: &'static str,
    pub report: fn() -> serde_json::Value,
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// Registers the executable's identity. Only the first registration takes effect.
pub fn install(identity: Identity) {
    let _ = IDENTITY.set(identity);
}

/// The registered version string, or the package version for unregistered entry points (tests).
pub fn version() -> &'static str {
    IDENTITY
        .get()
        .map_or(env!("CARGO_PKG_VERSION"), |identity| identity.version)
}

/// The registered build report, or one naming only the package version.
pub fn report() -> serde_json::Value {
    IDENTITY.get().map_or_else(
        || serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "revision": "unknown" }),
        |identity| (identity.report)(),
    )
}
