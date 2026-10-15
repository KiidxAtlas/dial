//! The browser tool's side of the relay: find a relay, start one in this process if the port is
//! free, and wait until the Dial extension is connected. Returns the CDP WebSocket URL.

use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use url::Url;

use super::protocol::{DEFAULT_RELAY_URL, TOKEN_HEADER};

/// How long a browser call waits for the extension before it fails.
const WAIT_LIMIT: Duration = Duration::from_secs(20);
const POLL_EVERY: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const EXTENSION_HINT: &str = "the Dial browser extension is not connected. Run `dial browser-relay install`, load the printed folder in chrome://extensions, and keep Chrome open";

/// A validated relay address: loopback, with an explicit port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAddress {
    pub port: u16,
}

impl RelayAddress {
    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Parses `browser.relay_url`. An empty value means the default relay address.
pub fn parse_relay_url(raw: &str) -> Result<RelayAddress, String> {
    let raw = raw.trim();
    let text = if raw.is_empty() {
        DEFAULT_RELAY_URL
    } else {
        raw
    };
    let url = Url::parse(text).map_err(|error| {
        format!("browser.relay_url is not a valid URL ({error}). Use something like http://127.0.0.1:9224.")
    })?;
    if url.scheme() != "http" {
        return Err("browser.relay_url must start with http:// (the relay is loopback HTTP), e.g. http://127.0.0.1:9224.".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("browser.relay_url must not contain credentials; the relay token is read from Dial's token file.".to_owned());
    }
    if !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
        return Err("browser.relay_url must point at 127.0.0.1 or localhost; the relay refuses other addresses.".to_owned());
    }
    let port = url.port().ok_or_else(|| {
        "browser.relay_url must include an explicit port, e.g. http://127.0.0.1:9224.".to_owned()
    })?;
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("browser.relay_url must be the relay root (no path, query, or fragment), e.g. http://127.0.0.1:9224.".to_owned());
    }
    Ok(RelayAddress { port })
}

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// The extension is connected; carries the CDP WebSocket URL.
    Ready(String),
    /// The relay is up but the extension is not connected yet; carries the reason.
    Waiting(String),
    /// Something answered, but it is not a usable relay for this user; carries the reason.
    Refused(String),
    /// Nothing answered on the port.
    Unreachable,
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(PROBE_TIMEOUT))
            .build(),
    )
}

fn probe(agent: &ureq::Agent, address: &RelayAddress, token: &str) -> Probe {
    let url = format!("{}/json/version", address.base());
    let mut response = match agent.get(&url).header(TOKEN_HEADER, token).call() {
        Ok(response) => response,
        Err(_) => return Probe::Unreachable,
    };
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    match status {
        200 => match serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value["webSocketDebuggerUrl"].as_str().map(str::to_owned))
        {
            Some(ws_url) => Probe::Ready(ws_url),
            None => Probe::Refused(format!(
                "{} answered without a CDP endpoint; another program may be using that port",
                address.base()
            )),
        },
        401 => Probe::Refused(format!(
            "the relay on {} rejected Dial's relay token. Run `dial browser-relay install` to refresh the extension, or stop the other relay on that port.",
            address.base()
        )),
        503 => Probe::Waiting(waiting_reason(&body)),
        other => Probe::Refused(format!(
            "the relay on {} answered HTTP {other}",
            address.base()
        )),
    }
}

fn waiting_reason(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut reason = parsed["error"]
        .as_str()
        .unwrap_or(EXTENSION_HINT)
        .to_owned();
    if let Some(last) = parsed["lastError"].as_str() {
        reason.push_str(&format!(" (last relay error: {last})"));
    }
    reason
}

