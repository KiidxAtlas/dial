//! QwenCloud Token Plan sign-in.
//!
//! An API-key grant: the user picks a region (international, China Beijing, or a custom base URL),
//! pastes the key, and the key is validated against `GET {base}/models` before it is stored. An
//! optional browser Cookie header enables quota reporting for the usage module.
//!
//! Storage shape (read by `usage_report`):
//! - `access`: the Token Plan API key.
//! - `api_endpoint`: the region base URL, only when it differs from the international default.
//!   Requests use it as `Access.base_url`.
//! - `extra["cookie"]`: the normalized Cookie header value, when one was supplied.
use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Interaction, Login, util};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const INTERNATIONAL_AUTH_URL: &str =
    "https://home.qwencloud.com/billing/subscription/token-plan-individual";
const CHINA_AUTH_URL: &str = "https://www.aliyun.com/benefit/scene/tokenplan";
const INTERNATIONAL_API_BASE_URL: &str =
    "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1";
const CHINA_API_BASE_URL: &str =
    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";
const CHINA_COOKIE_HOST: &str = "bailian-cs.console.aliyun.com";
const INTERNATIONAL_COOKIE_HOST: &str = "cs-data.qwencloud.com";
const VALIDATION_TIMEOUT_SECS: u64 = 15;

pub const FLOW: Flow = Flow {
    id: "alibaba-token-plan",
    name: "QwenCloud Token Plan",
    base_url: INTERNATIONAL_API_BASE_URL,
    protocol: ApiProtocol::ChatCompletions,
    needs_terminal: true,
    subscription: true,
    login,
    refresh: None,
    access,
};

/// Region endpoints. Production passes the published URLs; tests pass a loopback server.
struct Endpoints<'a> {
    international: &'a str,
    china: &'a str,
}

fn login(login: &Login<'_>) -> Result<Credentials> {
    sign_in(
        login.ui,
        &login.cancel,
        &|url: &str, instructions: Option<&str>| login.open(url, instructions),
        &Endpoints {
            international: INTERNATIONAL_API_BASE_URL,
            china: CHINA_API_BASE_URL,
        },
    )
}

fn sign_in(
    ui: &dyn Interaction,
    cancel: &Arc<AtomicBool>,
    open: &dyn Fn(&str, Option<&str>),
    endpoints: &Endpoints<'_>,
) -> Result<Credentials> {
    let choice = ui.prompt(
        "Select QwenCloud Token Plan region: 1=International (default), 2=China (Beijing), 3=Custom — enter 1, 2, or 3",
        Some("1"),
        false,
    )?;
    check_cancelled(cancel)?;
    let (base_url, auth_url, instructions) = match choice.trim() {
        "2" => (
            endpoints.china.to_owned(),
            CHINA_AUTH_URL,
            "Subscribe to the China (Beijing) 百炼 Token Plan and copy its dedicated API key",
        ),
        "3" => {
            let custom_url = ui.prompt(
                "Enter custom Token Plan base URL",
                Some("https://token-plan.<region>.maas.aliyuncs.com/compatible-mode/v1"),
                false,
            )?;
            let trimmed = custom_url.trim().trim_end_matches('/');
            ensure!(!trimmed.is_empty(), "Custom URL is required for option 3");
            (
                trimmed.to_owned(),
                INTERNATIONAL_AUTH_URL,
                "Copy your Token Plan API key for the custom endpoint",
            )
        }
        _ => (
            endpoints.international.to_owned(),
            INTERNATIONAL_AUTH_URL,
            "Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting.",
        ),
    };

    open(auth_url, Some(instructions));
    let answer = ui.prompt(
        "Paste your QwenCloud Token Plan API key",
        Some("sk-sp-..."),
        false,
    )?;
    check_cancelled(cancel)?;
    let key = answer.trim();
    ensure!(!key.is_empty(), "QwenCloud Token Plan API key is required");

    ui.progress("Validating API key...");
    validate_models(key, &base_url, cancel)?;

    let cookie = prompt_cookie(ui, cancel, &base_url, endpoints)?;

    let mut extra = serde_json::Map::new();
    if !cookie.is_empty() {
        extra.insert("cookie".to_owned(), cookie.into());
    }
    Ok(Credentials {
        access: key.to_owned(),
        refresh: String::new(),
        expires_at_ms: util::NEVER_EXPIRES,
        api_endpoint: (base_url != endpoints.international).then_some(base_url),
        extra,
        ..Credentials::default()
    })
}

