//! End-to-end tests against a scripted server (`fake.rs`) running as a real child process: the test
//! binary re-executes itself with [`fake::ENV`] set and runs the `fake_server` test below as the server.

use super::*;
use crate::tools::integrations::ext::{EXTENSION_API_VERSION, LifecycleEvent, ToolContext};
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
            "tools::integrations::mcp::tests::fake_server",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(String::from)
        .to_vec(),
        env: BTreeMap::from([(fake::ENV.to_owned(), flags.to_owned())]),
        timeout_secs,
        lifecycle_hooks: vec![],
        url: None,
        ..McpServerConfig::default()
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

fn call(loaded: &McpLoad, name: &str, args: Value) -> crate::runtime::tool_output::ToolOutput {
    let (_dir, workspace) = testkit::workspace();
    let config = Config::default();
    tool(loaded, name).call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
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
fn malformed_tool_arguments_are_rejected_without_remote_effects() {
    let loaded = load_fake();
    for args in [
        Value::Null,
        json!([]),
        json!("text"),
        json!(42),
        json!(true),
    ] {
        let output = call(&loaded, "mcp__fake__echo", args);
        assert!(output.is_error);
        assert!(output.text.contains("must be a JSON object"));
        assert_eq!(
            output.execution.effects,
            crate::runtime::execution::Effects::None
        );
    }
    let output = call(&loaded, "mcp__fake__echo", json!({"text": "still works"}));
    assert!(!output.is_error);
    assert!(output.text.contains("still works"));
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

    // An explicit empty object is valid for tools without parameters.
    assert_eq!(
        call(&loaded, "mcp__fake__bare", json!({})).text,
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
    assert_eq!(
        slow.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
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
    assert_eq!(
        died.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
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
    assert!(!later.is_error, "reconnected on demand: {}", later.text);
    assert_eq!(later.text, "echo: x");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "reconnected, not stuck at the 30s request timeout"
    );
}

#[test]
fn dropping_the_tools_kills_the_server_process() {
    let loaded = load_fake();
    let pid: u32 = call(&loaded, "mcp__fake__pid", json!({}))
        .text
        .parse()
        .unwrap();
    assert!(crate::platform::process::alive(pid), "server is running");
    drop(loaded);
    let deadline = Instant::now() + Duration::from_secs(5);
    while crate::platform::process::alive(pid) {
        assert!(Instant::now() < deadline, "server {pid} outlived its tools");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cancelling_before_send_never_invokes_the_remote_tool() {
    let loaded = load_fake();
    let pid = call(&loaded, "mcp__fake__pid", json!({})).text;
    let (_dir, workspace) = testkit::workspace();
    let config = Config::default();
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let output = tool(&loaded, "mcp__fake__die").call_streaming(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({}),
        &|_| {},
        &cancel,
    );
    assert!(output.is_error);
    assert_eq!(
        output.execution.effects,
        crate::runtime::execution::Effects::None
    );
    assert_eq!(
        call(&loaded, "mcp__fake__pid", json!({})).text,
        pid,
        "the same server remains alive because the die tool was never sent"
    );
}

#[test]
fn broken_servers_only_produce_notices_and_good_ones_still_load() {
    let missing = McpServerConfig {
        program: "/nonexistent/dial-mcp-server".into(),
        args: vec![],
        env: BTreeMap::new(),
        timeout_secs: 5,
        lifecycle_hooks: vec![],
        ..McpServerConfig::default()
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
    let loaded = load_with(&config, Duration::from_secs(1));
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
fn inventory_reports_runtime_status_without_exposing_configuration_secrets() {
    let mut disabled = fake_config("", 30);
    disabled.enabled = false;
    disabled
        .env
        .insert("PRIVATE_TEST_VALUE".into(), "not-for-model-output".into());
    let config = config_with(vec![
        ("live", fake_config("", 30)),
        ("empty", fake_config("no-tools", 30)),
        ("failed", fake_config("bad-protocol", 30)),
        ("disabled", disabled),
    ]);
    let loaded = load(&config);
    let inventory = loaded.inventory_tool(&config);
    assert!(!inventory.needs_approval(&json!({})));
    // Install mutates the global config, so the inventory is never read-only. Only list and
    // search are reads; every action is scheduled exclusively because the hint is false.
    assert!(!inventory.read_only_hint());
    for action in ["list", "search"] {
        let args = json!({"action": action});
        assert!(!inventory.needs_approval(&args), "{action} only reads");
        assert_eq!(
            inventory.concurrency(&args),
            crate::tools::integrations::ext::Concurrency::Exclusive
        );
    }
    let install = json!({"action": "install", "id": "example"});
    assert!(inventory.needs_approval(&install));
    assert_eq!(
        inventory.concurrency(&install),
        crate::tools::integrations::ext::Concurrency::Exclusive
    );
    let (_dir, workspace) = testkit::workspace();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let output = inventory.call(&ctx, &json!({}));
    assert!(!output.is_error);
    assert!(!output.text.contains("not-for-model-output"));
    assert!(!output.text.contains("PRIVATE_TEST_VALUE"));
    let listing: Value = serde_json::from_str(&output.text).unwrap();
    let server = |name: &str| {
        listing["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == name)
            .unwrap()
    };
    assert_eq!(server("disabled")["status"], "disabled");
    assert_eq!(server("failed")["status"], "failed");
    assert_eq!(server("empty")["status"], "connected");
    assert_eq!(server("empty")["tools"], json!([]));
    assert_eq!(server("live")["status"], "connected");
    assert_eq!(server("live")["capabilities"], json!(["tools"]));
    assert!(
        server("live")["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "mcp__live__echo")
    );
    // Inventory observes, but must not start or reconnect a server as a side effect.
    let session = loaded
        .sessions
        .iter()
        .find(|session| session.name() == "live")
        .unwrap();
    let _turn = session.turn.lock(); // Keep automatic recovery from racing this observation.
    let _ = session.current().request(
        "tools/call",
        json!({"name":"die","arguments":{}}),
        Duration::from_secs(5),
    );
    let listing: Value = serde_json::from_str(&inventory.call(&ctx, &json!({})).text).unwrap();
    assert_eq!(
        listing["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == "live")
            .unwrap()["status"],
        "disconnected"
    );
}

#[test]
fn rebuilt_registry_discovers_and_calls_installed_mcp_tools() {
    use crate::tools::integrations::ext::Registry;
    let before = Registry::standard(&Config::default(), Vec::new());
    assert!(before.contains("mcp_servers"));
    assert!(!before.contains("mcp__installed__echo"));
    let config = config_with(vec![("installed", fake_config("", 30))]);
    let registry = Registry::standard(&config, Vec::new());
    let (_dir, workspace) = testkit::workspace();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let listing = registry.get("mcp_servers").unwrap().call(&ctx, &json!({}));
    let listing: Value = serde_json::from_str(&listing.text).unwrap();
    assert_eq!(listing["servers"][0]["status"], "connected");
    let name = listing["servers"][0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "mcp__installed__echo")
        .unwrap()["name"]
        .as_str()
        .unwrap();
    let tool = registry.get(name).unwrap();
    assert!(
        tool.needs_approval(&json!({})),
        "inventory does not grant approval"
    );
    assert_eq!(
        tool.call(&ctx, &json!({"text":"installed"})).text,
        "echo: installed"
    );
    let mut disabled = config.clone();
    disabled.mcp.get_mut("installed").unwrap().enabled = false;
    let reloaded = Registry::standard(&disabled, Vec::new());
    assert!(!reloaded.contains(name));
    let listing = reloaded.get("mcp_servers").unwrap().call(&ctx, &json!({}));
    let listing: Value = serde_json::from_str(&listing.text).unwrap();
    assert_eq!(listing["servers"][0]["status"], "disabled");
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
            let mut get = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_lowercase();
                if line.starts_with("GET ") {
                    get = true;
                }
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
            if get {
                let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
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
        ..McpServerConfig::default()
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
        ..McpServerConfig::default()
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

/// A streamable-HTTP server whose `tools/call` stream sends a `ping` request and holds the final
/// reply until that ping is answered on a separate POST (or gives up after a few seconds).
fn ping_first_server() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::mpsc;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let (answered, answer) = mpsc::channel::<()>();
    let answer = Arc::new(std::sync::Mutex::new(answer));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (answered, answer) = (answered.clone(), answer.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                let (mut length, mut get) = (0usize, false);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    get |= line.starts_with("GET ");
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let head = |status: &str, ctype: &str, extra: &str| {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Connection: close\r\n"
                    )
                };
                if get {
                    let _ = stream.write_all(
                        format!(
                            "{}Content-Length: 0\r\n\r\n",
                            head("405 Method Not Allowed", "text/plain", "")
                        )
                        .as_bytes(),
                    );
                    return;
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                let id = message["id"].clone();
                let fixed =
                    |stream: &mut std::net::TcpStream, ctype: &str, extra: &str, body: String| {
                        let _ = stream.write_all(
                            format!(
                                "{}Content-Length: {}\r\n\r\n{body}",
                                head("200 OK", ctype, extra),
                                body.len()
                            )
                            .as_bytes(),
                        );
                    };
                match message["method"].as_str() {
                    Some("initialize") => fixed(&mut stream, "application/json", "Mcp-Session-Id: s\r\n",
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"p","version":"1"}}}).to_string()),
                    Some("tools/list") => fixed(&mut stream, "application/json", "",
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"hello","description":"d","inputSchema":{"type":"object"}}]}}).to_string()),
                    Some("tools/call") => {
                        let _ = stream.write_all(format!("{}\r\n", head("200 OK", "text/event-stream", "")).as_bytes());
                        let ping = json!({"jsonrpc":"2.0","id":"srv-1","method":"ping"});
                        let _ = stream.write_all(format!("event: message\ndata: {ping}\n\n").as_bytes());
                        let _ = stream.flush();
                        let got = answer.lock().unwrap().recv_timeout(Duration::from_secs(3));
                        if got.is_ok() {
                            let done = json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"after ping"}]}});
                            let _ = stream.write_all(format!("event: message\ndata: {done}\n\n").as_bytes());
                        }
                    }
                    _ => {
                        if message["id"] == "srv-1" && message.get("result").is_some() {
                            let _ = answered.send(());
                        }
                        let _ = stream.write_all(format!("{}Content-Length: 0\r\n\r\n", head("202 Accepted", "text/plain", "")).as_bytes());
                    }
                }
            });
        }
    });
    url
}

