use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

type Handler = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// Loopback HTTP fixture: each connection gets one response, `handler(path_and_query)`.
fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, body) = handler(&target);
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn err_text<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn browsing_keeps_nuxt_without_details_and_follows_provider_pagination() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let base = serve(Arc::new(move |target| {
        counter.fetch_add(1, Ordering::SeqCst);
        if !target.starts_with("/servers?") {
            return (404, "{}".into());
        }
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        let params: BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(params.get("q").map(|v| v.as_ref()), Some("nuxt"));
        let second = params.get("page").is_some_and(|v| v == "2");
        (
            200,
            json!({
                "servers": if second {
                    json!([{"namespace":"nuxt", "slug":"docs", "displayName":"Nuxt docs"}])
                } else {
                    json!([
                        {"qualifiedName":"@nuxt/mcp", "displayName":"Nuxt", "verified":true},
                        {"qualifiedName":"nuxt/mcp", "displayName":"Duplicate"},
                        {"id":"not-an-install-identity"}
                    ])
                },
                "pagination": {"totalPages":2}
            })
            .to_string(),
        )
    }));
    let (first, more) = browse_page_at(&base, "nuxt", 1, None).expect("first page");
    assert!(more);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].name, "@nuxt/mcp");
    assert_eq!(first[0].badges, ["verified"]);
    assert!(!first[0].installable());
    let (second, more) = browse_page_at(&base, "nuxt", 2, None).expect("second page");
    assert!(!more);
    assert_eq!(second[0].name, "@nuxt/docs");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "no detail requests while browsing"
    );
}

#[test]
fn config_names_follow_slug_rules() {
    assert_eq!(to_config_name("@Acme/My Notes!"), "acme-my-notes");
    assert_eq!(to_config_name("@@"), "mcp-server");
    assert_eq!(normalize_qualified_name("acme/x"), "@acme/x");
    assert_eq!(unique_name("a", |n| n == "a"), "a-2");
}

#[test]
fn search_maps_entries_and_drops_unresolvable_ones() {
    let servers = json!({"servers": [
        {"id": "srv1", "namespace": "acme", "slug": "notes", "qualifiedName": "@acme/notes",
         "displayName": "Notes  Pro", "useCount": 5, "verified": true, "isDeployed": true},
        {"id": "srv2", "namespace": "acme", "slug": "ghost", "qualifiedName": "@acme/ghost",
         "displayName": "Notes Ghost", "useCount": 9}
    ]})
    .to_string();
    let details = json!({
        "qualifiedName": "@acme/notes",
        "displayName": "Notes",
        "description": "  Take\n notes  ",
        "connections": [{"type": "http", "deploymentUrl": "https://notes.example/mcp"}],
    })
    .to_string();
    let handler: Handler = Arc::new(move |target: &str| {
        if target.starts_with("/servers?") {
            (200, servers.clone())
        } else if target == "/servers/acme/notes" {
            (200, details.clone())
        } else {
            (404, "{}".to_owned())
        }
    });
    let base = serve(handler);
    let opts = SearchOptions {
        limit: 20,
        api_key: None,
        semantic: false,
    };
    let results = search_at(&base, "notes", &opts).expect("search");
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result.name, "acme/notes");
    assert_eq!(result.qualified_name, "@acme/notes");
    assert_eq!(result.display_name, "Notes");
    assert_eq!(result.description, "Take notes");
    assert!(result.inputs.is_empty());
    assert_eq!(
        result.config,
        json!({"type": "http", "url": "https://notes.example/mcp"})
    );
}

#[test]
fn search_reports_registry_status() {
    let handler: Handler = Arc::new(|_: &str| (401, "{}".to_owned()));
    let base = serve(handler);
    let opts = SearchOptions {
        limit: 1,
        api_key: None,
        semantic: false,
    };
    let error = search_at(&base, "x", &opts).expect_err("401 must fail");
    assert_eq!(error.status, 401);
    assert_eq!(error.message, "Smithery search failed with status 401");
}

#[test]
fn login_session_and_poll_loop() {
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polls);
    let handler: Handler = Arc::new(move |target: &str| {
        if target == "/api/auth/cli/session" {
            (
                200,
                json!({"sessionId": "s1", "authUrl": "https://smithery.ai/auth/s1"}).to_string(),
            )
        } else if target == "/api/auth/cli/poll/s1" {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (200, json!({"status": "pending"}).to_string())
            } else {
                (
                    200,
                    json!({"status": "success", "apiKey": "sk-test"}).to_string(),
                )
            }
        } else {
            (404, "{}".to_owned())
        }
    });
    let base = serve(handler);
    let session = create_auth_session(&base).expect("session");
    assert_eq!(session.session_id, "s1");
    assert_eq!(session.auth_url, "https://smithery.ai/auth/s1");
    let key = await_api_key(
        &base,
        &session.session_id,
        Duration::from_millis(5),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .expect("key");
    assert_eq!(key, "sk-test");
    assert_eq!(polls.load(Ordering::SeqCst), 2);
}

