//! Resource schemes served by the host rather than by Dial.
//!
//! A host registers scheme names at runtime. A `read` of `<scheme>://…` for a served scheme is
//! answered by the resolver the host installed on the workspace. Built-in schemes (`file`, `ssh`,
//! `pr`, `issue`, `conflict`, `artifact`, `http`, `https`) always keep their own resolvers, and the
//! host cannot shadow them.

use std::fmt::Debug;
use std::sync::atomic::AtomicBool;

/// Answers reads of host-served schemes. Installed on a [`crate::tools::Workspace`].
pub trait HostUriResolver: Send + Sync + Debug {
    /// Whether the host currently serves `scheme` (lowercase, without `://`).
    fn serves(&self, scheme: &str) -> bool;
    /// Reads `target` (the full `<scheme>://…` string) from the host.
    fn read(&self, target: &str, cancel: &AtomicBool) -> Result<String, String>;
}

/// Lowercase scheme of a `<scheme>://…` target, or `None` for a plain path.
pub fn scheme_of(target: &str) -> Option<String> {
    let (scheme, _) = target.split_once("://")?;
    let valid = !scheme.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
    valid.then(|| scheme.to_ascii_lowercase())
}

/// A `<scheme>://…` target whose scheme no built-in resolver handles. Only the host can answer it,
/// so reading it needs approval, like any other call that leaves the workspace.
pub fn is_foreign_target(target: &str) -> bool {
    scheme_of(target).is_some()
        && crate::tools::integrations::resources::scheme(target).is_err()
        && !crate::tools::net::web::is_url(target)
}
