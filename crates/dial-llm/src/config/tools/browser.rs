//! `[browser]` settings for the browser tool.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    pub allowed_hosts: Vec<String>,
    /// Approved top-level localhost navigation grants only that exact host/port for this session.
    pub auto_localhost: bool,
    pub node: String,
    /// Empty (default) uses the built-in pinned runtime. A non-empty value is an operator-owned Playwright entry override.
    pub playwright_module: String,
    /// Permit provisioning a missing pinned runtime. Off uses only installed/cached prerequisites.
    pub allow_install: bool,
    pub chromium_executable: Option<String>,
    pub timeout_secs: u64,
    /// Launch Chromium headless (default). Headed mode keeps the same sandbox and egress bridge.
    pub headless: bool,
    /// Operator-started Chromium DevTools endpoint to attach to (loopback only). Requires allow_attach.
    pub cdp_url: Option<String>,
    /// Opt-in gate for attaching to an existing browser. Attached sessions do not bridge page egress.
    pub allow_attach: bool,
    /// Named storage-state profile (cookies, localStorage) saved under the project state directory.
    pub profile: Option<String>,
    /// Record Playwright video into the session artifact directory.
    pub record_video: bool,
    /// Apply the stealth init scripts and launch flags (headless). `DIAL_BROWSER_STEALTH` overrides the setting.
    pub stealth: bool,
    /// Drive browser surfaces through a cmux socket when `CMUX_SOCKET_PATH` is set. `DIAL_BROWSER_CMUX` overrides.
    pub cmux: bool,
    /// Drive the Chrome tabs you already use through the Dial browser extension and relay. Requires allow_attach. `DIAL_BROWSER_RELAY` overrides.
    pub relay: bool,
    /// Loopback relay address the browser extension connects to. Empty uses http://127.0.0.1:9224.
    pub relay_url: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            node: "node".into(),
            playwright_module: String::new(),
            allow_install: true,
            chromium_executable: None,
            timeout_secs: 0,
            headless: true,
            cdp_url: None,
            allow_attach: false,
            profile: None,
            record_video: false,
            allowed_hosts: Vec::new(),
            auto_localhost: true,
            stealth: true,
            cmux: true,
            relay: false,
            relay_url: String::new(),
        }
    }
}

impl BrowserConfig {
    pub fn validate_allowed_hosts(&self) -> Result<(), String> {
        if self.allowed_hosts.len() > 128 {
            return Err("browser.allowed_hosts accepts at most 128 entries".into());
        }
        for entry in &self.allowed_hosts {
            let uri = crate::url::parse_url(&format!("http://{entry}/"))?;
            let host = uri.host().ok_or("browser allowed host is missing")?;
            if uri.authority().map(|a| a.as_str()) != Some(entry.as_str())
                || uri.port_u16().is_none_or(|p| p == 0)
                || host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
            {
                return Err(format!(
                    "browser.allowed_hosts entry {entry:?} must be an exact lowercase hostname:port; no schemes, paths, credentials or wildcards"
                ));
            }
        }
        Ok(())
    }
}