#[test]
fn login_expired_and_timed_out() {
    let expired: Handler = Arc::new(|_: &str| (410, "{}".to_owned()));
    let base = serve(expired);
    let error = await_api_key(
        &base,
        "s",
        Duration::from_millis(5),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .expect_err("expired");
    assert_eq!(
        error.to_string(),
        "Smithery login session expired. Please try again."
    );

    let pending: Handler = Arc::new(|_: &str| (200, json!({"status": "pending"}).to_string()));
    let base = serve(pending);
    let error = await_api_key(
        &base,
        "s",
        Duration::from_millis(5),
        Duration::from_millis(50),
        &AtomicBool::new(false),
    )
    .expect_err("deadline");
    assert_eq!(
        error.to_string(),
        "Smithery authorization timed out after 5 minutes."
    );
}

#[test]
fn key_validation_uses_registry_status() {
    let handler: Handler = Arc::new(|_: &str| (403, "{}".to_owned()));
    let base = serve(handler);
    let error = validate_key_at(&base, "bad").expect_err("403 must fail");
    assert!(
        error
            .to_string()
            .starts_with("Smithery API key validation failed:")
    );
}

#[test]
fn key_cache_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("path");
    assert!(read_key(&path).expect("read").is_none());
    save_key(&path, "  sk-abc \n").expect("save");
    assert_eq!(read_key(&path).expect("read").as_deref(), Some("sk-abc"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(clear_key(&path).expect("clear"));
    assert!(!clear_key(&path).expect("clear again"));
    assert_eq!(
        err_text(save_key(&path, "   ")),
        "Smithery API key cannot be empty."
    );
    assert!(key_file(Path::new("")).is_err());
}

#[test]
fn install_plan_types_values_and_reports_errors() {
    let entry = json!({"id": "x", "qualifiedName": "@acme/tool"});
    let details = json!({"connections": [{"type": "stdio", "configSchema": {
        "properties": {
            "apiKey": {"type": "string", "format": "password"},
            "limit": {"type": "number", "default": "5"},
            "mode": {"type": "string", "enum": ["fast", "slow"]}
        },
        "required": ["apiKey"]
    }}]});
    let result = to_search_result(&entry, &details).expect("result");

    let empty = BTreeMap::new();
    assert_eq!(
        err_text(plan_install(&result, &empty)),
        "Missing required value for \"apiKey\"."
    );

    let mut sets = BTreeMap::new();
    sets.insert("apiKey".to_owned(), "k".to_owned());
    sets.insert("mode".to_owned(), "medium".to_owned());
    assert_eq!(
        err_text(plan_install(&result, &sets)),
        "Invalid value for \"mode\". Use one of: fast, slow."
    );
    sets.insert("mode".to_owned(), "fast".to_owned());
    sets.insert("bogus".to_owned(), "1".to_owned());
    assert_eq!(
        err_text(plan_install(&result, &sets)),
        "Unknown Smithery input \"bogus\"."
    );
    sets.remove("bogus");
    let server = plan_install(&result, &sets).expect("plan");
    assert_eq!(server.program, PathBuf::from("bunx"));
    assert_eq!(server.args[5], r#"{"apiKey":"k","limit":5,"mode":"fast"}"#);
}

#[test]
fn argument_parsing_messages() {
    assert_eq!(
        err_text(parse_search("")),
        "Keyword required. Usage: /mcp smithery-search <keyword> [--scope project|user] [--limit <1-100>] [--semantic]"
    );
    assert_eq!(
        err_text(parse_search("x --limit 0")),
        "Invalid --limit value. Use an integer between 1 and 100."
    );
    assert_eq!(
        err_text(parse_search("x --limit")),
        "Missing value for --limit."
    );
    assert_eq!(
        err_text(parse_search("x --bogus")),
        "Unknown option: --bogus"
    );
    assert_eq!(
        err_text(parse_search("x --scope team")),
        "Invalid --scope value. Use project or user."
    );
    assert_eq!(
        parse_search("x --scope project").expect("parse").scope,
        crate::storage::settings_store::Scope::Project
    );
    let parsed = parse_search("foo bar --limit 3 --semantic").expect("parse");
    assert_eq!(parsed.keyword, "foo bar");
    assert_eq!(parsed.limit, 3);
    assert!(parsed.semantic);
    assert_eq!(
        parse_login("--api-key sk").expect("login"),
        Some("sk".to_owned())
    );
    assert_eq!(parse_login("").expect("login"), None);
}

struct ScriptedPrompt {
    answers: std::collections::VecDeque<Option<String>>,
    errors: Vec<String>,
}

impl KeyPrompt for ScriptedPrompt {
    fn ask(&mut self, _question: &str) -> Option<String> {
        self.answers.pop_front().expect("unexpected prompt")
    }
    fn error(&mut self, message: &str) {
        self.errors.push(message.to_owned());
    }
}

#[test]
fn pasted_key_is_validated_then_cached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    let mut ui = ScriptedPrompt {
        answers: std::collections::VecDeque::from([
            Some("  ".to_owned()),
            Some("bad".to_owned()),
            Some(" good ".to_owned()),
        ]),
        errors: Vec::new(),
    };
    let validate = |key: &str| -> Result<()> {
        if key == "good" {
            Ok(())
        } else {
            Err(anyhow!(
                "Smithery API key validation failed: Smithery search failed with status 401"
            ))
        }
    };
    assert!(paste_key_login(&path, &mut ui, validate).expect("login"));
    assert_eq!(
        ui.errors,
        vec![
            "Smithery API key cannot be empty.",
            "Smithery API key validation failed: Smithery search failed with status 401",
        ]
    );
    assert_eq!(read_key(&path).expect("read"), Some("good".to_owned()));
}

#[test]
fn cancelled_paste_caches_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    let mut ui = ScriptedPrompt {
        answers: std::collections::VecDeque::from([None]),
        errors: Vec::new(),
    };
    assert!(!paste_key_login(&path, &mut ui, |_| Ok(())).expect("login"));
    assert!(!path.exists());
}

#[test]
fn unauthorized_reauths_once_and_retries_with_the_new_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    save_key(&path, "stale").expect("save stale");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let base = serve(Arc::new(move |_: &str| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (401, "{}".to_owned())
        } else {
            (200, json!({"servers": []}).to_string())
        }
    }));
    let seen = std::cell::RefCell::new(Vec::new());
    let found = run_with_reauth(
        dir.path(),
        "required for smithery-search",
        true,
        |key| {
            seen.borrow_mut().push(key.map(str::to_owned));
            let opts = SearchOptions {
                limit: 1,
                api_key: key.map(str::to_owned),
                semantic: false,
            };
            search_at(&base, "x", &opts).map(|results| results.len())
        },
        |reason, may_reauth_again| {
            assert_eq!(reason, UNAUTHORIZED_REASON);
            assert!(!may_reauth_again);
            save_key(&path, "fresh")?;
            Ok(Reauth::Saved)
        },
    )
    .expect("retry succeeds");
    assert_eq!(found, Some(0));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        *seen.borrow(),
        vec![Some("stale".to_owned()), Some("fresh".to_owned())]
    );
}

