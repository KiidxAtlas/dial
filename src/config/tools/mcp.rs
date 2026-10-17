//! `[mcp_settings]` and MCP server validation.
use crate::config::types::McpServerConfig;

use anyhow::{Context, Result, bail};

/// `[mcp_settings]`. Reserved: both keys load, but nothing reads them yet, so neither discovery nor
/// project approval exists.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    /// Reserved; no MCP config files of other tools are discovered.
    pub discover: bool,
    /// Reserved; project-scoped servers are not gated by it.
    pub project_approval: bool,
}

pub fn validate_server(name: &str, server: &McpServerConfig) -> Result<()> {
    if name.trim().is_empty() {
        bail!("MCP server name cannot be empty");
    }
    if let Some(url) = &server.url {
        if !server.program.as_os_str().is_empty() {
            bail!("MCP server `{name}` must set exactly one of program or url");
        }
        validate_url(url)?;
    } else if server.program.as_os_str().is_empty() && server.enabled {
        bail!("MCP server `{name}` needs program or url");
    }
    if server.timeout_secs > 600 || server.connect_timeout_secs > 600 {
        bail!("MCP timeouts must be at most 600 seconds");
    }
    if server.url.is_some() && server.transport == Some(crate::config::types::McpTransport::Stdio) {
        bail!("remote MCP server cannot use stdio transport");
    }
    if server.url.is_none()
        && matches!(
            server.transport,
            Some(
                crate::config::types::McpTransport::Http | crate::config::types::McpTransport::Sse
            )
        )
    {
        bail!("stdio MCP server cannot use HTTP/SSE transport");
    }
    Ok(())
}

/// Plain HTTP only to loopback; everything else must be HTTPS.
pub fn validate_url(url: &str) -> Result<()> {
    let uri: ureq::http::Uri = url
        .parse()
        .with_context(|| format!("invalid MCP url {url:?}"))?;
    let host = uri.host().unwrap_or("");
    match uri.scheme_str() {
        Some("https") if !host.is_empty() => Ok(()),
        Some("http") if matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1") => Ok(()),
        _ => bail!("MCP url must be https (http is allowed only for localhost): {url}"),
    }
}
