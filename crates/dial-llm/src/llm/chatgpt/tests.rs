use super::*;
use crate::llm::{catalog::model_context_window, model_config::ModelSelection};
use ring::signature::KeyPair;
use serde_json::json;
fn attempt() -> Attempt {
    Attempt {
        state: "pending-state".into(),
        verifier: SecretToken::new("test-verifier".into()),
        redirect: REDIRECT_URI.into(),
    }
}
fn signed(claims: &Value) -> (String, Value) {
    let rng = ring::rand::SystemRandom::new();
    let pkcs = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &rng,
    )
    .unwrap();
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        pkcs.as_ref(),
        &rng,
    )
    .unwrap();
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(json!({"alg":"ES256","kid":"test"}).to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = key.sign(&rng, input.as_bytes()).unwrap();
    let public = key.public_key().as_ref();
    (
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())),
        json!({"keys":[{"kid":"test","kty":"EC","crv":"P-256","x":URL_SAFE_NO_PAD.encode(&public[1..33]),"y":URL_SAFE_NO_PAD.encode(&public[33..65])}]}),
    )
}
fn login_fixture(
    dir: &Path,
    selected: Option<&str>,
    subject: &str,
) -> (Result<String>, Vec<String>, String) {
    use std::io::{BufRead, BufReader};
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let payload = Arc::new(parking_lot::Mutex::new((Value::Null, Value::Null)));
    let shared = Arc::clone(&payload);
    let job = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..1 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                head.push_str(&line);
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            requests.push(format!("{head}{}", String::from_utf8_lossy(&body)));
            let data = {
                let p = shared.lock();
                if index == 0 { p.0.clone() } else { p.1.clone() }
            }
            .to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",data.len()).unwrap();
        }
        requests
    });
    let mut seen_url = String::new();
    let outcome = login_at(
        dir,
        selected,
        Arc::new(AtomicBool::new(false)),
        &format!("{base}/token"),
        0,
        |url| {
            seen_url = url.to_owned();
            let fields = url
                .split_once('?')
                .unwrap()
                .1
                .split('&')
                .map(|field| {
                    let (key, value) = field.split_once('=').unwrap();
                    (decode(key).unwrap(), decode(value).unwrap())
                })
                .collect::<BTreeMap<_, _>>();
            let account_id = format!("workspace-{subject}");
            let (id_token, keys) = signed(
                &json!({"iss":ISSUER,"aud":CLIENT_ID,"exp":now()+3600,"sub":subject,"email":"fixture@example.com"}),
            );
            *payload.lock() = (
                json!({"access_token":test_access(&account_id),"refresh_token":"fixture-refresh","id_token":id_token,"token_type":"Bearer","expires_in":3600,"scope":SCOPE}),
                keys,
            );
            let redirect = &fields["redirect_uri"];
            let (host, path) = redirect
                .strip_prefix("http://")
                .unwrap()
                .split_once('/')
                .unwrap();
            let mut stream = TcpStream::connect(host).unwrap();
            write!(
                stream,
                "GET /{path}?{} HTTP/1.1\r\nHost: {host}\r\n\r\n",
                form(&[("state", &fields["state"]), ("code", "fixture-code")])
            )
            .unwrap();
            Ok(())
        },
    );
    (outcome, job.join().unwrap(), seen_url)
}
#[test]
fn catalog_merges_new_codex_models_and_preserves_account_capabilities() {
    let account = json!({"models":[{"slug":"existing","display_name":"Existing","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},{"slug":"internal","visibility":"hide"}]});
    let codex = json!({"models":[{"slug":"existing","visibility":"list","supported_reasoning_levels":[{"effort":"ultra"}]},
        {"slug":"new-sol","display_name":"New Sol","visibility":"list","input_modalities":["text","image"],"default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"max"},{"effort":"ultra"},{"effort":"future"}]},
        {"slug":"internal","visibility":"list"},{"slug":"hidden","visibility":"hide"}]});
    let catalog = build_catalog(&account, Some(&codex)).unwrap();
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(
        catalog.models[1].source,
        "Codex catalog (access unverified until inference)"
    );
    let existing = &catalog.profiles["existing"];
    assert_eq!(existing.capabilities.reasoning_efforts.len(), 2);
    let new = &catalog.profiles["new-sol"];
    assert_eq!(new.capabilities.vision, Some(true));
    assert_eq!(new.capabilities.reasoning_efforts.len(), 3);
    assert_eq!(
        new.capabilities.reasoning,
        super::super::profiles::ReasoningApi::OpenaiResponses
    );
}
#[test]
fn codex_model_discovery_sends_dial_account_headers_and_normalizes_catalog() {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
        200,
        &json!({
            "models":[{"slug":"gpt-codex-test","display_name":"Codex Test","visibility":"list","context_window":128000,"input_modalities":["text","image"]}]
        }),
    )]);
    let token = SecretToken::new(test_access("workspace-1"));
    let url = format!(
        "{}/codex/models?client_version={CLIENT_VERSION}",
        server.url
    );
    let mut response = codex_request("workspace-1", &token, &url, 5).unwrap();
    let models =
        parse_models(&limited_json(&mut response, MAX_CATALOG_JSON, "model catalog").unwrap())
            .unwrap();
    assert_eq!(models[0].selection.model, "gpt-codex-test");
    assert_eq!(models[0].metadata.vision, Some(true));
    let request = server.requests().remove(0);
    assert_eq!(
        crate::llm::fake::request_line(&request),
        format!("GET /codex/models?client_version={CLIENT_VERSION} HTTP/1.1")
    );
    assert_eq!(
        crate::llm::fake::header(&request, "chatgpt-account-id").as_deref(),
        Some("workspace-1")
    );
    assert_eq!(
        crate::llm::fake::header(&request, "OpenAI-Beta").as_deref(),
        Some("responses=experimental")
    );
    assert_eq!(
        crate::llm::fake::header(&request, "originator").as_deref(),
        Some("dial")
    );
}
#[test]
fn reasoning_metadata_is_separate_for_each_signed_in_account() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), false);
    test_catalog(
        dir.path(),
        &json!({"models":[{"slug":"account-model","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}),
    );
    assert!(
        model_profile(dir.path(), "account-model")
            .unwrap()
            .is_some()
    );
    let mut state = load(dir.path()).unwrap();
    let tokens = state.accounts.get_mut("oaiapp_test").unwrap().tokens.take();
    state.accounts.insert(
        "other".into(),
        Account {
            client_id: "other".into(),
            subject: "different".into(),
            email: String::new(),
            tokens,
        },
    );
    state.active = Some("other".into());
    save(dir.path(), &state).unwrap();
    assert!(
        model_profile(dir.path(), "account-model")
            .unwrap()
            .is_none()
    );
    let tokens = state.accounts.get_mut("other").unwrap().tokens.take();
    state.accounts.get_mut("oaiapp_test").unwrap().tokens = tokens;
    state.active = Some("oaiapp_test".into());
    save(dir.path(), &state).unwrap();
    assert!(
        model_profile(dir.path(), "account-model")
            .unwrap()
            .is_some()
    );
}
#[test]
fn browser_codex_oauth_activates_account_and_preserves_workspace_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let (result, requests, first_url) = login_fixture(dir.path(), None, "first-person");
    assert!(result.unwrap().contains("Codex sign-in"));
    assert!(first_url.contains(&format!("client_id={CLIENT_ID}")));
    assert!(first_url.contains("codex_cli_simplified_flow=true"));
    assert!(first_url.contains("originator=dial"));
    assert!(requests[0].contains(&format!("client_id={CLIENT_ID}")));
    assert!(requests[0].contains("code=fixture-code") && requests[0].contains("code_verifier="));
    assert!(requests[0].contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
    let account_id = "workspace-first-person";
    let state = load(dir.path()).unwrap();
    assert_eq!(state.active.as_deref(), Some(account_id));
    assert_eq!(
        account_access(dir.path(), Arc::new(AtomicBool::new(false)))
            .unwrap()
            .0,
        account_id
    );
    let (result, _, second_url) = login_fixture(dir.path(), Some(account_id), "first-person");
    assert!(result.is_ok());
    assert!(second_url.contains("id_token_hint=") && second_url.contains("login_hint="));
    let before = std::fs::read(dir.path().join(FILE)).unwrap();
    let (result, _, _) = login_fixture(dir.path(), Some(account_id), "different-person");
    assert!(result.is_err());
    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
}
#[test]
fn codex_authorization_url_matches_pkce_flow() {
    let url = attempt().url(None);
    for text in [
        "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
        "response_type=code",
        "code_challenge_method=S256",
        "state=pending-state",
        "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        "api.connectors.read",
        "api.connectors.invoke",
        "id_token_add_organizations=true",
        "codex_cli_simplified_flow=true",
        "originator=dial",
    ] {
        assert!(url.contains(text), "missing {text}: {url}");
    }
    assert!(
        !url.contains("resource=") && !url.contains("nonce=") && !url.contains("test-verifier")
    );
}
#[test]
fn codex_callback_validates_state_and_fixed_client() {
    let attempt = attempt();
    let valid = "/auth/callback?state=pending-state&code=valid-code";
    let (code, client_id) = attempt.callback(valid).unwrap();
    assert_eq!(code.as_str(), "valid-code");
    assert_eq!(client_id, CLIENT_ID);
    for target in [
        "/callback?state=pending-state&code=c",
        "/auth/callback?state=other&code=c",
        "/auth/callback?state=pending-state&code=c&client_id=other-client",
        "/auth/callback?state=pending-state&state=other&code=c",
        "/auth/callback?state=pending-state&error=access_denied&code=c",
    ] {
        assert!(attempt.callback(target).is_err());
    }
}
#[test]
fn identity_signature_issuer_audience_expiry_nonce_and_subject_are_verified() {
    let claims = json!({"iss":ISSUER,"aud":"oaiapp_test","exp":200,"nonce":"n","sub":"s","email":"person@example.com"});
    let (token, keys) = signed(&claims);
    assert_eq!(
        verify_id(&token, &keys, "oaiapp_test", Some("n"), Some("s"), 100).unwrap(),
        ("s".into(), "person@example.com".into())
    );
    for (aud, nonce, subject, time) in [
        ("wrong", "n", "s", 100),
        ("oaiapp_test", "wrong", "s", 100),
        ("oaiapp_test", "n", "wrong", 100),
        ("oaiapp_test", "n", "s", 200),
    ] {
        assert!(verify_id(&token, &keys, aud, Some(nonce), Some(subject), time).is_err());
    }
    let mut wrong = claims.clone();
    wrong["iss"] = json!("https://evil.example");
    let (token, keys) = signed(&wrong);
    assert!(verify_id(&token, &keys, "oaiapp_test", Some("n"), None, 100).is_err());
    let (token, keys) = signed(&claims);
    let mut parts = token.split('.').map(str::to_owned).collect::<Vec<_>>();
    parts[1] = URL_SAFE_NO_PAD.encode(
        json!({"iss":ISSUER,"aud":"oaiapp_test","exp":999,"nonce":"n","sub":"forged"}).to_string(),
    );
    assert!(verify_id(&parts.join("."), &keys, "oaiapp_test", Some("n"), None, 100).is_err());
    parts[0] = URL_SAFE_NO_PAD.encode(json!({"alg":"none","kid":"test"}).to_string());
    assert!(verify_id(&parts.join("."), &keys, "oaiapp_test", Some("n"), None, 100).is_err());
}
#[test]
fn plan_scope_and_complete_rotating_token_sets_are_required() {
    let mut response = json!({"access_token":"access","refresh_token":"refresh","id_token":"id","token_type":"Bearer","expires_in":3600,"scope":SCOPE});
    assert!(tokens(&response, None).is_ok());
    response["expires_in"] = json!(31_536_000);
    assert!(
        tokens(&response, None).is_ok(),
        "Codex lifetime is not capped at one day"
    );
    response["expires_in"] = json!(0);
    assert!(
        tokens(&response, None).is_err(),
        "zero-lifetime credentials are invalid"
    );
    response["expires_in"] = json!(3600);
    response["scope"] = json!("openid profile email");
    assert!(tokens(&response, None).is_err());
    response["scope"] = json!(SCOPE);
    response["refresh_token"] = json!(null);
    assert!(tokens(&response, None).is_err());
    response["refresh_token"] = json!("refresh");
    response["access_token"] = json!("with\nnewline");
    assert!(tokens(&response, None).is_err());
}
#[test]
fn resume_refresh_replaces_tokens_atomically_and_reuses_granted_scopes() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), true);
    let renewed = test_access("oaiapp_test");
    let server = super::super::fake::serve(vec![super::super::fake::json(
        200,
        &json!({"access_token":renewed.clone(),"refresh_token":"rotated","token_type":"Bearer","expires_in":3600}),
    )]);
    let url = server.url.clone();
    assert_eq!(
        access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &url)
            .unwrap()
            .as_str(),
        renewed
    );
    assert_eq!(
        access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &url)
            .unwrap()
            .as_str(),
        renewed,
        "unexpired token makes no second request"
    );
    let state = load(dir.path()).unwrap();
    let t = state.accounts["oaiapp_test"].tokens.as_ref().unwrap();
    assert_eq!(t.refresh_token, "rotated");
    assert_eq!(t.id_token, "test-id");
    assert_eq!(
        crate::platform::fs::mode(&std::fs::metadata(dir.path().join(FILE)).unwrap()) & 0o777,
        0o600
    );
    let requests = server.requests();
    let raw = String::from_utf8_lossy(&requests[0]);
    assert!(
        raw.contains("grant_type=refresh_token")
            && raw.contains(&format!("client_id={CLIENT_ID}"))
            && raw.contains("refresh_token=test-refresh")
    );
    assert!(!raw.contains("scope=") && !raw.contains("dynamic_agent_client"));
}
#[test]
fn concurrent_refreshes_send_a_rotating_token_only_once() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), true);
    let renewed = test_access("oaiapp_test");
    let server = super::super::fake::serve(vec![super::super::fake::json(
        200,
        &json!({"access_token":renewed.clone(),"refresh_token":"rotated","token_type":"Bearer","expires_in":3600}),
    )]);
    std::thread::scope(|scope| {
        let jobs = (0..4)
            .map(|_| {
                let url = &server.url;
                let dir = dir.path();
                scope.spawn(move || {
                    access_token_at(dir, Arc::new(AtomicBool::new(false)), url)
                        .unwrap()
                        .as_str()
                        .to_owned()
                })
            })
            .collect::<Vec<_>>();
        for job in jobs {
            assert_eq!(job.join().unwrap(), renewed);
        }
    });
    assert_eq!(server.requests().len(), 1);
}
#[test]
fn refresh_errors_preserve_credentials_and_do_not_echo_secret_responses() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), true);
    let before = std::fs::read(dir.path().join(FILE)).unwrap();
    let server = super::super::fake::serve(vec![super::super::fake::json(
        400,
        &json!({"error":"invalid_grant","detail":"test-refresh secret"}),
    )]);
    let error =
        access_token_at(dir.path(), Arc::new(AtomicBool::new(false)), &server.url).unwrap_err();
    assert!(!format!("{error:#}").contains("test-refresh"));
    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    server.requests();
}
#[test]
fn account_switching_is_isolated_and_corrupt_or_symlink_storage_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), false);
    {
        let _lock = locked(&dir.path().join(FILE)).unwrap();
        let mut state = load(dir.path()).unwrap();
        state.accounts.insert(
            "oaiapp_two".into(),
            Account {
                client_id: CLIENT_ID.into(),
                subject: "second".into(),
                email: "test@example.com".into(),
                tokens: None,
            },
        );
        save(dir.path(), &state).unwrap();
    }
    assert!(select_account(dir.path(), "oaiapp_two").is_err());
    assert!(accounts(dir.path()).unwrap()[0].active);
    assert!(
        sign_out(dir.path(), "oaiapp_two", Arc::new(AtomicBool::new(false)))
            .unwrap()
            .contains("Signed out")
    );
    let state = load(dir.path()).unwrap();
    assert_eq!(state.active.as_deref(), Some("oaiapp_test"));
    assert!(state.accounts["oaiapp_test"].tokens.is_some());
    #[cfg(unix)]
    {
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join(FILE), other.path().join(FILE)).unwrap();
        assert!(accounts(other.path()).is_err());
    }
    std::fs::write(dir.path().join(FILE), "invalid sensitive data").unwrap();
    let err = accounts(dir.path()).unwrap_err();
    assert!(!err.to_string().contains("sensitive"));
}
#[test]
fn catalog_prefers_valid_extended_maximum_over_default_context() {
    let models = parse_models(&json!({"models":[
        {"slug":"gpt-6.1-sol", "context_window":272000, "max_context_window":872000},
        {"slug":"api-size", "context_window":272000, "max_context_window":1050000},
        {"slug":"no-maximum", "context_window":272000},
        {"slug":"invalid-maximum", "context_window":64000, "max_context_window":-1},
        {"slug":"oversized-maximum", "context_window":64000, "max_context_window":4294967296u64},
        {"slug":"bad-default", "context_window":-1, "max_context_window":1000000},
        {"slug":"stale-low-maximum", "context_window":128000, "max_context_window":64000},
        {"slug":"no-valid-window", "context_window":0, "max_context_window":-1}
    ]}))
    .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|entry| entry.selection.context_window)
            .collect::<Vec<_>>(),
        vec![
            Some(872000),
            Some(1050000),
            Some(272000),
            Some(64000),
            Some(64000),
            Some(1000000),
            Some(128000),
            None
        ]
    );
    let home = tempfile::tempdir().unwrap();
    test_account(home.path(), false);
    test_catalog(
        home.path(),
        &json!({"models":[{"slug":"gpt-6.1-sol", "context_window":272000, "max_context_window":872000}]}),
    );
    assert_eq!(
        model_context_window(
            home.path(),
            &ModelSelection {
                provider: PROVIDER.into(),
                model: "gpt-6.1-sol".into(),
                ..Default::default()
            }
        ),
        Some(872000)
    );
    let project = tempfile::tempdir().unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(project.path(), home.path().to_owned())
            .unwrap();
    let auto = crate::config::types::Config::from_table(
        &layout,
        toml::from_str("provider='chatgpt'\nmodel='gpt-6.1-sol'\ncontext_window='auto'\n").unwrap(),
    )
    .unwrap();
    assert_eq!(auto.context_window, 872000);
    let capped = crate::config::types::Config::from_table(
        &layout,
        toml::from_str("provider='chatgpt'\nmodel='gpt-6.1-sol'\ncloud_context_window=64000\n")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(capped.context_window, 64000);
}

