use super::*;

#[test]
fn catalog_lookup_and_choices_agree() {
    assert_eq!(find("nomic-embed-text").map(|m| m.dims), Some(768));
    assert!(find("missing-model").is_none());
    let mut names: Vec<&str> = CATALOG.iter().map(|m| m.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), CATALOG.len(), "catalog names must be unique");
    let choices: Vec<&str> = CHOICES.iter().map(|(name, _)| *name).collect();
    let catalog: Vec<&str> = CATALOG.iter().map(|m| m.name).collect();
    assert_eq!(choices, catalog);
    assert!(CATALOG.iter().all(|m| validate_name(m.name).is_ok()));
}

#[test]
fn setup_ids_round_trip_and_reject_bad_names() {
    assert_eq!(model_from_id("embeddings"), Some(DEFAULT_MODEL));
    assert_eq!(model_from_id("embeddings:bge-m3"), Some("bge-m3"));
    assert_eq!(setup_id("bge-m3"), "embeddings:bge-m3");
    assert_eq!(model_from_id("embeddings:"), None);
    assert_eq!(model_from_id("embeddings:a b"), None);
    assert_eq!(model_from_id("bun"), None);
    assert_eq!(model_from_id("embeddingsx:bge"), None);
}

#[test]
fn name_validation_rejects_flags_spaces_and_empties() {
    assert!(validate_name("library/model:0.6b").is_ok());
    assert!(validate_name("").is_err());
    assert!(validate_name("-x").is_err());
    assert!(validate_name("a b").is_err());
    assert!(validate_name("a;rm").is_err());
    assert!(validate_name(&"a".repeat(129)).is_err());
}

