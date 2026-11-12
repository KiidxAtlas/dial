//! OpenCode Zen and Go request identity.
//!
//! OpenCode routes and caches by conversation: OpenCode Go refuses a request without a stable
//! `x-opencode-session` header (`400 MissingSessionID`), and both services ask clients to name
//! themselves in `User-Agent` rather than send an HTTP library's default. These headers are
//! attached only to requests for OpenCode's own host.

use std::sync::LazyLock;

/// Header OpenCode reads the conversation identity from.
pub const SESSION_HEADER: &str = "x-opencode-session";

/// Dial's own client identity, as OpenCode asks of coding agents.
pub const USER_AGENT: &str = concat!("dial/", env!("CARGO_PKG_VERSION"));

const HOST: &str = "opencode.ai";

/// Whether `url` addresses OpenCode's hosted inference (any Zen or Go route).
pub fn is_endpoint(url: &str) -> bool {
    super::host_of(url).is_some_and(|host| host.eq_ignore_ascii_case(HOST))
}

/// One identifier per process for requests that belong to no conversation (one-shot calls), so
/// they still route instead of failing; a bound conversation always sends its own id.
static PROCESS_SESSION: LazyLock<String> = LazyLock::new(|| {
    use rand::RngExt;
    format!("dial-{:032x}", rand::rng().random::<u128>())
});

/// The session header value: the owning conversation's id, else the process-wide id.
pub fn session(session_id: Option<&str>) -> &str {
    session_id
        .filter(|id| !id.is_empty())
        .unwrap_or(PROCESS_SESSION.as_str())
}

/// Headers for an OpenCode request, omitting any name the caller already supplies.
pub fn headers<'a>(
    session_id: Option<&'a str>,
    existing: &[(&str, &str)],
) -> Vec<(&'static str, &'a str)> {
    [
        (SESSION_HEADER, session(session_id)),
        ("User-Agent", USER_AGENT),
    ]
    .into_iter()
    .filter(|(name, _)| {
        !existing
            .iter()
            .any(|(present, _)| present.eq_ignore_ascii_case(name))
    })
    .collect()
}

#[cfg(test)]
mod tests;
