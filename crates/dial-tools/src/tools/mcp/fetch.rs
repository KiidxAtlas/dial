//! Bounded HTTP GETs for the package listings (npm, PyPI, GitHub). Reads only: nothing here
//! downloads a package or starts a server.
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("dial/", env!("CARGO_PKG_VERSION"));

pub(super) struct Reply {
    pub status: u16,
    pub body: String,
}

/// One GET with any status returned as-is. `what` names the listing in error messages.
pub(super) fn get(url: &str, headers: &[(&str, &str)], what: &str) -> Result<Reply> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut request = agent.get(url).header("User-Agent", USER_AGENT);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut response = request.call().map_err(|error| match error {
        ureq::Error::Timeout(_) => anyhow!("{what} timed out after {}s", TIMEOUT.as_secs()),
        other => anyhow!("{what} request failed: {other}"),
    })?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| anyhow!("{what} reply could not be read: {error}"))?;
    Ok(Reply { status, body })
}

/// The JSON body of a 2xx reply, `None` for 404, and an error for any other status.
pub(super) fn get_json(url: &str, headers: &[(&str, &str)], what: &str) -> Result<Option<Value>> {
    let reply = get(url, headers, what)?;
    match reply.status {
        404 => Ok(None),
        200..=299 => serde_json::from_str(&reply.body)
            .map(Some)
            .map_err(|error| anyhow!("{what} returned invalid JSON: {error}")),
        status => bail!("{what} returned status {status}"),
    }
}

/// Command and Python package names that can be passed to `npx` or `uvx` as one plain argument.
pub(super) fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 214
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Package versions as registries publish them (`1.2.3`, `0.4.0-beta.1+build`).
pub(super) fn plain_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Picks the command that starts the server: the only one, the one named like the package, or
/// the only one with `mcp` in its name. `Err` lists the commands when none of those holds.
pub(super) fn pick_command<'a>(
    commands: &'a [String],
    names: &[&str],
) -> std::result::Result<&'a str, String> {
    if let [only] = commands {
        return Ok(only);
    }
    if let Some(named) = commands.iter().find(|command| {
        names
            .iter()
            .any(|name| normalize(command) == normalize(name))
    }) {
        return Ok(named);
    }
    let mcp: Vec<&String> = commands
        .iter()
        .filter(|command| command.to_ascii_lowercase().contains("mcp"))
        .collect();
    match mcp.as_slice() {
        [only] => Ok(only),
        _ => Err(commands.join(", ")),
    }
}

fn normalize(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

#[cfg(test)]
mod tests;