#[test]
fn pull_lines_parse_into_status_progress_success_and_errors() {
    assert_eq!(parse_pull_line("  ").unwrap(), None);
    assert_eq!(
        parse_pull_line(r#"{"status":"pulling manifest"}"#).unwrap(),
        Some(PullUpdate::Status("pulling manifest".into()))
    );
    assert_eq!(
        parse_pull_line(
            r#"{"status":"pulling abc","digest":"sha256:abc","total":200,"completed":50}"#
        )
        .unwrap(),
        Some(PullUpdate::Progress {
            status: "pulling abc".into(),
            completed: 50,
            total: 200
        })
    );
    assert_eq!(
        parse_pull_line(r#"{"status":"success"}"#).unwrap(),
        Some(PullUpdate::Success)
    );
    let error =
        parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#).unwrap_err();
    assert!(error.to_string().contains("file does not exist"));
    assert!(parse_pull_line("<html>").is_err());
}

#[test]
fn progress_is_described_and_throttled_to_five_point_steps() {
    let progress = |completed| PullUpdate::Progress {
        status: "pulling abc".into(),
        completed,
        total: 200_000_000,
    };
    assert_eq!(
        describe(&progress(50_000_000)),
        "pulling abc: 25% (50 of 200 MB)"
    );
    let mut last = None;
    assert!(should_report(&progress(0), &mut last));
    assert!(!should_report(&progress(4_000_000), &mut last));
    assert!(should_report(&progress(10_000_000), &mut last));
    assert!(should_report(
        &PullUpdate::Status("verifying".into()),
        &mut last
    ));
    assert!(should_report(&progress(10_000_000), &mut last));
}

#[test]
fn installed_tags_match_with_or_without_latest() {
    let names = parse_tags(
        r#"{"models":[{"name":"nomic-embed-text:latest","model":"nomic-embed-text:latest"},{"name":"bge-m3:v1"}]}"#,
    )
    .unwrap();
    assert!(has_model(&names, "nomic-embed-text"));
    assert!(has_model(&names, "nomic-embed-text:latest"));
    assert!(!has_model(&names, "all-minilm"));
    assert!(has_model(&names, "bge-m3:v1"));
    assert!(!has_model(&names, "bge-m3"));
    assert!(parse_tags("nope").is_err());
}

fn sets(changes: &[Change]) -> Vec<(&str, Option<&str>)> {
    changes
        .iter()
        .filter_map(|change| match change {
            Change::Set { path, value } => Some((path.as_str(), value.as_str())),
            Change::Remove { .. } => None,
        })
        .collect()
}

#[test]
fn wiring_points_the_role_at_ollama_and_turns_off_into_on() {
    let config = Config::default();
    let changes = wiring(&config, "bge-m3", EmbeddingMode::Off).unwrap();
    let off = sets(&changes);
    assert!(off.contains(&("model_roles.embedding.provider", Some("ollama"))));
    assert!(off.contains(&("model_roles.embedding.model", Some("bge-m3"))));
    assert!(off.contains(&("memory.semantic.embedding", Some("on"))));
    for mode in [EmbeddingMode::Auto, EmbeddingMode::On] {
        let changes = wiring(&config, "bge-m3", mode).unwrap();
        assert!(
            !changes.iter().any(
                |c| matches!(c, Change::Set { path, .. } if path == "memory.semantic.embedding")
            )
        );
    }
    assert!(wiring(&config, "", EmbeddingMode::Off).is_err());
}

#[test]
fn switching_a_remote_embedding_role_to_ollama_clears_its_stale_route() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let headers: toml::Value = toml::from_str::<toml::Table>(
        "rows = [{ name = \"X-Remote-Feature\", value = { literal = \"custom\" } }]",
    )
    .unwrap()["rows"]
        .clone();
    let set = |path: &str, value: toml::Value| Change::Set {
        path: path.into(),
        value,
    };
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("model_roles.embedding.provider", "openai".into()),
            set(
                "model_roles.embedding.model",
                "text-embedding-3-small".into(),
            ),
            set(
                "model_roles.embedding.base_url",
                "https://remote.example/v1".into(),
            ),
            set(
                "model_roles.embedding.api_key_env",
                "REMOTE_EMBED_KEY".into(),
            ),
            set("model_roles.embedding.allow_insecure", true.into()),
            set("model_roles.embedding.protocol", "chat-completions".into()),
            set("model_roles.embedding.headers", headers),
            set("memory.semantic.embedding", "auto".into()),
            set(
                "memory.semantic.embedding_base_url",
                "http://127.0.0.1:9/v1".into(),
            ),
            set("memory.semantic.embedding_model", "override-model".into()),
        ],
    )
    .unwrap();
    let before = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    assert!(
        before
            .role_selection("embedding")
            .unwrap()
            .headers
            .is_some()
    );
    let changes = wiring(&before, "bge-m3", before.memory.semantic.embedding).unwrap();
    settings_store::save_model_config(&layout, Scope::Global, &changes, "embedding").unwrap();

    let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    let role = saved.role_selection("embedding").expect("role assigned");
    assert_eq!(role.provider, "ollama");
    assert_eq!(role.model, "bge-m3");
    assert_eq!(role.base_url.as_deref(), Some("http://127.0.0.1:11434"));
    assert!(role.api_key_env.as_deref().unwrap_or_default().is_empty());
    assert!(!role.allow_insecure);
    assert!(matches!(
        role.protocol,
        Some(crate::llm::model_config::ApiProtocol::Ollama)
    ));
    assert!(
        role.headers.is_none(),
        "headers inherit the Ollama provider"
    );
    assert!(role.account_id.as_deref().unwrap_or_default().is_empty());
    let endpoint = saved
        .for_role("embedding")
        .endpoint
        .expect("effective Ollama route");
    assert_eq!(endpoint.base_url, "http://127.0.0.1:11434");
    assert!(endpoint.api_key_env.unwrap_or_default().is_empty());
    assert!(matches!(
        endpoint.protocol,
        Some(crate::llm::model_config::ApiProtocol::Ollama)
    ));
    // Mode and explicit overrides are untouched: auto stays auto, overrides still win.
    assert_eq!(saved.memory.semantic.embedding, EmbeddingMode::Auto);
    assert_eq!(
        saved.memory.semantic.embedding_base_url,
        "http://127.0.0.1:9/v1"
    );
    assert_eq!(saved.memory.semantic.embedding_model, "override-model");
}

#[test]
fn configured_counts_a_role_or_a_complete_override_only() {
    let mut config = Config::default();
    assert!(!configured(&config));
    config.memory.semantic.embedding_base_url = "http://127.0.0.1:1/v1".into();
    assert!(!configured(&config));
    config.memory.semantic.embedding_model = "m".into();
    assert!(configured(&config));
    let mut role = Config::default();
    let selection: crate::llm::model_config::ModelSelection =
        toml::from_str("provider = \"ollama\"\nmodel = \"nomic-embed-text\"").unwrap();
    role.model_roles.insert("embedding".into(), selection);
    assert!(configured(&role));
}

#[test]
fn offer_appears_only_while_no_embedding_model_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    let offer = setting_offer(dir.path(), &config).expect("unconfigured offers the default");
    assert_eq!(offer.path, EMBEDDING_MODEL_PATH);
    assert_eq!(offer.id, "embeddings:nomic-embed-text");
    assert!(
        offer
            .preview
            .contains("Confirm with: /setup embeddings:nomic-embed-text yes")
    );
    let mut configured_config = Config::default();
    configured_config.memory.semantic.embedding_base_url = "http://127.0.0.1:1/v1".into();
    configured_config.memory.semantic.embedding_model = "m".into();
    assert!(setting_offer(dir.path(), &configured_config).is_none());
}