/// Returns the CDP WebSocket URL for a connected extension, starting an in-process relay when the
/// address is free. Waits up to 20 seconds for the extension. `cancelled` is polled while waiting.
pub fn resolve(
    relay_url: &str,
    user_dir: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    let address = parse_relay_url(relay_url)?;
    let token = super::load_or_create_token(user_dir).map_err(|error| {
        format!(
            "cannot read or create the browser relay token in {}: {error}",
            user_dir.display()
        )
    })?;
    let agent = agent();
    let mut current = probe(&agent, &address, &token);
    if current == Probe::Unreachable {
        super::ensure_serving(address.port, user_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::AddrInUse {
                format!(
                    "port {} on 127.0.0.1 is used by another program, so the browser relay cannot start here. Free the port, or point browser.relay_url at a running `dial browser-relay serve`.",
                    address.port
                )
            } else {
                format!("cannot start the browser relay on 127.0.0.1:{}: {error}", address.port)
            }
        })?;
        current = probe(&agent, &address, &token);
    }
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        match current {
            Probe::Ready(ws_url) => return Ok(ws_url),
            Probe::Refused(reason) => return Err(reason),
            Probe::Unreachable => {
                return Err(format!(
                    "the browser relay at {} did not answer after it started",
                    address.base()
                ));
            }
            Probe::Waiting(reason) => {
                if cancelled() {
                    return Err("waiting for the browser extension was cancelled".to_owned());
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{reason}. Waited {} seconds for the extension.",
                        WAIT_LIMIT.as_secs()
                    ));
                }
                thread::sleep(POLL_EVERY);
                current = probe(&agent, &address, &token);
            }
        }
    }
}

/// One-line answer for `dial browser-relay status`: `Ok` when the extension is connected. The
/// CDP URL is never printed because it carries the relay token.
pub fn status_report(port: u16, user_dir: &Path) -> Result<String, String> {
    let address = RelayAddress { port };
    let token = super::load_or_create_token(user_dir).map_err(|error| {
        format!(
            "cannot read the browser relay token in {}: {error}",
            user_dir.display()
        )
    })?;
    match probe(&agent(), &address, &token) {
        Probe::Ready(_) => Ok(format!(
            "connected: the Dial extension is attached to the relay on 127.0.0.1:{port}"
        )),
        Probe::Waiting(reason) => Err(format!("not connected: {reason}")),
        Probe::Refused(reason) => Err(reason),
        Probe::Unreachable => Err(format!(
            "no relay is listening on 127.0.0.1:{port}. Run `dial browser-relay serve --port {port}`, or start Dial with browser relay enabled."
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    use super::*;

    #[test]
    fn an_empty_relay_url_means_the_default_loopback_port() {
        assert_eq!(parse_relay_url("").unwrap(), RelayAddress { port: 9224 });
        assert_eq!(parse_relay_url("  ").unwrap(), RelayAddress { port: 9224 });
    }

    #[test]
    fn localhost_and_loopback_with_a_port_are_accepted() {
        assert_eq!(
            parse_relay_url("http://localhost:9300").unwrap(),
            RelayAddress { port: 9300 }
        );
        assert_eq!(
            parse_relay_url("http://127.0.0.1:9301/").unwrap(),
            RelayAddress { port: 9301 }
        );
    }

    #[test]
    fn remote_hosts_https_credentials_and_missing_ports_are_refused() {
        assert!(
            parse_relay_url("http://example.com:9224")
                .unwrap_err()
                .contains("127.0.0.1")
        );
        assert!(
            parse_relay_url("https://127.0.0.1:9224")
                .unwrap_err()
                .contains("http://")
        );
        assert!(
            parse_relay_url("http://user:pw@127.0.0.1:9224")
                .unwrap_err()
                .contains("credentials")
        );
        assert!(
            parse_relay_url("http://127.0.0.1")
                .unwrap_err()
                .contains("explicit port")
        );
        assert!(
            parse_relay_url("http://127.0.0.1:9224/json")
                .unwrap_err()
                .contains("relay root")
        );
    }

    #[test]
    fn the_waiting_reason_carries_the_relay_error_and_install_hint() {
        let reason = waiting_reason(r#"{"error":"not connected","lastError":"tab 12 busy"}"#);
        assert!(reason.contains("not connected"), "{reason}");
        assert!(reason.contains("tab 12 busy"), "{reason}");
        assert!(waiting_reason("not json").contains("dial browser-relay install"));
    }

    #[test]
    fn a_cancelled_wait_returns_at_once_and_still_starts_the_relay() {
        let dir = tempfile::tempdir().unwrap();
        let port = {
            let probe = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
            probe.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}");
        let started = Instant::now();
        let error = resolve(&url, dir.path(), &|| true).unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(started.elapsed() < WAIT_LIMIT);
        // The relay is now listening: the port accepts connections.
        assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn a_port_held_by_another_program_is_reported_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let holder = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let port = holder.local_addr().unwrap().port();
        // The holder answers nothing HTTP-wise, so the probe fails and the bind is attempted.
        let error = resolve(&format!("http://127.0.0.1:{port}"), dir.path(), &|| true).unwrap_err();
        assert!(error.contains("used by another program"), "{error}");
    }
}