#[test]
fn account_model_catalog_uses_visibility_slug_and_display_name() {
    let rows=parse_models(&json!({"models":[{"slug":"available-model","display_name":"Friendly","visibility":"list","context_window":131072},{"slug":"hidden","visibility":"hidden"}]})).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].selection.provider, PROVIDER);
    assert_eq!(rows[0].selection.model, "available-model");
    assert_eq!(rows[0].label, "Friendly");
    assert_eq!(rows[0].selection.context_window, Some(131072));
    assert_eq!(
        parse_models(&json!({"data":[{"id":"api-key-model"}]})).unwrap()[0]
            .selection
            .model,
        "api-key-model"
    );
}
#[test]
fn large_model_catalogs_load_without_raising_the_oauth_response_limit() {
    use super::super::fake;
    let catalog = json!({"models":[{"slug":"account-model","display_name":"Account Model","visibility":"list","context_window":131072,"base_instructions":"x".repeat(MAX_JSON as usize+1024)}]});
    let server = fake::serve(vec![fake::json(200, &catalog)]);
    let models = fetch_models(&server.url, "fixture-token").unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].selection.model, "account-model");
    let requests = server.requests();
    assert_eq!(
        fake::header(&requests[0], "authorization").as_deref(),
        Some("Bearer fixture-token")
    );
    let server = fake::serve(vec![fake::json(200, &catalog)]);
    let error = get_json(&server.url, Arc::new(AtomicBool::new(false))).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("authorization response exceeds limits")
    );
    server.requests();
    let oversized = json!({"models":[],"instructions":"x".repeat(MAX_CATALOG_JSON as usize)});
    let server = fake::serve(vec![fake::json(200, &oversized)]);
    assert!(
        fetch_models(&server.url, "fixture-token")
            .unwrap_err()
            .to_string()
            .contains("model catalog response exceeds limits")
    );
    server.requests();
}
#[test]
fn unrelated_and_invalid_callbacks_do_not_consume_the_browser_attempt() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let job = std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
    for target in [
        "/favicon.ico",
        "/auth/callback?state=other&code=fixture-code",
        "/auth/callback?state=pending-state&state=other&code=fixture-code",
        "/auth/callback?state=pending-state&code=fixture-code&client_id=other-client",
    ] {
        let mut browser = TcpStream::connect(addr).unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            browser,
            "GET {target} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        browser.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 400"));
    }
    let mut browser = TcpStream::connect(addr).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(browser,"GET /auth/callback?state=pending-state&code=fixture-code HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\nAccept: text/html\r\nCookie: {}\r\n\r\n","x".repeat(8192)).unwrap();
    let mut reply = String::new();
    browser.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("200 OK"));
    assert_eq!(job.join().unwrap().unwrap().1, CLIENT_ID);
}
#[test]
fn matching_denied_authorization_ends_the_attempt() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let job = std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
    let mut browser = TcpStream::connect(addr).unwrap();
    browser
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(browser,"GET /auth/callback?state=pending-state&error=access_denied HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut reply = String::new();
    browser.read_to_string(&mut reply).unwrap();
    assert!(
        job.join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("declined")
    );
    assert!(reply.contains("400 Bad Request"));
}
#[test]
fn idle_browser_preconnection_does_not_end_login() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let job = std::thread::spawn(move || receive(&listener, &attempt(), &AtomicBool::new(false)));
    let idle = TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(650));
    drop(idle);
    let result = (|| -> Result<String> {
        let mut browser = TcpStream::connect(addr)?;
        browser.set_read_timeout(Some(Duration::from_secs(3)))?;
        write!(
            browser,
            "GET /auth/callback?state=pending-state&code=fixture-code HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: fixture-browser\r\nAccept: text/html\r\n\r\n"
        )?;
        let mut reply = String::new();
        browser.read_to_string(&mut reply)?;
        Ok(reply)
    })();
    let received = job.join().unwrap();
    assert!(
        received.is_ok(),
        "an idle browser connection must not stop the callback listener"
    );
    assert!(result.unwrap().contains("200 OK"));
}
#[test]
fn waiting_for_the_browser_is_cancellable() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let _idle = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = Arc::clone(&cancel);
    let job = std::thread::spawn(move || receive(&listener, &attempt(), &worker));
    std::thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    cancel.store(true, Ordering::SeqCst);
    assert!(
        job.join()
            .unwrap()
            .unwrap_err()
            .downcast_ref::<super::super::Interrupted>()
            .is_some()
    );
    assert!(start.elapsed() < Duration::from_millis(200));
}

#[test]
fn stored_chatgpt_file_keeps_its_shape_through_the_shared_store() {
    let dir = tempfile::tempdir().unwrap();
    test_account(dir.path(), false);
    let keys = |path: &Path| -> Vec<String> {
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value.as_object().unwrap().keys().cloned().collect()
    };
    let file = dir.path().join(FILE);
    assert_eq!(keys(&file), vec!["accounts", "active"]);
    let listed = accounts(dir.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].active);
    select_account(dir.path(), "oaiapp_test").unwrap();
    assert_eq!(keys(&file), vec!["accounts", "active"]);
    assert!(accounts(dir.path()).unwrap()[0].active);
}