#[test]
fn summary_names_a_shadowing_project_role_and_the_backend_switch() {
    let before = Config::default();
    let mut saved = Config::default();
    let shadow: crate::llm::model_config::ModelSelection =
        toml::from_str("provider = \"openai\"\nmodel = \"text-embedding-3-small\"").unwrap();
    saved.model_roles.insert("embedding".into(), shadow);
    let text = summary("nomic-embed-text", &before, &saved);
    assert!(text.starts_with(SUMMARY_PREFIX), "{text}");
    assert!(
        text.contains("This project assigns its own embedding role"),
        "{text}"
    );
    assert!(text.contains("/setup memory semantic"), "{text}");
    let own: crate::llm::model_config::ModelSelection =
        toml::from_str("provider = \"ollama\"\nmodel = \"nomic-embed-text\"").unwrap();
    saved.model_roles.insert("embedding".into(), own);
    saved.memory.backend = MemoryBackendId::Semantic;
    let text = summary("nomic-embed-text", &before, &saved);
    assert!(!text.contains("This project"), "{text}");
    assert!(!text.contains("semantic memory backend"), "{text}");
}

#[test]
fn every_curated_model_has_its_own_offer() {
    let dir = tempfile::tempdir().unwrap();
    let offers = catalog_offers(dir.path());
    assert_eq!(offers.len(), CATALOG.len());
    for (offer, model) in offers.iter().zip(CATALOG) {
        assert_eq!(offer.id, setup_id(model.name));
        assert!(offer.preview.contains(model.name));
    }
}

#[test]
fn pull_reports_progress_and_stops_on_cancel() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if std::io::Read::read(&mut stream, &mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let length: usize = String::from_utf8_lossy(&request)
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
                .unwrap_or(0);
            let mut request_body = vec![0; length];
            std::io::Read::read_exact(&mut stream, &mut request_body).unwrap();
            let body = concat!(
                "{\"status\":\"pulling manifest\"}\n",
                "{\"status\":\"pulling abc\",\"total\":100,\"completed\":50}\n",
                "{\"status\":\"success\"}\n"
            );
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    pull(
        &Server::bare(&base),
        "nomic-embed-text",
        &cancel,
        &mut |line| seen.push(line.to_owned()),
    )
    .unwrap();
    assert!(seen.iter().any(|line| line == "pulling manifest"));
    assert!(seen.iter().any(|line| line.starts_with("pulling abc: 50%")));
    let cancelled = AtomicBool::new(true);
    let error = pull(
        &Server::bare(&base),
        "nomic-embed-text",
        &cancelled,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    server.join().unwrap();
}

#[test]
fn native_paths_drop_an_openai_v1_suffix() {
    assert_eq!(
        Server::bare("http://gpu:11434/v1/").api("/api/pull"),
        "http://gpu:11434/api/pull"
    );
    assert_eq!(
        Server::bare("http://127.0.0.1:11434").api("/api/tags"),
        "http://127.0.0.1:11434/api/tags"
    );
}

fn ollama_config(layout: &Layout, base: &str, scope: Scope, mode: &str) {
    let headers: toml::Value = toml::from_str::<toml::Table>(
        "rows = [{ name = \"X-Gateway\", value = { literal = \"edge\" } }]",
    )
    .unwrap()["rows"]
        .clone();
    let mut changes = vec![Change::Set {
        path: "memory.semantic.embedding".into(),
        value: mode.into(),
    }];
    if scope == Scope::Global {
        changes.push(Change::Set {
            path: "providers.ollama.base_url".into(),
            value: base.into(),
        });
        changes.push(Change::Set {
            path: "providers.ollama.headers".into(),
            value: headers,
        });
    }
    settings_store::save_config(layout, scope, &changes).unwrap();
}

#[test]
fn setup_pulls_from_and_wires_the_configured_ollama_provider() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let fake = crate::llm::fake::serve(vec![
        crate::llm::fake::json(200, &json!({"version": "test"})),
        crate::llm::fake::json(200, &json!({"models": []})),
        crate::llm::fake::Script::Http {
            status: 200,
            content_type: "application/x-ndjson",
            body: b"{\"status\":\"success\"}\n".to_vec(),
        },
    ]);
    let url = fake.url.clone();
    ollama_config(&layout, &url, Scope::Global, "off");
    let mut lines = Vec::new();
    let summary = setup(&layout, "bge-m3", &AtomicBool::new(false), &mut |line| {
        lines.push(line.to_owned())
    })
    .unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 3, "version, tags, pull");
    for raw in &requests {
        assert_eq!(
            crate::llm::fake::header(raw, "x-gateway").as_deref(),
            Some("edge"),
            "configured provider headers reach every setup request"
        );
    }
    assert!(crate::llm::fake::request_line(&requests[2]).starts_with("POST /api/pull "));
    assert_eq!(crate::llm::fake::body(&requests[2])["model"], "bge-m3");
    let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    let role = saved.role_selection("embedding").expect("role assigned");
    assert_eq!(role.provider, "ollama");
    assert_eq!(role.base_url.as_deref(), Some(url.as_str()));
    assert_eq!(saved.memory.semantic.embedding, EmbeddingMode::On);
    let endpoint = saved
        .for_role("embedding")
        .endpoint
        .expect("effective route");
    assert_eq!(endpoint.base_url, url);
    assert!(summary.contains("now on"), "{summary}");
}