#[test]
fn rate_limit_reauths_once_then_retries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    save_key(&path, "cached").expect("save");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let base = serve(Arc::new(move |_: &str| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (429, "{}".to_owned())
        } else {
            (200, json!({"servers": []}).to_string())
        }
    }));
    let reauths = AtomicUsize::new(0);
    let found = run_with_reauth(
        dir.path(),
        "required for smithery-search",
        true,
        |key| {
            let opts = SearchOptions {
                limit: 1,
                api_key: key.map(str::to_owned),
                semantic: false,
            };
            search_at(&base, "x", &opts).map(|results| results.len())
        },
        |reason, _| {
            reauths.fetch_add(1, Ordering::SeqCst);
            assert_eq!(reason, RATE_LIMIT_REASON);
            Ok(Reauth::Saved)
        },
    )
    .expect("retry after rate limit succeeds");
    assert_eq!(found, Some(0));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(reauths.load(Ordering::SeqCst), 1);
}

#[test]
fn rate_limit_surfaces_when_the_retry_is_declined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    save_key(&path, "cached").expect("save");
    let base = serve(Arc::new(|_: &str| (429, "{}".to_owned())));
    let attempts = AtomicUsize::new(0);
    let reauths = AtomicUsize::new(0);
    let error = run_with_reauth(
        dir.path(),
        "required for smithery-search",
        true,
        |key| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let opts = SearchOptions {
                limit: 1,
                api_key: key.map(str::to_owned),
                semantic: false,
            };
            search_at(&base, "x", &opts).map(|results| results.len())
        },
        |_, _| {
            reauths.fetch_add(1, Ordering::SeqCst);
            Ok(Reauth::Declined)
        },
    )
    .expect_err("429 must surface");
    assert_eq!(error.status, 429);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(reauths.load(Ordering::SeqCst), 1);
}

#[test]
fn resumed_retry_never_reauths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = key_file(dir.path()).expect("key file");
    save_key(&path, "stale").expect("save");
    let base = serve(Arc::new(|_: &str| (401, "{}".to_owned())));
    let reauths = AtomicUsize::new(0);
    let error = run_with_reauth(
        dir.path(),
        "required for smithery-search",
        false,
        |key| {
            let opts = SearchOptions {
                limit: 1,
                api_key: key.map(str::to_owned),
                semantic: false,
            };
            search_at(&base, "x", &opts).map(|results| results.len())
        },
        |_, _| {
            reauths.fetch_add(1, Ordering::SeqCst);
            Ok(Reauth::Saved)
        },
    )
    .expect_err("401 must surface on the resumed retry");
    assert_eq!(error.status, 401);
    assert_eq!(reauths.load(Ordering::SeqCst), 0);
}