#[test]
fn server_requests_on_an_open_call_stream_are_answered_before_it_closes() {
    let server = McpServerConfig {
        url: Some(ping_first_server()),
        program: Default::default(),
        args: vec![],
        timeout_secs: 20,
        ..McpServerConfig::default()
    };
    let loaded = load(&config_with(vec![("ping", server)]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let out = call(&loaded, "mcp__ping__hello", json!({}));
    assert!(
        !out.is_error && out.text.contains("after ping"),
        "{}",
        out.text
    );
}

#[test]
fn listing_fetched_before_an_invalidation_is_not_cached() {
    let mut listing = Listing::default();
    let before = listing.generation;
    listing.reset();
    listing.store(before, |cache| {
        cache.resources = Some(json!({"stale": true}))
    });
    assert!(
        listing.resources.is_none(),
        "reconnect must beat an in-flight list"
    );

    let loaded = load_fake();
    let session = &loaded.sessions[0];
    let before = session.listing.lock().generation;
    session.on_signal(Signal::PromptsChanged);
    let after = session.listing.lock().generation;
    assert_ne!(before, after);
    session
        .listing
        .lock()
        .store(before, |cache| cache.prompts = Some(json!({"stale": true})));
    assert!(session.listing.lock().prompts.is_none());
    session
        .listing
        .lock()
        .store(after, |cache| cache.prompts = Some(json!({"fresh": true})));
    assert!(session.listing.lock().prompts.is_some());
}
