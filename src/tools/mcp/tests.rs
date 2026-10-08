//! End-to-end tests against a scripted server (`fake.rs`) running as a real child process: the test
//! binary re-executes itself with [`fake::ENV`] set and runs the `fake_server` test below as the server.

use super::*;
use crate::tools::ext::{EXTENSION_API_VERSION, LifecycleEvent, ToolContext};
use crate::tools::testkit;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// The server entry point. A normal test run (variable unset) does nothing.
#[test]
fn fake_server() {
    if let Ok(flags) = std::env::var(fake::ENV) {
        fake::run(&flags);
    }
}

fn fake_config(flags: &str, timeout_secs: u64) -> McpServerConfig {
    McpServerConfig {
        program: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "tools::mcp::tests::fake_server",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(String::from)
        .to_vec(),
        env: BTreeMap::from([(fake::ENV.to_owned(), flags.to_owned())]),
        timeout_secs,
        lifecycle_hooks: vec![],
        url: None,
        bearer_env: None,
        headers: BTreeMap::new(),
    }
}

fn config_with(servers: Vec<(&str, McpServerConfig)>) -> Config {
    Config {
        mcp: servers
            .into_iter()
            .map(|(name, server)| (name.to_owned(), server))
            .collect(),
        ..Config::default()
    }
}

fn load_fake() -> McpLoad {
    let loaded = load(&config_with(vec![("fake", fake_config("", 30))]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    loaded
}

fn tool<'a>(loaded: &'a McpLoad, name: &str) -> &'a Arc<dyn Tool> {
    loaded
        .tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

fn call(loaded: &McpLoad, name: &str, args: Value) -> crate::tools::ToolOutput {
    let (_dir, workspace) = testkit::workspace();
    let config = Config::default();
    tool(loaded, name).call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
        },
        &args,
    )
}

#[test]
fn handshake_follows_every_page_and_exposes_prefixed_tools() {
    let loaded = load_fake();
    let names: BTreeSet<_> = loaded.tools.iter().map(|t| t.name().to_owned()).collect();
    let expected: BTreeSet<_> = [
        "echo", "plain", "fail", "mixed", "slow", "die", "pid", "a_b", "a_b_2", "big", "pong",
        "bare",
    ]
    .iter()
    .map(|n| format!("mcp__fake__{n}"))
    .collect();
    assert_eq!(
        names, expected,
        "tools from all three pages, colliding names disambiguated"
    );

    let echo = tool(&loaded, "mcp__fake__echo").schema();
    assert_eq!(echo["function"]["name"], "mcp__fake__echo");
    assert_eq!(echo["function"]["description"], "fake: fake tool echo");
    assert_eq!(
        echo["function"]["parameters"]["properties"]["text"]["type"],
        "string"
    );
    // Missing description and inputSchema fall back to defaults.
    let bare = tool(&loaded, "mcp__fake__bare").schema();
    assert_eq!(bare["function"]["description"], "fake: MCP tool `bare`");
    assert_eq!(bare["function"]["parameters"], json!({"type": "object"}));
}

#[test]
fn server_read_only_hints_never_bypass_approval() {
    let loaded = load_fake();
    let approval = |name: &str| tool(&loaded, name).needs_approval(&json!({}));
    assert!(
        approval("mcp__fake__echo"),
        "server hint is not an operator permission"
    );
    assert!(
        approval("mcp__fake__pid"),
        "server hint is not an operator permission"
    );
    assert!(approval("mcp__fake__plain"), "no annotations");
    assert!(approval("mcp__fake__fail"), "readOnlyHint false");
    assert!(approval("mcp__fake__bare"), "no schema, no annotations");
}

#[test]
fn extension_version_and_mcp_lifecycle_config_default() {
    assert_eq!(EXTENSION_API_VERSION, 1);
    let server: McpServerConfig = serde_json::from_value(json!({"program": "server"})).unwrap();
    assert!(server.lifecycle_hooks.is_empty());
}