#[test]
fn setup_reports_a_project_mode_that_still_overrides_the_global_change() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let fake = crate::llm::fake::serve(vec![
        crate::llm::fake::json(200, &json!({"version": "test"})),
        crate::llm::fake::json(200, &json!({"models": [{"name": "bge-m3:latest"}]})),
    ]);
    let url = fake.url.clone();
    ollama_config(&layout, &url, Scope::Global, "auto");
    ollama_config(&layout, &url, Scope::Project, "off");
    let summary = setup(&layout, "bge-m3", &AtomicBool::new(false), &mut |_| {}).unwrap();
    fake.requests();
    assert!(summary.contains("still off"), "{summary}");
    let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    assert_eq!(saved.memory.semantic.embedding, EmbeddingMode::Off);
    assert!(saved.role_selection("embedding").is_some());
}

#[test]
fn a_configured_remote_ollama_that_is_down_is_not_started_or_installed_locally() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    ollama_config(
        &layout,
        &format!("http://127.0.0.1:{port}"),
        Scope::Global,
        "auto",
    );
    let error = setup(&layout, "bge-m3", &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    assert!(error.to_string().contains("is not reachable"), "{error:#}");
    let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    assert!(
        saved.role_selection("embedding").is_none(),
        "nothing saved on failure"
    );
}

#[test]
fn configured_models_list_the_configured_endpoint_with_its_headers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let fake = crate::llm::fake::serve(vec![crate::llm::fake::json(
        200,
        &json!({"models": [{"name": "bge-m3:latest"}]}),
    )]);
    ollama_config(&layout, &fake.url, Scope::Global, "auto");
    let names = configured_models(&layout).unwrap();
    let requests = fake.requests();
    assert!(has_model(&names, "bge-m3"), "{names:?}");
    assert!(crate::llm::fake::request_line(&requests[0]).starts_with("GET /api/tags "));
    assert_eq!(
        crate::llm::fake::header(&requests[0], "x-gateway").as_deref(),
        Some("edge")
    );
}

#[test]
fn pull_errors_never_echo_the_bearer_key() {
    let fake = crate::llm::fake::serve(vec![crate::llm::fake::json(
        401,
        &json!({"error": "bad key sk-edge-secret-42"}),
    )]);
    let server = Server {
        base_url: fake.url.clone(),
        headers: Vec::new(),
        token: Some("sk-edge-secret-42".into()),
    };
    let error = pull(&server, "bge-m3", &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    let requests = fake.requests();
    assert_eq!(
        crate::llm::fake::header(&requests[0], "authorization").as_deref(),
        Some("Bearer sk-edge-secret-42")
    );
    let text = format!("{error:#}");
    assert!(!text.contains("sk-edge-secret-42"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

#[test]
fn pull_progress_never_echoes_the_bearer_key() {
    let fake = crate::llm::fake::serve(vec![crate::llm::fake::Script::Http {
        status: 200,
        content_type: "application/x-ndjson",
        body: b"{\"status\":\"downloading for sk-fixture-secret\"}\n{\"status\":\"success\"}\n"
            .to_vec(),
    }]);
    let server = Server {
        base_url: fake.url.clone(),
        headers: Vec::new(),
        token: Some("sk-fixture-secret".into()),
    };
    let mut progress = Vec::new();
    pull(&server, "bge-m3", &AtomicBool::new(false), &mut |line| {
        progress.push(line.to_owned())
    })
    .unwrap();
    assert!(progress.iter().any(|line| line.contains("[redacted]")));
    assert!(
        progress
            .iter()
            .all(|line| !line.contains("sk-fixture-secret"))
    );
}
