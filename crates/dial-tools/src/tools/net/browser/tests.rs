use crate::config::tools::browser::BrowserConfig;

use super::*;
use std::time::Instant;
#[test]
fn sandbox_dependency_paths_are_not_profile_injection() {
    assert!(profile_path(Path::new("/tmp/a\"(allow network*)")).is_err());
    assert!(profile_path(Path::new("relative")).is_err());
    assert_eq!(profile_path(Path::new("/tmp/ok")).unwrap(), "\"/tmp/ok\"");
}
#[test]
fn dependency_ancestors_only_receive_literal_metadata_permission() {
    let rules = dependency_metadata(&[
        Path::new("/private/tmp/operator/node_modules"),
        Path::new("/Users/operator/chromium/bin/chrome"),
    ])
    .unwrap();
    assert!(rules.starts_with("(allow file-read-metadata"));
    assert!(rules.contains("(literal \"/private/tmp\")"));
    assert!(rules.contains("(literal \"/Users/operator\")"));
    assert!(
        !rules.contains("file-read-data")
            && !rules.contains("file-read*")
            && !rules.contains("subpath")
    );
}
#[test]
fn browser_ipc_permission_is_restricted_to_private_state_unix_sockets() {
    let rules = state_unix_ipc(Path::new("/private/tmp/dial-browser-owned")).unwrap();
    assert!(rules.contains("local unix-socket") && rules.contains("remote unix-socket"));
    assert!(rules.contains("^/private/tmp/dial\\-browser\\-owned/"));
    assert!(
        !rules.contains("tcp") && !rules.contains("udp") && !rules.contains("(allow network*)")
    );
}
#[test]
fn trusted_browser_read_scope_includes_app_bundle_without_parent_directory_contents() {
    assert_eq!(
        chromium_bundle_root(Path::new(
            "/Users/operator/cache/Chrome.app/Contents/MacOS/Chrome"
        ))
        .unwrap(),
        Path::new("/Users/operator/cache/Chrome.app")
    );
    assert_eq!(
        chromium_bundle_root(Path::new("/opt/chromium/chrome")).unwrap(),
        Path::new("/opt/chromium")
    );
}
#[test]
fn bridge_refuses_private_addresses_and_non_http_schemes() {
    for url in [
        "http://127.0.0.1/",
        "http://169.254.169.254/",
        "file:///etc/passwd",
    ] {
        let result = bridge(
            &json!({"callback":"http","url":url,"method":"GET","headers":{},"body":""}),
            Duration::from_secs(2),
            &[],
            Arc::new(AtomicBool::new(false)),
        );
        assert!(result.is_err(), "{url}");
    }
}
#[test]
fn bridge_rejects_oversize_upload_before_network() {
    let body = STANDARD.encode(vec![0u8; 1024 * 1024 + 1]);
    let error = bridge(
        &json!({"callback":"http","url":"https://example.com/","method":"POST","body":body}),
        Duration::from_secs(1),
        &[],
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(error.contains("1 MiB"));
}
#[test]
fn every_browser_action_requires_approval_and_policy_applies_before_startup() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.sandbox.mode = SandboxMode::Strict;
    let tool = tools(&config).pop().unwrap();
    for action in [
        "open",
        "evaluate",
        "evidence",
        "screenshot",
        "close",
        "shutdown",
    ] {
        assert!(tool.needs_approval(&json!({"action":action})));
    }
    let output = tool.call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({"action":"open","url":"https://example.com/"}),
    );
    assert!(output.is_error && output.text.contains("network access is disabled"));
}
#[test]
fn approved_loopback_grants_are_exact_bounded_and_never_come_from_page_actions() {
    let config = BrowserConfig::default();
    let mut hosts = Vec::new();
    permit_requested_localhost(
        &mut hosts,
        &config,
        &json!({"action":"open", "url":"http://127.0.0.1:3000/studio/porting-windowing"}),
    )
    .unwrap();
    assert_eq!(hosts, ["127.0.0.1:3000"]);
    for (action, url) in [
        ("evaluate", "http://127.0.0.1:4000/"),
        ("click", "http://localhost:3000/"),
        ("open", "http://evil.example:3000/"),
        ("navigate", "http://169.254.169.254/"),
        ("open", "http://10.1.2.3:3000/"),
    ] {
        permit_requested_localhost(&mut hosts, &config, &json!({"action":action, "url":url}))
            .unwrap();
    }
    let resolver = BrowserResolver {
        allowed_hosts: hosts.clone(),
        interrupt: Arc::new(AtomicBool::new(false)),
    };
    let check = |url: &str, ip: &str| resolver.check(&parse_url(url).unwrap(), ip.parse().unwrap());
    assert_eq!(check("http://127.0.0.1:3000/", "127.0.0.1"), None);
    for (url, ip) in [
        ("http://127.0.0.1:4000/", "127.0.0.1"),
        ("http://localhost:3000/", "127.0.0.1"),
        ("http://evil.example:3000/", "127.0.0.1"),
        ("http://169.254.169.254/", "169.254.169.254"),
        ("http://127.0.0.1:3000/", "169.254.169.254"),
        ("http://10.1.2.3:3000/", "10.1.2.3"),
    ] {
        assert!(check(url, ip).is_some(), "{url} via {ip}");
    }
    assert_eq!(hosts, ["127.0.0.1:3000"]);
    permit_requested_localhost(
        &mut hosts,
        &config,
        &json!({"action":"navigate", "url":"http://localhost:4000/"}),
    )
    .unwrap();
    assert_eq!(hosts, ["127.0.0.1:3000", "localhost:4000"]);
    let strict = BrowserConfig {
        auto_localhost: false,
        ..Default::default()
    };
    let mut denied = Vec::new();
    permit_requested_localhost(
        &mut denied,
        &strict,
        &json!({"action":"open", "url":"http://localhost:3000/"}),
    )
    .unwrap();
    assert!(denied.is_empty());
    let mut full = vec!["already:80".into(); 256];
    assert!(
        permit_requested_localhost(
            &mut full,
            &config,
            &json!({"action":"open", "url":"http://localhost:3000/"})
        )
        .is_err()
    );
    assert!(
        permit_requested_localhost(
            &mut hosts,
            &config,
            &json!({"action":"open", "url":"http://localhost:0/"})
        )
        .is_err()
    );
    assert!(
        permit_requested_localhost(
            &mut hosts,
            &config,
            &json!({"action":"open", "url":"http://localhost:65536/"})
        )
        .is_err()
    );
}