#[test]
fn mcp_lifecycle_hooks_are_opt_in_and_do_not_bypass_approval() {
    let mut server = fake_config("", 30);
    server.lifecycle_hooks = vec!["tool_finished".into()];
    let loaded = load(&config_with(vec![("fake", server)]));
    let plain = tool(&loaded, "mcp__fake__plain");
    assert_eq!(plain.lifecycle_hooks().len(), 1);
    assert_eq!(plain.lifecycle_hooks()[0], "tool_finished");
    assert_eq!(plain.extension_api_version(), EXTENSION_API_VERSION);
    assert!(plain.needs_approval(&json!({"sensitive": "argument"})));

    plain.on_lifecycle_event(&LifecycleEvent {
        event: "tool_started".into(),
        session_id: Some("session".into()),
        turn_id: Some("turn".into()),
        tool_name: Some(plain.name().into()),
        is_error: None,
    });
    plain.on_lifecycle_event(&LifecycleEvent {
        event: "tool_finished".into(),
        session_id: Some("session".into()),
        turn_id: Some("turn".into()),
        tool_name: Some(plain.name().into()),
        is_error: Some(false),
    });
    assert!(plain.needs_approval(&json!({"sensitive": "argument"})));
}

#[test]
fn results_map_text_errors_and_other_content() {
    let loaded = load_fake();
    let echoed = call(&loaded, "mcp__fake__echo", json!({"text": "hello"}));
    assert!(!echoed.is_error);
    assert_eq!(echoed.text, "echo: hello");

    let failed = call(&loaded, "mcp__fake__fail", json!({}));
    assert!(failed.is_error);
    assert_eq!(failed.text, "it broke");

    let mixed = call(&loaded, "mcp__fake__mixed", json!({}));
    assert_eq!(
        mixed.text,
        "caption\n[image content, image/png, not shown]\n[resource: file:///r.txt]\nresource body"
    );

    let big = call(&loaded, "mcp__fake__big", json!({}));
    assert!(!big.is_error);
    assert!(
        big.text.len() < 70 * 1024 && big.text.contains("[output truncated"),
        "{}",
        big.text.len()
    );

    // Arguments that are not an object are sent as an empty object.
    assert_eq!(
        call(&loaded, "mcp__fake__bare", Value::Null).text,
        "bare args: {}"
    );
}

#[test]
fn server_noise_is_ignored_and_server_pings_are_answered() {
    // The fake prints a non-JSON line, a notification and a ping before answering `initialize`.
    let loaded = load_fake();
    assert_eq!(
        call(&loaded, "mcp__fake__pong", json!({})).text,
        "ping answered: true"
    );
}

#[test]
fn a_call_that_never_returns_times_out_and_the_server_stays_usable() {
    let loaded = load(&config_with(vec![("fake", fake_config("", 1))]));
    let started = Instant::now();
    let slow = call(&loaded, "mcp__fake__slow", json!({}));
    assert!(slow.is_error);
    assert!(
        slow.text.contains("did not answer `tools/call` within 1s"),
        "{}",
        slow.text
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        call(&loaded, "mcp__fake__echo", json!({"text": "after"})).text,
        "echo: after"
    );
}

#[test]
fn a_server_that_dies_mid_session_yields_clear_errors_without_hanging() {
    let loaded = load(&config_with(vec![("fake", fake_config("", 30))]));
    let started = Instant::now();
    let died = call(&loaded, "mcp__fake__die", json!({}));
    assert!(died.is_error);
    assert!(
        died.text.contains("MCP server `fake` is not running"),
        "{}",
        died.text
    );
    assert!(
        died.text.contains("fatal: kaboom"),
        "server stderr is quoted: {}",
        died.text
    );

    let later = call(&loaded, "mcp__fake__echo", json!({"text": "x"}));
    assert!(later.is_error);
    assert!(later.text.contains("is not running"), "{}", later.text);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "failed fast, not at the 30s request timeout"
    );
}