/// Optional usage-reporting cookie. Empty input skips quota reporting; anything else must be a
/// `name=value` Cookie header so a lone cookie value pasted by mistake is rejected here.
fn prompt_cookie(
    ui: &dyn Interaction,
    cancel: &Arc<AtomicBool>,
    base_url: &str,
    endpoints: &Endpoints<'_>,
) -> Result<String> {
    let china = base_url == endpoints.china;
    let (message, host) = if china {
        (CHINA_COOKIE_PROMPT, CHINA_COOKIE_HOST)
    } else {
        (INTERNATIONAL_COOKIE_PROMPT, INTERNATIONAL_COOKIE_HOST)
    };
    let answer = ui.prompt(message, Some("name=value; name=value; ..."), true)?;
    check_cancelled(cancel)?;
    let cookie = normalize_cookie(&answer);
    ensure!(
        cookie.is_empty() || is_cookie_header(&cookie),
        "Invalid QwenCloud Cookie header. Copy the complete Cookie request header from the {host} usage request, not a single cookie value."
    );
    Ok(cookie)
}

/// Trims whitespace and a leading case-insensitive `Cookie:` label.
fn normalize_cookie(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        .map_or(trimmed, |_| &trimmed[7..]);
    stripped.trim().to_owned()
}

/// True when at least one `;`-separated segment is `name=value` with both sides non-empty.
fn is_cookie_header(cookie: &str) -> bool {
    cookie.split(';').any(|segment| {
        segment
            .trim()
            .split_once('=')
            .is_some_and(|(name, value)| !name.trim().is_empty() && !value.trim().is_empty())
    })
}

fn validate_models(key: &str, base_url: &str, cancel: &Arc<AtomicBool>) -> Result<()> {
    let bearer = format!("Bearer {key}");
    let response = util::request_with_timeout(
        "GET",
        &format!("{base_url}/models"),
        &[("Authorization", bearer.as_str())],
        util::Body::Empty,
        cancel,
        VALIDATION_TIMEOUT_SECS,
    )?;
    ensure!(
        response.ok(),
        "QwenCloud Token Plan API key validation failed (HTTP {})",
        response.status
    );
    Ok(())
}

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "sign-in cancelled");
    Ok(())
}

fn access(credentials: &Credentials) -> Result<Access> {
    let mut access = super::bearer(credentials)?;
    access.base_url = credentials.api_endpoint.clone();
    Ok(access)
}