#[test]
fn operator_allowlist_is_exact_and_never_allows_metadata() {
    let resolver = BrowserResolver {
        allowed_hosts: vec![
            "127.0.0.1:8080".into(),
            "localhost:8080".into(),
            "169.254.169.254:80".into(),
            "10.1.2.3:8080".into(),
            "evil.example:8080".into(),
        ],
        interrupt: Arc::new(AtomicBool::new(false)),
    };
    let check = |url: &str, address: &str| {
        resolver.check(&parse_url(url).unwrap(), address.parse().unwrap())
    };
    assert_eq!(check("http://127.0.0.1:8080/", "127.0.0.1"), None);
    assert_eq!(check("http://localhost:8080/", "::1"), None);
    assert!(check("http://127.0.0.1:8081/", "127.0.0.1").is_some());
    assert!(check("http://evil.example:8080/", "127.0.0.1").is_some());
    assert!(check("http://169.254.169.254/", "169.254.169.254").is_some());
    assert_eq!(check("http://10.1.2.3:8080/", "10.1.2.3"), None);
    assert!(check("http://10.1.2.3:8080/", "169.254.169.254").is_some());
    let mut config = BrowserConfig::default();
    for invalid in [
        "localhost",
        "localhost:0",
        "*.example:80",
        "http://localhost:80",
        "localhost:80/path",
        "LOCALHOST:80",
    ] {
        config.allowed_hosts = vec![invalid.into()];
        assert!(config.validate_allowed_hosts().is_err(), "{invalid}");
    }
}
#[test]
fn workspace_playwright_is_resolved_without_executing_it_then_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let escaped = outside.path().join("preflight-escaped");
    let module = dir.path().join("node_modules/playwright");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("package.json"),
        r#"{"name":"playwright","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        module.join("index.js"),
        format!(
            "require('node:fs').writeFileSync({}, 'escaped'); throw Error('executed');",
            serde_json::to_string(&escaped).unwrap()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            module.join("index.js"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let root = workspace.root();
    let node = resolve_operator_executable(root, "node", "browser.node").unwrap();
    let trusted_module = outside.path().join("trusted.cjs");
    std::fs::write(
        &trusted_module,
        "exports.chromium = { executablePath: () => (process.env.PLAYWRIGHT_BROWSERS_PATH || 'external') + '/chromium' };",
    )
    .unwrap();
    let runtime = runtime::Runtime {
        root: outside.path().to_path_buf(),
        entry: trusted_module.clone(),
        cli: outside.path().join("cli.js"),
        readability: outside.path().join("Readability.js"),
        browsers: outside.path().join("browsers"),
    };
    let check = |module: &str| {
        let browser_config = BrowserConfig {
            playwright_module: module.into(),
            ..Default::default()
        };
        probe(
            &ctx,
            &node,
            &browser_config,
            &runtime,
            &Arc::new(AtomicI32::new(0)),
            &AtomicBool::new(false),
            Some(Instant::now() + Duration::from_secs(5)),
        )
    };
    assert!(
        check(module.to_str().unwrap())
            .unwrap_err()
            .contains("workspace-owned")
    );
    assert!(!escaped.exists());
    // An operator-owned module completes both requests, proving this uses the production probe.
    let (_, meta) = check(trusted_module.to_str().unwrap()).unwrap();
    assert_eq!(meta["executable"], "external/chromium");
    // The built-in cache still completes the second request with its private browser path.
    let (entry, meta) = check("").unwrap();
    assert_eq!(entry, trusted_module.canonicalize().unwrap());
    assert_eq!(
        meta["executable"],
        format!("{}/chromium", runtime.browsers.display())
    );
    #[cfg(unix)]
    {
        let symlink = outside.path().join("symlink.js");
        std::os::unix::fs::symlink(module.join("index.js"), &symlink).unwrap();
        assert!(
            check(symlink.to_str().unwrap())
                .unwrap_err()
                .contains("workspace-owned")
        );
        assert!(!escaped.exists());
        assert!(
            resolve_operator_executable(
                root,
                module.join("index.js").to_str().unwrap(),
                "browser.node"
            )
            .unwrap_err()
            .contains("outside")
        );
        assert!(
            resolve_operator_executable(root, symlink.to_str().unwrap(), "browser.node").is_err()
        );
    }
}
#[test]
fn guarded_http_cancellation_interrupts_stalled_headers_and_body_and_closes_socket() {
    use std::io::Write;
    for send_headers in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).unwrap();
            if send_headers {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
            ready_tx.send(()).unwrap();
            let closed = loop {
                match socket.read(&mut buffer) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    // Linux may reset a connection when cancellation drops
                    // unread response bytes. RST proves closure just as EOF does;
                    // a read timeout still fails this leak check.
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                        break true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break false,
                }
            };
            closed_tx.send(closed).unwrap();
        });
        let token = Arc::new(AtomicBool::new(false));
        let running = token.clone();
        let worker = std::thread::spawn(move || {
            bridge(
                &json!({"callback":"http","url":format!("http://127.0.0.1:{port}/"),"method":"GET","headers":{}}),
                Duration::from_secs(3600),
                &[format!("127.0.0.1:{port}")],
                running,
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        token.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(closed_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        server.join().unwrap();
    }
}
struct LocalApp {
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl LocalApp {
    fn start() -> Self {
        Self::start_with_header_poll(Duration::from_millis(25))
    }

    fn start_with_header_poll(header_poll: Duration) -> Self {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            // Browsers may open idle preconnect sockets. Serve accepted connections independently
            // so one empty socket cannot prevent the guarded client from sending its real request.
            std::thread::scope(|scope| {
                while !stopping.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let stopping = &stopping;
                            scope.spawn(move || {
                        stream
                            .set_read_timeout(Some(header_poll))
                            .unwrap();
                        // TCP can split even the request line. Wait for a complete bounded
                        // header before routing or replying; an early close also races the
                        // guarded client's remaining header writes.
                        let mut request = Vec::new();
                        let mut chunk = [0u8; 4096];
                        while request.len() < 16 * 1024
                            && !stopping.load(Ordering::Acquire)
                            && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                        {
                            match stream.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(count) => request.extend_from_slice(&chunk[..count]),
                                Err(error)
                                    if error.kind() == std::io::ErrorKind::Interrupted =>
                                {
                                    continue;
                                }
                                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                                    // A preconnect or partial request can stay idle legitimately. This
                                    // timeout only polls fixture shutdown; it is not a header deadline.
                                    continue;
                                }
                                Err(error) => {
                                    eprintln!(
                                        "local browser fixture header read failed after {} bytes: {error}",
                                        request.len()
                                    );
                                    break;
                                }
                            }
                        }
                        if !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                            return;
                        }
                        let response = if String::from_utf8_lossy(&request)
                            .starts_with("GET /redirect ")
                        {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/blocked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            let article =
                                String::from_utf8_lossy(&request).starts_with("GET /article ");
                            let body = if article {
                                "<!doctype html><title>Dial article</title><article><h1>Dial article</h1><p>Local browser readable extraction keeps the substantive paragraphs of this fixture page, which is long enough for Readability to score it as an article rather than navigation chrome.</p><p>A second paragraph repeats the claim with more ordinary prose so the extracted text is clearly article content from the local fixture server.</p><p>A third paragraph adds further descriptive sentences about the local test page so the readable extraction has a stable body of text to return to the caller.</p></article>"
                            } else {
                                "<!doctype html><title>Dial local smoke</title><input id='name'><button id='go' onclick=\"document.querySelector('#result').textContent=document.querySelector('#name').value;console.log('verified-click')\">Go</button><p id='result'></p>"
                            };
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        if let Err(error) = stream.write_all(response.as_bytes()) {
                            eprintln!("local browser fixture response write failed: {error}");
                        }
                        });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("local browser fixture: {e}"),
                    }
                }
            });
        });
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for LocalApp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[test]
fn local_http_fixture_serves_requests_while_a_preconnect_socket_is_idle() {
    use std::io::{Read, Write};
    let app = LocalApp::start();
    let idle = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
    let mut client = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client
        .write_all(b"GET /first HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("Dial local smoke"));
    drop(idle);
}

#[test]
fn local_http_fixture_preserves_delayed_headers_and_closes_idle_connections_on_shutdown() {
    use std::io::{Read, Write};
    let app = LocalApp::start_with_header_poll(Duration::from_millis(5));
    let mut client = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    client.write_all(b"GET /fi").unwrap();
    let error = client
        .read(&mut [0u8; 1])
        .expect_err("partial headers cannot receive a response or be closed");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    client
        .write_all(b"rst HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    let idle = std::net::TcpStream::connect(("127.0.0.1", app.port)).unwrap();
    let (closed, shutdown) = std::sync::mpsc::channel();
    let join = std::thread::spawn(move || {
        drop(app);
        closed.send(()).unwrap();
    });
    shutdown
        .recv_timeout(Duration::from_secs(2))
        .expect("fixture shutdown must not wait for idle clients to send headers");
    drop(idle);
    join.join().unwrap();
}
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires operator-installed Node/Playwright/Chromium and sandbox-exec; no public internet needed"]
fn chromium_approved_localhost_without_manual_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let app = LocalApp::start();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.sandbox.mode = SandboxMode::Dev;
    config.sandbox.allow_network = true;
    config.browser.playwright_module = std::env::var("DIAL_PLAYWRIGHT").unwrap();
    assert!(config.browser.allowed_hosts.is_empty());
    let tool = tools(&config).pop().unwrap();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let blank = tool.call(&ctx, &json!({"action":"open", "url":"about:blank"}));
    assert!(!blank.is_error, "{}", blank.text);
    let blank: Value = serde_json::from_str(&blank.text).unwrap();
    assert_eq!(blank["url"], "about:blank");
    let evaluated = tool.call(
        &ctx,
        &json!({"action":"evaluate", "expression":"document.URL"}),
    );
    assert!(
        !evaluated.is_error && evaluated.text.contains("about:blank"),
        "{}",
        evaluated.text
    );
    let closed = tool.call(&ctx, &json!({"action":"close", "tab":blank["tab"]}));
    assert!(!closed.is_error, "{}", closed.text);
    // A managed Node preview must survive start/wait and other tool calls until explicitly
    // stopped. No Nuxt/package download or user project is involved in this lifecycle check.
    std::fs::write(dir.path().join("preview.cjs"), "const http=require('node:http'); const server=http.createServer((req,res)=>{res.writeHead(200,{'content-type':'text/html'});res.end('<title>Managed preview</title><p>Preview stays running between tools</p>')});server.listen(0,'127.0.0.1',()=>console.log('READY http://127.0.0.1:'+server.address().port+'/')); ").unwrap();
    let policy = crate::tools::exec::sandbox::Policy::Dev {
        deny_read: Vec::new(),
        extra_write: Vec::new(),
        allow_network: true,
    };
    let sandbox = crate::tools::exec::sandbox::Sandbox::probe_policy(dir.path(), &policy);
    sandbox.available().unwrap();
    let preview_workspace = workspace.clone().with_policy(policy).with_sandbox(sandbox);
    let preview_ctx = ToolContext {
        workspace: &preview_workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let bg = crate::tools::exec::background::tools(&config)
        .pop()
        .unwrap();
    let started = bg.call(
        &preview_ctx,
        &json!({"action":"start","name":"preview","command":"node preview.cjs"}),
    );
    assert!(!started.is_error, "{}", started.text);
    let ready = bg.call(&preview_ctx, &json!({"action":"wait","name":"preview","until":"READY http://127\\.0\\.0\\.1:","timeout":10}));
    assert!(
        !ready.is_error && ready.text.starts_with("ready:"),
        "{}",
        ready.text
    );
    let preview_url = ready
        .text
        .lines()
        .find_map(|line| line.strip_prefix("> READY "))
        .unwrap();
    let running = bg.call(&preview_ctx, &json!({"action":"list"}));
    assert!(running.text.contains("1 running"), "{}", running.text);
    let page = tool.call(&ctx, &json!({"action":"open","url":preview_url}));
    assert!(!page.is_error, "{}", page.text);
    let page: Value = serde_json::from_str(&page.text).unwrap();
    assert_eq!(page["status"], 200);
    assert_eq!(page["title"], "Managed preview");
    let pixels = tool.call(&ctx, &json!({"action":"screenshot"}));
    assert!(
        !pixels.is_error && pixels.text.contains("image/png"),
        "{}",
        pixels.text
    );
    let killed = bg.call(&preview_ctx, &json!({"action":"kill","name":"preview"}));
    assert!(!killed.is_error, "{}", killed.text);
    let stopped = tool.call(&ctx, &json!({"action":"navigate","url":preview_url}));
    assert!(
        stopped.is_error && stopped.text.contains("Connection refused"),
        "{}",
        stopped.text
    );
    // A failed request closes the session, so the next open must recover from scratch.
    assert!(!tool.call(&ctx, &json!({"action":"shutdown"})).is_error);
    let url = format!("http://127.0.0.1:{}/studio/porting-windowing", app.port);
    let opened = tool.call(&ctx, &json!({"action":"open", "url":url}));
    assert!(!opened.is_error, "{}", opened.text);
    let result: Value = serde_json::from_str(&opened.text).unwrap();
    assert_eq!(result["status"], 200);
    assert_eq!(result["title"], "Dial local smoke");
    let tab = result["tab"].as_u64().unwrap();
    let screenshot = tool.call(&ctx, &json!({"action":"screenshot", "tab":tab}));
    assert!(!screenshot.is_error, "{}", screenshot.text);
    let screenshot: Value = serde_json::from_str(&screenshot.text).unwrap();
    let png = STANDARD
        .decode(screenshot["artifact"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    if let Ok(path) = std::env::var("DIAL_TEST_BROWSER_SCREENSHOT") {
        std::fs::write(path, &png).unwrap();
    }
    // An unapproved localhost alias in a redirect must still be blocked.
    let rejected = tool.call(&ctx, &json!({"action":"navigate", "tab":tab, "url":format!("http://127.0.0.1:{}/redirect", app.port)}));
    assert!(
        rejected.is_error && rejected.text.contains("loopback"),
        "{}",
        rejected.text
    );
    tool.call(&ctx, &json!({"action":"shutdown"}));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires operator-installed Node/Playwright/Chromium and sandbox-exec; no public internet needed"]
fn chromium_end_to_end_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let app = LocalApp::start();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.sandbox.mode = SandboxMode::Dev;
    config.sandbox.allow_network = true;
    config.browser.auto_localhost = false;
    config.browser.allowed_hosts = vec![format!("127.0.0.1:{}", app.port)];
    if let Ok(module) = std::env::var("DIAL_PLAYWRIGHT") {
        config.browser.playwright_module = module;
    }
    let tool = tools(&config).pop().unwrap();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let call = |args: Value| {
        let output = tool.call(&ctx, &args);
        assert!(!output.is_error, "{}", output.text);
        serde_json::from_str::<Value>(&output.text).unwrap()
    };
    let opened = call(json!({"action":"open","url":format!("http://127.0.0.1:{}/first",app.port)}));
    let tab = opened["tab"].as_u64().unwrap();
    assert_eq!(opened["status"], 200);
    let navigated = call(
        json!({"action":"navigate","tab":tab,"url":format!("http://127.0.0.1:{}/actual",app.port)}),
    );
    assert_eq!(navigated["title"], "Dial local smoke");
    call(json!({"action":"fill","tab":tab,"selector":"#name","text":"verified"}));
    call(json!({"action":"click","tab":tab,"selector":"#go"}));
    assert_eq!(
        call(
            json!({"action":"evaluate","tab":tab,"expression":"document.querySelector('#result').textContent"})
        )["value"],
        "verified"
    );
    let evidence = call(json!({"action":"evidence","tab":tab}));
    assert!(
        evidence["console"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["text"] == "verified-click")
    );
    assert!(
        evidence["network"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == 200)
    );
    let screenshot = call(json!({"action":"screenshot","tab":tab}));
    let png = STANDARD
        .decode(screenshot["artifact"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    call(json!({"action":"close","tab":tab}));
    assert_eq!(call(json!({"action":"tabs"}))["tabs"], json!([]));
    call(json!({"action":"shutdown"}));
    let rejected = tool.call(
        &ctx,
        &json!({"action":"open","url":format!("http://localhost:{}/blocked",app.port)}),
    );
    assert!(
        rejected.is_error && rejected.text.contains("loopback"),
        "{}",
        rejected.text
    );
    let redirect = tool.call(
        &ctx,
        &json!({"action":"open","url":format!("http://127.0.0.1:{}/redirect",app.port)}),
    );
    assert!(
        redirect.is_error && redirect.text.contains("loopback"),
        "{}",
        redirect.text
    );
    call(json!({"action":"shutdown"}));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn builtin_runtime_end_to_end_sandboxed() {
    // Reads PATH and HOME (bun, runtime cache); gh fakes mutate both under this lock.
    let _env = crate::tools::vcs::gh_common::test_support::PATH_LOCK.lock();
    if std::process::Command::new("bun")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping built-in browser test: bun is not installed");
        return;
    }
    if !super::runtime::is_cached() && !super::runtime::network_reachable() {
        eprintln!(
            "skipping built-in browser test: runtime is not cached and the npm registry or Playwright CDN is unreachable"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let app = LocalApp::start();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut config = Config {
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.sandbox.mode = SandboxMode::Dev;
    config.sandbox.allow_network = true;
    config.browser.auto_localhost = false;
    // Isolate from cmux sessions on the host so this test always exercises Chromium.
    config.browser.cmux = false;
    config.browser.allowed_hosts = vec![format!("127.0.0.1:{}", app.port)];
    assert!(
        config.browser.playwright_module.trim().is_empty(),
        "the built-in runtime is the default"
    );
    let tool = tools(&config).pop().unwrap();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let call = |args: Value| {
        let output = tool.call(&ctx, &args);
        assert!(!output.is_error, "{}", output.text);
        serde_json::from_str::<Value>(&output.text).unwrap()
    };
    let opened = call(json!({"action":"open","url":format!("http://127.0.0.1:{}/first",app.port)}));
    let tab = opened["tab"].as_u64().unwrap();
    call(json!({"action":"type","tab":tab,"selector":"#name","text":"verified"}));
    call(json!({"action":"click","tab":tab,"selector":"#go"}));
    assert_eq!(
        call(
            json!({"action":"evaluate","tab":tab,"expression":"document.querySelector('#result').textContent"})
        )["value"],
        "verified"
    );
    call(
        json!({"action":"navigate","tab":tab,"url":format!("http://127.0.0.1:{}/article",app.port)}),
    );
    let article = call(json!({"action":"readable","tab":tab}));
    assert!(
        article["text"]
            .as_str()
            .unwrap()
            .contains("Local browser readable extraction"),
        "{article}"
    );
    let first = format!("http://127.0.0.1:{}/first", app.port);
    let article_url = format!("http://127.0.0.1:{}/article", app.port);
    assert_eq!(call(json!({"action":"back","tab":tab}))["url"], first);
    assert_eq!(
        call(json!({"action":"forward","tab":tab}))["url"],
        article_url
    );
    assert_eq!(call(json!({"action":"reload","tab":tab}))["status"], 200);
    let events = call(json!({"action":"events","tab":tab}));
    let all = events["events"].as_array().unwrap();
    assert!(
        all.iter()
            .any(|event| event["channel"] == "console" && event["text"] == "verified-click"),
        "{events}"
    );
    assert!(
        all.iter()
            .any(|event| event["channel"] == "navigation" && event["url"] == first),
        "{events}"
    );
    let last = events["last_seq"].as_u64().unwrap();
    let newer = call(json!({"action":"events","tab":tab,"after":last}));
    assert_eq!(newer["events"], json!([]), "{newer}");
    let screenshot = call(json!({"action":"screenshot","tab":tab}));
    let png = STANDARD
        .decode(screenshot["artifact"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    call(json!({"action":"close","tab":tab}));
    call(json!({"action":"shutdown"}));
}