#[test]
#[cfg(unix)]
fn dropping_the_tools_kills_the_server_process() {
    let loaded = load_fake();
    let pid: libc::pid_t = call(&loaded, "mcp__fake__pid", json!({}))
        .text
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "server is running");
    drop(loaded);
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "server {pid} outlived its tools");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn broken_servers_only_produce_notices_and_good_ones_still_load() {
    let missing = McpServerConfig {
        program: "/nonexistent/dial-mcp-server".into(),
        args: vec![],
        env: BTreeMap::new(),
        timeout_secs: 5,
        lifecycle_hooks: vec![],
        url: None,
        bearer_env: None,
        headers: BTreeMap::new(),
    };
    let config = config_with(vec![
        ("good", fake_config("", 30)),
        ("missing", missing),
        ("badversion", fake_config("bad-protocol", 30)),
        ("silent", fake_config("silent", 30)),
        ("empty", fake_config("no-tools", 30)),
        ("badlist", fake_config("bad-list", 30)),
    ]);
    let started = Instant::now();
    let loaded = load_with(&config, Duration::from_secs(2));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "servers connect concurrently with a bounded budget"
    );

    assert!(
        loaded
            .tools
            .iter()
            .all(|t| t.name().starts_with("mcp__good__"))
    );
    assert_eq!(loaded.tools.len(), 12);
    let notice = |server: &str| {
        let prefix = format!("mcp `{server}`: ");
        let found: Vec<_> = loaded
            .notices
            .iter()
            .filter(|n| n.starts_with(&prefix))
            .collect();
        assert_eq!(found.len(), 1, "{server}: {:?}", loaded.notices);
        found[0].clone()
    };
    assert!(notice("missing").contains("starting `/nonexistent/dial-mcp-server`"));
    assert!(notice("badversion").contains("unsupported protocol version \"1999-01-01\""));
    assert!(notice("silent").contains("did not answer `initialize`"));
    assert!(notice("empty").contains("offers no tools"));
    assert!(notice("badlist").contains("list exploded"));
    assert_eq!(loaded.notices.len(), 5, "{:?}", loaded.notices);
}

#[test]
fn tools_from_servers_with_alike_names_never_collide() {
    let loaded = load(&config_with(vec![
        ("a.b", fake_config("", 30)),
        ("a_b", fake_config("", 30)),
    ]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let names: BTreeSet<_> = loaded.tools.iter().map(|t| t.name().to_owned()).collect();
    assert_eq!(names.len(), 24, "no name was shadowed");
    assert!(names.iter().all(|n| n.len() <= 64));
}

#[test]
fn nothing_is_started_without_configured_servers() {
    let loaded = load(&Config::default());
    assert!(loaded.tools.is_empty() && loaded.notices.is_empty());
}

#[test]
fn server_environment_drops_inherited_credentials_but_keeps_explicit_entries() {
    let inherited = || {
        [
            ("PATH", "/usr/bin"),
            ("OPENAI_API_KEY", "sk-inherited"),
            ("GITHUB_TOKEN", "ghp_inherited"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
    };
    let none = BTreeMap::new();
    assert_eq!(
        client::server_env(inherited(), &none),
        vec![("PATH".to_owned(), "/usr/bin".to_owned())]
    );

    let explicit = BTreeMap::from([("GITHUB_TOKEN".to_owned(), "ghp_configured".to_owned())]);
    let env: BTreeMap<_, _> = client::server_env(inherited(), &explicit)
        .into_iter()
        .collect();
    assert_eq!(env["GITHUB_TOKEN"], "ghp_configured");
    assert_eq!(env["PATH"], "/usr/bin");
    assert!(!env.contains_key("OPENAI_API_KEY"));
}

/// A minimal streamable-HTTP MCP server: JSON for `initialize`, SSE for `tools/list`, 202 for notifications.
fn http_server(token: &'static str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut stream = stream;
            let (mut length, mut auth, mut session) = (0usize, String::new(), String::new());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if lower.starts_with("authorization:") {
                    auth = line.split_once(':').unwrap().1.trim().to_owned();
                }
                if lower.starts_with("mcp-session-id:") {
                    session = line.split_once(':').unwrap().1.trim().to_owned();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let message: Value = serde_json::from_slice(&body).unwrap();
            let method = message["method"].as_str().unwrap_or("").to_owned();
            log.lock()
                .unwrap()
                .push(format!("{method}|{auth}|{session}"));
            let mut respond = |status: &str, ctype: &str, extra: &str, body: String| {
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            };
            if auth != format!("Bearer {token}") {
                respond("401 Unauthorized", "text/plain", "", "no".into());
                continue;
            }
            let id = message["id"].clone();
            match method.as_str() {
                "initialize" => respond("200 OK", "application/json", "Mcp-Session-Id: sess-1\r\n",
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"h","version":"1"}}}).to_string()),
                "tools/list" => respond("200 OK", "text/event-stream", "", format!("event: message\ndata: {}\n\n",
                    json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"hello","description":"d","inputSchema":{"type":"object"}}]}}))),
                "tools/call" => respond("200 OK", "application/json", "",
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"hi over http"}]}}).to_string()),
                _ => respond("202 Accepted", "text/plain", "", String::new()),
            }
        }
    });
    (url, seen)
}

