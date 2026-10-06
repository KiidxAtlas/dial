//! MCP "streamable HTTP" transport: every message is a POST to one endpoint.
//!
//! The server answers a request with either a JSON body or a `text/event-stream` carrying the
//! response (and possibly server messages dial ignores). A session id handed out on `initialize`
//! is echoed on every later call. Credentials come from `bearer_env` (an environment variable
//! name, so the secret never sits in the config file) and non-secret `headers`. There is no OAuth
//! flow: a server that needs one must be given a token obtained elsewhere.
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

/// Largest response body accepted.
const MAX_BODY: u64 = 16 * 1024 * 1024;

pub struct Http {
    server: String,
    url: String,
    headers: Vec<(String, String)>,
    agent: ureq::Agent,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
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

impl Http {
    pub fn new(
        server: &str,
        url: &str,
        bearer_env: Option<&str>,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        validate_url(url)?;
        let mut all: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(var) = bearer_env {
            let token = std::env::var(var)
                .map_err(|_| anyhow!("environment variable {var} (bearer_env) is not set"))?;
            all.push(("Authorization".into(), format!("Bearer {}", token.trim())));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            server: server.to_owned(),
            url: url.to_owned(),
            headers: all,
            agent,
            session: Mutex::default(),
            protocol: Mutex::default(),
        })
    }

    /// POST one JSON-RPC message. Returns the JSON-RPC messages in the reply (empty for a bare 202).
    fn post(&self, message: &Value, timeout: Duration) -> Result<Vec<Value>> {
        let mut request = self
            .agent
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(id) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
        {
            request = request.header("Mcp-Session-Id", id);
        }
        if let Some(version) = self
            .protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
        {
            request = request.header("MCP-Protocol-Version", version);
        }
        let mut response = request
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send(message.to_string())
            .map_err(|e| anyhow!("MCP server `{}` request failed: {e}", self.server))?;
        let status = response.status().as_u16();
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_owned());
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| {
                anyhow!(
                    "MCP server `{}`: reading the response failed: {e}",
                    self.server
                )
            })?;
        if status == 401 || status == 403 {
            bail!(
                "MCP server `{}` refused the credentials (HTTP {status}); set bearer_env to a valid token",
                self.server
            );
        }
        if status == 404
            && self
                .session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            bail!(
                "MCP server `{}` ended the session (HTTP 404); restart dial to reconnect",
                self.server
            );
        }
        if !(200..300).contains(&status) {
            bail!(
                "MCP server `{}` returned HTTP {status}: {}",
                self.server,
                body.chars().take(300).collect::<String>()
            );
        }
        if body.trim().is_empty() {
            return Ok(Vec::new());
        }
        if is_sse {
            Ok(parse_sse(&body))
        } else {
            let value: Value = serde_json::from_str(&body).with_context(|| {
                format!("MCP server `{}` sent a body that is not JSON", self.server)
            })?;
            Ok(match value {
                Value::Array(items) => items,
                other => vec![other],
            })
        }
    }

    pub fn set_protocol(&self, version: &str) {
        *self.protocol.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_owned());
    }

    pub fn notify(&self, message: &Value) -> Result<()> {
        self.post(message, Duration::from_secs(30)).map(|_| ())
    }

    /// Send a request and return its `result`; JSON-RPC errors become `Err(Err(text))` so the caller
    /// can word them like the stdio transport does.
    pub fn request(
        &self,
        id: u64,
        message: &Value,
        timeout: Duration,
    ) -> Result<std::result::Result<Value, String>> {
        for reply in self.post(message, timeout)? {
            if reply.get("id").and_then(Value::as_u64) != Some(id) || reply.get("method").is_some()
            {
                continue;
            }
            if let Some(error) = reply.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                return Ok(Err(format!("{text} (code {code})")));
            }
            if let Some(result) = reply.get("result") {
                return Ok(Ok(result.clone()));
            }
        }
        bail!(
            "MCP server `{}` answered without a response to request {id}",
            self.server
        )
    }
}

/// The JSON `data:` payloads of a server-sent-event body, in order. Comments and non-JSON events are skipped.
pub fn parse_sse(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut data = Vec::new();
    let flush = |data: &mut Vec<&str>, out: &mut Vec<Value>| {
        if !data.is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(&data.join("\n")) {
                out.push(value);
            }
            data.clear();
        }
    };
    for line in body.lines() {
        if line.is_empty() {
            flush(&mut data, &mut out);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    flush(&mut data, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sse_events_join_data_lines_and_skip_noise() {
        let body = ": keep-alive\nevent: message\ndata: {\"id\":1,\ndata: \"result\":{}}\n\ndata: not json\n\ndata:{\"id\":2}\n";
        assert_eq!(
            parse_sse(body),
            vec![json!({"id":1,"result":{}}), json!({"id":2})]
        );
    }

    #[test]
    fn urls_must_be_https_or_loopback() {
        for ok in [
            "https://mcp.example.com/mcp",
            "http://localhost:8080/mcp",
            "http://127.0.0.1:1/x",
        ] {
            assert!(validate_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com/mcp",
            "ftp://x/y",
            "mcp.example.com",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }
}
