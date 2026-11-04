use super::*;
use std::io::{BufRead, BufReader, Read};
use std::sync::{atomic::Ordering, mpsc};
use std::time::Instant;

#[test]
fn slow_refresh_obeys_request_cancellation_and_releases_its_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let (entered, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        entered.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
    });
    let source = Arc::new(TokenSource::empty(PathBuf::new()));
    *lock(&source.grant) = Some(Grant {
        access: "expired".into(),
        refresh: "refresh".into(),
        token_endpoint: endpoint,
        ..Grant::default()
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_source = source.clone();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || worker_source.bearer_with(&worker_cancel));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let began = Instant::now();
    cancel.store(true, Ordering::SeqCst);
    assert!(worker.join().unwrap().is_none());
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(source.stored_access().as_deref(), Some("expired"));
    server.join().unwrap();
}

#[test]
fn waiting_for_another_refresh_is_cancellable_without_spending_the_token() {
    let source = Arc::new(TokenSource::empty(PathBuf::new()));
    let turn = source.refreshing.lock();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_source = source.clone();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || worker_source.refresh_with("stale", &worker_cancel));
    cancel.store(true, Ordering::SeqCst);
    assert!(!worker.join().unwrap());
    drop(turn);
}

#[test]
fn only_exact_validated_resource_bindings_can_load_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let resource = "https://original.example/mcp?tenant=one";
    let cancel = Arc::new(AtomicBool::new(false));
    for expires_at_ms in [0, util::NEVER_EXPIRES] {
        for binding in [
            "",
            "not a URL",
            "http://192.168.1.2/mcp",
            "https://other.example/mcp?tenant=one",
            "https://original.example/other?tenant=one",
            "https://original.example/mcp?tenant=two",
            "https://original.example/mcp",
            resource,
        ] {
            persist(
                &grant_path(dir.path(), "server"),
                &Grant {
                    access: "private-access".into(),
                    refresh: "private-refresh".into(),
                    expires_at_ms,
                    resource: binding.into(),
                    token_endpoint: "https://original.example/token".into(),
                    ..Grant::default()
                },
            )
            .unwrap();
            let loaded = TokenSource::load(dir.path(), "server", resource);
            if binding == resource {
                let source = loaded.unwrap();
                assert_eq!(source.stored_access().as_deref(), Some("private-access"));
                if expires_at_ms == util::NEVER_EXPIRES {
                    assert_eq!(
                        source.bearer_with(&cancel).as_deref(),
                        Some("private-access")
                    );
                }
            } else {
                let error = loaded
                    .err()
                    .expect("mismatched or legacy grants must not load")
                    .to_string();
                assert!(error.contains("sign in again"), "{error}");
                assert!(
                    !error.contains("private-access") && !error.contains("private-refresh"),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn credential_endpoints_use_the_mcp_https_and_loopback_policy() {
    for endpoint in [
        "https://auth.example/authorize",
        "https://192.168.1.2/token",
        "http://localhost/register",
        "http://127.0.0.1:1234/token",
        "http://[::1]:1234/authorize",
    ] {
        assert!(validate_endpoint(endpoint).is_ok(), "{endpoint}");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    for endpoint in [
        "http://192.168.1.2/token",
        "http://10.0.0.1/authorize",
        "http://172.16.0.1/register",
        "http://server.local/token",
        "http://metadata.google.internal/token",
        "http://localhost.evil.example/token",
        "http://[fe80::1]/token",
        "http://auth.example/token?secret=private-value",
    ] {
        let error = validate_endpoint(endpoint).unwrap_err().to_string();
        assert!(!error.contains("private-value"), "{error}");
        assert!(get_json(endpoint, &cancel).is_err(), "{endpoint}");
        assert!(
            authorization_metadata(endpoint, &cancel).is_err(),
            "{endpoint}"
        );
        assert!(
            register(
                Some(&json!(endpoint)),
                "http://127.0.0.1/callback",
                "server",
                &cancel
            )
            .is_err(),
            "{endpoint}"
        );
        let source = TokenSource::empty(PathBuf::new());
        *lock(&source.grant) = Some(Grant {
            access: "private-access".into(),
            refresh: "private-refresh".into(),
            token_endpoint: endpoint.into(),
            ..Grant::default()
        });
        assert!(
            !source.refresh_with("private-access", &cancel),
            "{endpoint}"
        );
        assert_eq!(source.stored_access().as_deref(), Some("private-access"));
    }
}

#[test]
fn matching_binding_keeps_stored_cleanup_headers() {
    let dir = tempfile::tempdir().unwrap();
    let resource = "https://original.example/mcp";
    persist(
        &grant_path(dir.path(), "server"),
        &Grant {
            access: "private-access".into(),
            expires_at_ms: util::NEVER_EXPIRES,
            resource: resource.into(),
            ..Grant::default()
        },
    )
    .unwrap();
    let source = Arc::new(TokenSource::load(dir.path(), "server", resource).unwrap());
    let config = McpServerConfig {
        url: Some(resource.into()),
        ..McpServerConfig::default()
    };
    let auth = super::super::http::Auth::new("server", &config, Some(source)).unwrap();
    assert!(
        auth.headers_stored()
            .iter()
            .any(|(key, value)| key == "Authorization" && value == "Bearer private-access")
    );
}

#[test]
fn fixed_credentials_connect_despite_a_mismatched_oauth_grant() {
    if !crate::tools::testkit::in_env_subprocess(
        "tools::integrations::mcp::oauth::tests::fixed_credentials_connect_despite_a_mismatched_oauth_grant",
        &[("DIAL_TEST_MCP_FIXED_TOKEN", "fixed-token")],
    ) {
        return;
    }
    let user = tempfile::tempdir().unwrap();
    for use_env in [false, true] {
        let (url, seen) = super::super::tests::http_server_with("fixed-token", "");
        persist(
            &grant_path(user.path(), "server"),
            &Grant {
                access: "old-oauth-token".into(),
                refresh: "old-oauth-refresh".into(),
                expires_at_ms: util::NEVER_EXPIRES,
                resource: "https://previous.example/mcp".into(),
                ..Grant::default()
            },
        )
        .unwrap();
        let mut config = McpServerConfig {
            url: Some(url),
            ..McpServerConfig::default()
        };
        if use_env {
            config.bearer_env = Some("DIAL_TEST_MCP_FIXED_TOKEN".into());
        } else {
            config
                .headers
                .insert("aUtHoRiZaTiOn".into(), "Bearer fixed-token".into());
        }
        let (client, _) = super::super::open(
            "server",
            &config,
            user.path(),
            user.path(),
            Some(Instant::now() + Duration::from_secs(3)),
            &|| false,
        )
        .unwrap();
        let log = seen.lock().unwrap().clone();
        assert!(
            log.iter().any(|line| line.starts_with("initialize|")),
            "{log:?}"
        );
        assert!(
            log.iter().all(|line| line.contains("|Bearer fixed-token|")),
            "{log:?}"
        );
        assert!(
            log.iter().all(|line| !line.contains("old-oauth")),
            "{log:?}"
        );
        drop(client);
    }
}