#[test]
fn http_servers_handshake_with_a_bearer_token_session_and_sse_replies() {
    let (url, seen) = http_server("s3cret");
    // SAFETY-free: the variable is unique to this test.
    unsafe { std::env::set_var("DIAL_TEST_MCP_TOKEN", "s3cret") };
    let server = McpServerConfig {
        url: Some(url.clone()),
        bearer_env: Some("DIAL_TEST_MCP_TOKEN".into()),
        program: Default::default(),
        args: vec![],
        env: BTreeMap::new(),
        timeout_secs: 10,
        lifecycle_hooks: vec![],
        headers: BTreeMap::new(),
    };
    let loaded = load(&config_with(vec![("web", server)]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let out = call(&loaded, "mcp__web__hello", json!({}));
    assert!(
        !out.is_error && out.text.contains("hi over http"),
        "{}",
        out.text
    );
    let log = seen.lock().unwrap().clone();
    assert!(log.iter().all(|l| l.contains("|Bearer s3cret|")), "{log:?}");
    assert!(
        log.iter()
            .any(|l| l.starts_with("tools/call|") && l.ends_with("|sess-1")),
        "session id is echoed: {log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.starts_with("notifications/initialized")),
        "{log:?}"
    );
}

#[test]
fn http_credential_failures_are_clear_and_config_needs_exactly_one_transport() {
    let (url, _) = http_server("right");
    unsafe { std::env::set_var("DIAL_TEST_MCP_WRONG", "wrong") };
    let server = McpServerConfig {
        url: Some(url),
        bearer_env: Some("DIAL_TEST_MCP_WRONG".into()),
        program: Default::default(),
        args: vec![],
        env: BTreeMap::new(),
        timeout_secs: 5,
        lifecycle_hooks: vec![],
        headers: BTreeMap::new(),
    };
    let loaded = load(&config_with(vec![("web", server.clone())]));
    assert!(
        loaded.tools.is_empty() && loaded.notices[0].contains("refused the credentials"),
        "{:?}",
        loaded.notices
    );
    assert!(
        config_with(vec![("web", server.clone())])
            .validate()
            .is_ok()
    );
    let both = McpServerConfig {
        program: "x".into(),
        ..server.clone()
    };
    assert!(config_with(vec![("both", both)]).validate().is_err());
    let neither = McpServerConfig {
        url: None,
        ..server.clone()
    };
    assert!(config_with(vec![("neither", neither)]).validate().is_err());
    let insecure = McpServerConfig {
        url: Some("http://example.com/mcp".into()),
        ..server
    };
    assert!(
        config_with(vec![("insecure", insecure)])
            .validate()
            .is_err()
    );
}