const CHINA_COOKIE_PROMPT: &str = "Optional: paste the Cookie header from the China Bailian usage request (bailian-cs.console.aliyun.com) to enable quota reporting. Leave empty to skip.";
const INTERNATIONAL_COOKIE_PROMPT: &str = "Optional: paste the Cookie header from the QwenCloud usage request (cs-data.qwencloud.com) to enable quota reporting. Leave empty to skip.";

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
    };

    #[derive(Default)]
    struct Ui {
        answers: Mutex<VecDeque<String>>,
        auths: Mutex<Vec<(String, Option<String>)>>,
    }

    impl Ui {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().map(|a| (*a).to_owned()).collect()),
                auths: Mutex::new(Vec::new()),
            }
        }
    }

    impl Interaction for Ui {
        fn auth(&self, url: &str, instructions: Option<&str>) {
            self.auths
                .lock()
                .push((url.to_owned(), instructions.map(str::to_owned)));
        }
        fn progress(&self, _message: &str) {}
        fn prompt(
            &self,
            _message: &str,
            _placeholder: Option<&str>,
            _allow_empty: bool,
        ) -> Result<String> {
            Ok(self.answers.lock().pop_front().unwrap_or_default())
        }
    }

    fn idle() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn unused_endpoints() -> Endpoints<'static> {
        Endpoints {
            international: "http://127.0.0.1:1/v1",
            china: "http://127.0.0.1:1/v1",
        }
    }

    /// Loopback HTTP server answering one canned response per expected request; each raw request
    /// is sent back on the channel. Returns the base URL (`http://127.0.0.1:port/v1`).
    fn serve(responses: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                tx.send(read_request(&mut stream)).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (base, rx)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "client closed before sending headers");
            bytes.extend_from_slice(&buf[..n]);
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let n = stream.read(&mut buf).unwrap();
            assert!(n > 0, "client closed before sending body");
            bytes.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn flow_invariants() {
        assert_eq!(FLOW.id, "alibaba-token-plan");
        assert_eq!(FLOW.base_url, INTERNATIONAL_API_BASE_URL);
        assert!(matches!(FLOW.protocol, ApiProtocol::ChatCompletions));
        assert!(FLOW.needs_terminal && FLOW.subscription);
        assert!(FLOW.refresh.is_none());
        assert_eq!(
            crate::llm::oauth::flow("alibaba-token-plan").map(|f| f.name),
            Some("QwenCloud Token Plan")
        );
    }

    #[test]
    fn international_login_stores_key_without_endpoint_or_cookie() {
        let (base, requests) = serve(vec![(200, r#"{"data":[]}"#)]);
        // Empty cookie answer skips quota reporting.
        let ui = Ui::new(&["1", " sk-sp-test ", ""]);
        let endpoints = Endpoints {
            international: &base,
            china: "http://127.0.0.1:1/v1",
        };
        let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

        let request = requests.recv().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models http/1.1"));
        assert!(request.contains("authorization: bearer sk-sp-test"));

        assert_eq!(credentials.access, "sk-sp-test");
        assert!(credentials.refresh.is_empty());
        assert_eq!(credentials.expires_at_ms, util::NEVER_EXPIRES);
        assert_eq!(credentials.api_endpoint, None);
        assert!(!credentials.extra.contains_key("cookie"));
        assert_eq!(
            *ui.auths.lock(),
            vec![(
                INTERNATIONAL_AUTH_URL.to_owned(),
                Some(
                    "Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting."
                        .to_owned()
                ),
            )]
        );
    }

    #[test]
    fn china_login_stores_region_endpoint_and_normalized_cookie() {
        let (china, requests) = serve(vec![(200, "{}")]);
        let ui = Ui::new(&["2", "sk-sp-cn", "  cookie: a=1; b=2  "]);
        let endpoints = Endpoints {
            international: "http://127.0.0.1:1/v1",
            china: &china,
        };
        let credentials = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints).unwrap();

        assert!(requests.recv().unwrap().starts_with("GET /v1/models"));
        assert_eq!(credentials.api_endpoint.as_deref(), Some(china.as_str()));
        assert_eq!(
            credentials.extra.get("cookie").and_then(|v| v.as_str()),
            Some("a=1; b=2")
        );
        assert_eq!(ui.auths.lock()[0].0, CHINA_AUTH_URL);
    }

    #[test]
    fn custom_region_normalizes_url_and_stores_it_as_endpoint() {
        let (base, requests) = serve(vec![(200, "{}")]);
        let custom = format!("{base}//");
        let ui = Ui::new(&["3", custom.as_str(), "sk-sp-custom", ""]);
        let credentials =
            sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints()).unwrap();

        assert!(requests.recv().unwrap().starts_with("GET /v1/models"));
        assert_eq!(credentials.api_endpoint.as_deref(), Some(base.as_str()));
        assert_eq!(credentials.access, "sk-sp-custom");
    }

    #[test]
    fn lone_cookie_value_is_rejected() {
        let (base, _requests) = serve(vec![(200, "{}")]);
        let ui = Ui::new(&["1", "sk-sp-x", "abc123"]);
        let endpoints = Endpoints {
            international: &base,
            china: "",
        };
        let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Invalid QwenCloud Cookie header."));
        assert!(error.contains(INTERNATIONAL_COOKIE_HOST));
    }

    #[test]
    fn rejected_key_fails_without_echoing_the_key_or_body() {
        let (base, _requests) = serve(vec![(403, r#"{"error":"sk-sp-leaked"}"#)]);
        let ui = Ui::new(&["1", "sk-sp-leaked"]);
        let endpoints = Endpoints {
            international: &base,
            china: "",
        };
        let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &endpoints)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "QwenCloud Token Plan API key validation failed (HTTP 403)"
        );
    }

    #[test]
    fn empty_custom_url_is_rejected() {
        let ui = Ui::new(&["3", "  "]);
        let error = sign_in(&ui, &idle(), &|u, i| ui.auth(u, i), &unused_endpoints())
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Custom URL is required for option 3");
    }

    #[test]
    fn cookie_normalization_and_shape_check() {
        assert_eq!(normalize_cookie("  Cookie:  a=1 "), "a=1");
        assert_eq!(normalize_cookie("COOKIE:a=1"), "a=1");
        assert_eq!(normalize_cookie("   "), "");
        assert!(is_cookie_header("a=1"));
        assert!(is_cookie_header("junk; b=2"));
        assert!(!is_cookie_header("abc"));
        assert!(!is_cookie_header("=x"));
        assert!(!is_cookie_header("a="));
    }

    #[test]
    fn access_uses_the_stored_region_endpoint_when_present() {
        let regional = Credentials {
            access: "sk-a".into(),
            api_endpoint: Some("https://example.test/v1".into()),
            expires_at_ms: util::NEVER_EXPIRES,
            ..Credentials::default()
        };
        assert_eq!(
            access(&regional).unwrap().base_url.as_deref(),
            Some("https://example.test/v1")
        );

        let international = Credentials {
            access: "sk-b".into(),
            ..Credentials::default()
        };
        let shaped = access(&international).unwrap();
        assert_eq!(shaped.token.as_str(), "sk-b");
        assert_eq!(shaped.base_url, None);
    }
}
