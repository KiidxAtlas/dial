use super::*;
use crate::storage::memory_backend::test_support::{FakeRunner, authority, scope};
use rusqlite::Connection;
use std::collections::HashMap;
use std::time::Instant;

/// Deterministic embedder: vectors come from a table; `fail` simulates an outage.
struct Fake {
    model: String,
    table: HashMap<String, Vec<f32>>,
    fail: bool,
}

impl Embedder for Fake {
    fn model(&self) -> &str {
        &self.model
    }

    fn embed(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        ensure!(!self.fail, "endpoint down");
        inputs
            .iter()
            .map(|text| {
                self.table
                    .get(text)
                    .cloned()
                    .with_context(|| format!("no fake vector for {text:?}"))
            })
            .collect()
    }
}

fn store() -> Store {
    Store::new(Connection::open_in_memory().unwrap()).unwrap()
}

fn fake(table: &[(&str, [f32; 2])], fail: bool) -> Fake {
    Fake {
        model: "test-embed".into(),
        table: table
            .iter()
            .map(|(text, vector)| ((*text).to_owned(), vector.to_vec()))
            .collect(),
        fail,
    }
}

fn no_check() -> Result<()> {
    Ok(())
}

/// Answers only after `delay`; a shorter budget times out, as the HTTP embedder reports.
struct Slow {
    delay: Duration,
}

impl Embedder for Slow {
    fn model(&self) -> &str {
        "slow-embed"
    }

    fn embed(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        std::thread::sleep(self.delay);
        Ok(inputs.iter().map(|_| vec![1.0, 0.0]).collect())
    }

    fn embed_within(&mut self, inputs: &[String], budget: Duration) -> Result<Vec<Vec<f32>>> {
        if budget < self.delay {
            std::thread::sleep(budget);
            anyhow::bail!("timed out after {} ms", budget.as_millis());
        }
        self.embed(inputs)
    }
}

fn semantic_with(embedder: Option<Box<dyn Embedder>>) -> Semantic {
    Semantic {
        store: store(),
        user: store(),
        embedder,
        embedding_mode: EmbeddingMode::Auto,
        embedding_note: None,
        token_budget: 1500,
        auto_retain: true,
        retain_paused: false,
        consolidate_interval_secs: 0,
        turn_failures: HashMap::new(),
        retain_note: None,
        embed_error: None,
        recall_note: None,
        project_cache: RecallCache::default(),
        user_cache: RecallCache::default(),
        last_recall: None,
    }
}

#[test]
fn late_query_embedding_returns_keyword_results_within_the_recall_deadline() {
    let mut semantic = semantic_with(Some(Box::new(Slow {
        delay: Duration::from_secs(30),
    })));
    semantic
        .store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let started = Instant::now();
    let deadline = started + Duration::from_millis(1500);
    let op = Operation::for_test(authority("s"), scope(), Some(deadline));
    let found = semantic
        .search(&op, "deploy vault", 8, &mut FakeRunner::new("{}"))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(found["mode"], "lexical");
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    let error = found["embedding_error"].as_str().unwrap();
    assert!(error.contains("timed out"), "{error}");
    let notes = semantic.status(&op).unwrap()["notes"].to_string();
    assert!(notes.contains("keyword results only"), "{notes}");
    // Without a deadline (the model's own recall tool path) the embedder's timeout applies.
    let mut quick = semantic_with(Some(Box::new(Slow {
        delay: Duration::from_millis(1),
    })));
    quick.store.retain("deploy vault", "test", 1).unwrap();
    let op = Operation::for_test(authority("s"), scope(), None);
    let found = quick
        .search(&op, "deploy", 8, &mut FakeRunner::new("{}"))
        .unwrap();
    assert_eq!(found["mode"], "hybrid");
}

#[test]
fn http_query_embedding_stops_at_the_recall_budget() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
    // Accepts the request and never answers, like a cold model server.
    std::thread::spawn(move || {
        let held = listener.accept();
        std::thread::sleep(Duration::from_secs(10));
        drop(held);
    });
    let mut embedder = http_embedder(&url, EmbeddingWire::OpenAi, None, Vec::new());
    embedder.timeout_secs = 20;
    let started = Instant::now();
    let error = embedder
        .embed_within(&["query".to_owned()], Duration::from_millis(400))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(
        format!("{error:#}").contains("ms left for recall"),
        "{error:#}"
    );
}

#[test]
fn failing_extraction_backs_off_and_consolidation_still_runs() {
    struct Failing(usize);
    impl MemoryModelRunner for Failing {
        fn limits(
            &mut self,
            _: &str,
            _: Option<&str>,
            _: &str,
            _: u32,
            _: &Operation,
        ) -> Result<super::super::MemoryModelLimits> {
            Ok(super::super::MemoryModelLimits {
                context_window: 32768,
                max_output_tokens: 1024,
                max_input_bytes: 1 << 20,
            })
        }
        fn complete(&mut self, _: super::super::MemoryModelRequest) -> Result<String> {
            self.0 += 1;
            anyhow::bail!("memory model unavailable")
        }
        fn authorize_external(&mut self, _: &Operation, _: &str) -> Result<u64> {
            Ok(0)
        }
        fn external_started(&mut self, _: &Operation, _: u64) -> Result<()> {
            Ok(())
        }
        fn account_external(&mut self, _: &Operation, _: u64, _: &Value, _: bool) -> Result<()> {
            Ok(())
        }
        fn abandon_external(&mut self, _: &Operation, _: u64) -> Result<()> {
            Ok(())
        }
        fn authorize_managed_write(
            &mut self,
            _: &Operation,
            _: &[std::path::PathBuf],
        ) -> Result<()> {
            Ok(())
        }
    }
    let mut semantic = semantic_with(None);
    semantic.consolidate_interval_secs = 60;
    semantic
        .store
        .retain("Use the staging database for billing tests", "test", 1)
        .unwrap();
    semantic
        .store
        .retain("Use the staging database for billing tests now", "test", 2)
        .unwrap();
    semantic
        .store
        .enqueue_turn("s", "a long enough prompt about billing tests", "ok", 1)
        .unwrap();
    let op = Operation::for_test(authority("s"), scope(), None);
    let mut runner = Failing(0);
    assert!(semantic.maintain(&op, &mut runner).is_ok());
    assert_eq!(runner.0, 1);
    assert_eq!(
        semantic.store.recent(10).unwrap().len(),
        1,
        "the duplicate merge ran despite the failed turn"
    );
    // The failed turn waits for its backoff instead of being retried on every pass.
    semantic.maintain(&op, &mut runner).unwrap();
    assert_eq!(runner.0, 1);
    let notes = semantic.status(&op).unwrap()["notes"].to_string();
    assert!(notes.contains("retries in 60 s"), "{notes}");
    // After the last allowed attempt the turn is dropped, not retried forever.
    for _ in 1..MAX_TURN_ATTEMPTS {
        semantic
            .turn_failures
            .values_mut()
            .for_each(|entry| entry.1 = 0);
        semantic.maintain(&op, &mut runner).unwrap();
    }
    assert_eq!(runner.0, MAX_TURN_ATTEMPTS as usize);
    assert!(semantic.store.next_turns(10).unwrap().is_empty());
    assert!(
        semantic
            .retain_note
            .as_deref()
            .unwrap()
            .contains("skipped a turn")
    );
}

#[test]
fn lexical_recall_works_without_any_embedder() {
    let mut store = store();
    store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    store
        .retain("pizza dough needs two hours", "test", 2)
        .unwrap();
    let found = recall(&store, None, "deploy vault", 8, 1500).unwrap();
    assert_eq!(found.mode, "lexical");
    assert!(found.embedding_error.is_none());
    assert_eq!(found.facts.len(), 1);
    assert!(found.facts[0].content.contains("deploy vault"));
}

#[test]
fn hybrid_recall_surfaces_vector_neighbour_and_fuses_in_rank_order() {
    let mut store = store();
    let a = match store.retain("alpha note about billing", "test", 1).unwrap() {
        Saved::Created(fact) => fact.id,
        Saved::Existing(_) => unreachable!(),
    };
    let b = match store
        .retain("unrelated wording entirely", "test", 2)
        .unwrap()
    {
        Saved::Created(fact) => fact.id,
        Saved::Existing(_) => unreachable!(),
    };
    let mut embedder = fake(
        &[
            ("alpha note about billing", [1.0, 0.0]),
            ("unrelated wording entirely", [0.0, 1.0]),
            ("alpha", [0.0, 1.0]),
        ],
        false,
    );
    embed_pending(&mut store, Some(&mut embedder), 16, &no_check).unwrap();
    // Lexical hits only A; the vector ranking puts B first. Fusion keeps A ahead because it is
    // ranked by both lists, and B is recalled through the vector index alone.
    let found = recall(&store, Some(&mut embedder), "alpha", 8, 1500).unwrap();
    assert_eq!(found.mode, "hybrid");
    let ids: Vec<i64> = found.facts.iter().map(|fact| fact.id).collect();
    assert_eq!(ids, vec![a, b]);
}

#[test]
fn embedding_outage_degrades_to_lexical_and_reports_error() {
    let mut store = store();
    store.retain("deploy vault token", "test", 1).unwrap();
    let mut embedder = fake(&[], true);
    let found = recall(&store, Some(&mut embedder), "deploy", 8, 1500).unwrap();
    assert_eq!(found.mode, "lexical");
    assert_eq!(found.facts.len(), 1);
    assert!(found.embedding_error.unwrap().contains("endpoint down"));
    let report = embed_pending(&mut store, Some(&mut embedder), 16, &no_check).unwrap();
    assert_eq!(report.embedded, 0);
    assert!(report.error.is_some());
}

#[test]
fn token_budget_cuts_lower_ranked_facts_without_skipping_ahead() {
    let mut store = store();
    let long = format!(
        "vault {}",
        "cache eviction policy and retry notes ".repeat(2)
    );
    store.retain(&long, "test", 1).unwrap();
    store.retain("vault short note", "test", 2).unwrap();
    let all = recall(&store, None, "vault", 8, 10_000).unwrap();
    assert_eq!(all.facts.len(), 2);
    // The long fact costs 29 tokens and the short one 12: 41 together, so a 40-token budget
    // keeps only the top-ranked fact and reports the cut.
    let found = recall(&store, None, "vault", 8, 40).unwrap();
    assert_eq!(found.facts.len(), 1);
    assert!(found.truncated);
    assert_eq!(found.facts[0].id, all.facts[0].id);
}

#[test]
fn embedding_urls_follow_each_wire_format() {
    let openai = |base| embedding_url(EmbeddingWire::OpenAi, base, false, "m");
    assert_eq!(
        openai("https://api.example.com/v1/").unwrap(),
        "https://api.example.com/v1/embeddings"
    );
    assert_eq!(
        openai("http://127.0.0.1:8080/v1").unwrap(),
        "http://127.0.0.1:8080/v1/embeddings"
    );
    assert_eq!(
        openai("http://localhost:1234").unwrap(),
        "http://localhost:1234/v1/embeddings"
    );
    assert!(openai("http://api.example.com/v1").is_err());
    assert!(openai("https://user:pw@api.example.com").is_err());
    assert!(openai("https://api.example.com/v1?key=1").is_err());
    for wire in [
        EmbeddingWire::OpenAi,
        EmbeddingWire::Ollama,
        EmbeddingWire::Gemini,
    ] {
        assert!(embedding_url(wire, "https://api.example.com/v1#secret", false, "m").is_err());
    }
    assert!(openai("not a url").is_err());
    assert_eq!(
        embedding_url(
            EmbeddingWire::OpenAi,
            "http://api.example.com/v1",
            true,
            "m"
        )
        .unwrap(),
        "http://api.example.com/v1/embeddings"
    );
    let ollama = |base| embedding_url(EmbeddingWire::Ollama, base, false, "m");
    assert_eq!(
        ollama("http://localhost:11434").unwrap(),
        "http://localhost:11434/api/embed"
    );
    assert_eq!(
        ollama("http://localhost:11434/v1/").unwrap(),
        "http://localhost:11434/api/embed"
    );
    let gemini = |model| {
        embedding_url(
            EmbeddingWire::Gemini,
            "https://generativelanguage.googleapis.com",
            false,
            model,
        )
    };
    assert_eq!(
        gemini("text-embedding-004").unwrap(),
        "https://generativelanguage.googleapis.com/v1beta/models/text-embedding-004:batchEmbedContents"
    );
    assert_eq!(
        gemini("models/gemini-embedding-001").unwrap(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-embedding-001:batchEmbedContents"
    );
    for model in ["", "../x", "a/b", "a?b", ".."] {
        assert!(gemini(model).is_err(), "{model:?} must not reach the path");
    }
}

#[test]
fn openai_responses_reorder_by_index_and_tolerate_missing_index() {
    let ordered = br#"{"data":[{"index":1,"embedding":[0,1]},{"index":0,"embedding":[1,0]}]}"#;
    assert_eq!(
        parse_embeddings(EmbeddingWire::OpenAi, ordered, 2).unwrap(),
        vec![vec![1.0, 0.0], vec![0.0, 1.0]]
    );
    let unindexed = br#"{"data":[{"embedding":[1,0]},{"embedding":[0,1]}]}"#;
    assert_eq!(
        parse_embeddings(EmbeddingWire::OpenAi, unindexed, 2).unwrap(),
        vec![vec![1.0, 0.0], vec![0.0, 1.0]]
    );
    let short = br#"{"data":[{"index":0,"embedding":[1,0]}]}"#;
    assert!(parse_embeddings(EmbeddingWire::OpenAi, short, 2).is_err());
    let mixed = br#"{"data":[{"index":0,"embedding":[1,0]},{"index":1,"embedding":[1]}]}"#;
    assert!(parse_embeddings(EmbeddingWire::OpenAi, mixed, 2).is_err());
    let duplicate = br#"{"data":[{"index":0,"embedding":[1]},{"index":0,"embedding":[1]}]}"#;
    assert!(parse_embeddings(EmbeddingWire::OpenAi, duplicate, 2).is_err());
    assert!(parse_embeddings(EmbeddingWire::OpenAi, b"not json", 1).is_err());
}

#[test]
fn index_and_number_boundaries_are_checked_before_use() {
    for body in [
        r#"{"data":[{"index":1,"embedding":[1]}]}"#,
        r#"{"data":[{"index":-1,"embedding":[1]}]}"#,
        r#"{"data":[{"index":0.5,"embedding":[1]}]}"#,
        r#"{"data":[{"index":"0","embedding":[1]}]}"#,
        r#"{"data":[{"index":null,"embedding":[1]}]}"#,
        r#"{"data":[{"index":0,"embedding":[1e39]}]}"#,
        r#"{"data":[{"index":0,"embedding":["1"]}]}"#,
        r#"{"data":[{"index":0,"embedding":[]}]}"#,
    ] {
        assert!(
            parse_embeddings(EmbeddingWire::OpenAi, body.as_bytes(), 1).is_err(),
            "{body}"
        );
    }
    let edge = br#"{"data":[{"index":0,"embedding":[3.4028234e38,-1]}]}"#;
    assert_eq!(
        parse_embeddings(EmbeddingWire::OpenAi, edge, 1).unwrap(),
        vec![vec![f32::MAX, -1.0]]
    );
}

#[test]
fn native_wires_parse_their_own_shapes_in_input_order() {
    let ollama = br#"{"model":"m","embeddings":[[1,0],[0,1]]}"#;
    assert_eq!(
        parse_embeddings(EmbeddingWire::Ollama, ollama, 2).unwrap(),
        vec![vec![1.0, 0.0], vec![0.0, 1.0]]
    );
    assert!(parse_embeddings(EmbeddingWire::Ollama, ollama, 3).is_err());
    let gemini = br#"{"embeddings":[{"values":[1,0]},{"values":[0,1]}]}"#;
    assert_eq!(
        parse_embeddings(EmbeddingWire::Gemini, gemini, 2).unwrap(),
        vec![vec![1.0, 0.0], vec![0.0, 1.0]]
    );
    assert!(parse_embeddings(EmbeddingWire::Gemini, ollama, 2).is_err());
}

fn serve_once(status: u16, body: &str) -> (String, crate::llm::fake::Fake) {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
        status,
        &serde_json::from_str(body).unwrap(),
    )]);
    (server.url.clone(), server)
}

fn recorded_request(server: crate::llm::fake::Fake) -> String {
    String::from_utf8(server.requests().remove(0))
        .unwrap()
        .to_ascii_lowercase()
}

fn http_embedder(
    url: &str,
    wire: EmbeddingWire,
    token: Option<&str>,
    headers: Vec<RequestHeader>,
) -> HttpEmbedder {
    HttpEmbedder {
        url: url.into(),
        wire,
        model: "m".into(),
        token: token.map(str::to_owned),
        headers,
        timeout_secs: 5,
    }
}

#[test]
fn openai_wire_sends_bearer_and_custom_headers_and_reads_vectors() {
    let (base, server) = serve_once(200, r#"{"data":[{"index":0,"embedding":[1,0]}]}"#);
    let headers = vec![RequestHeader {
        name: "X-Route-Pool".into(),
        value: HeaderValue::Literal("eval".into()),
    }];
    let url = format!("{base}/v1/embeddings");
    let mut embedder = http_embedder(&url, EmbeddingWire::OpenAi, Some("sk-test"), headers);
    assert_eq!(
        embedder.embed(&["hello".to_owned()]).unwrap(),
        vec![vec![1.0, 0.0]]
    );
    let request = recorded_request(server);
    assert!(request.starts_with("post /v1/embeddings "));
    assert!(request.contains("authorization: bearer sk-test\r\n"));
    assert!(request.contains("x-route-pool: eval\r\n"));
    assert!(request.contains(r#""input":["hello"]"#));
}

#[test]
fn gemini_wire_posts_batch_request_with_key_header_and_no_bearer() {
    let (base, server) = serve_once(200, r#"{"embeddings":[{"values":[0,1]}]}"#);
    let url = embedding_url(EmbeddingWire::Gemini, &base, false, "gemini-embedding-001").unwrap();
    let mut embedder = http_embedder(&url, EmbeddingWire::Gemini, Some("sk-g"), Vec::new());
    embedder.model = "gemini-embedding-001".into();
    assert_eq!(
        embedder.embed(&["hello".to_owned()]).unwrap(),
        vec![vec![0.0, 1.0]]
    );
    let request = recorded_request(server);
    assert!(request.starts_with("post /v1beta/models/gemini-embedding-001:batchembedcontents "));
    assert!(request.contains("x-goog-api-key: sk-g\r\n"));
    assert!(!request.contains("authorization:"));
    assert!(request.contains(r#""model":"models/gemini-embedding-001""#));
    assert!(request.contains(r#""parts":[{"text":"hello"}]"#));
}

#[test]
fn ollama_wire_posts_native_request_without_truncation() {
    let (base, server) = serve_once(200, r#"{"embeddings":[[1,0]]}"#);
    let url = embedding_url(EmbeddingWire::Ollama, &base, false, "m").unwrap();
    let mut embedder = http_embedder(&url, EmbeddingWire::Ollama, None, Vec::new());
    assert_eq!(
        embedder.embed(&["hello".to_owned()]).unwrap(),
        vec![vec![1.0, 0.0]]
    );
    let request = recorded_request(server);
    assert!(request.starts_with("post /api/embed "));
    assert!(request.contains(r#""truncate":false"#));
    assert!(!request.contains("authorization:"));
}

#[test]
fn provider_error_keeps_status_and_message_but_never_the_key() {
    let (base, server) = serve_once(
        429,
        r#"{"error":{"message":"rate limited for key sk-secret-token"}}"#,
    );
    let url = format!("{base}/v1/embeddings");
    let mut embedder = http_embedder(
        &url,
        EmbeddingWire::OpenAi,
        Some("sk-secret-token"),
        Vec::new(),
    );
    let error = format!("{:#}", embedder.embed(&["hi".to_owned()]).unwrap_err());
    server.requests();
    assert!(error.contains("HTTP 429"), "{error}");
    assert!(error.contains("rate limited"), "{error}");
    assert!(!error.contains("sk-secret-token"), "{error}");
}

#[test]
fn provider_error_redacts_credentials_before_bounding_the_excerpt() {
    let secret = "private-credential".repeat(30);
    let bytes =
        serde_json::to_vec(&json!({"error":{"message":format!("denied {secret}")}})).unwrap();
    let detail = provider_detail(&bytes, &[secret]).unwrap();
    assert_eq!(detail, "denied [redacted]");
}

#[test]
fn a_second_session_opens_the_store_while_the_first_is_still_using_it() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let layout = Layout::with_user_dir(&project, dir.path().join("home")).unwrap();
    let config = SemanticConfig::default();
    let mut first = open(&layout, &config, Ok(None), false).unwrap();
    let mut second = open(&layout, &config, Ok(None), false)
        .expect("reopening must not switch the journal mode under the live connection");
    let op = Operation::for_test(authority("session"), scope(), None);
    assert_eq!(first.status(&op).unwrap()["backend"], "semantic");
    assert_eq!(second.status(&op).unwrap()["backend"], "semantic");
}

#[test]
fn off_ignores_unusable_role_and_partial_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let config = SemanticConfig {
        embedding: EmbeddingMode::Off,
        embedding_base_url: "https://override.example/v1".into(),
        ..Default::default()
    };
    assert!(
        build_embedder(&config, Err("role unusable".into()), dir.path())
            .unwrap()
            .is_none()
    );
}

#[test]
fn complete_override_is_used_when_the_role_is_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let config = SemanticConfig {
        embedding_base_url: "https://override.example/v1".into(),
        embedding_model: "override-model".into(),
        ..Default::default()
    };
    let embedder = build_embedder(&config, Err("role unusable".into()), dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(embedder.model(), "override-model");
}

#[test]
fn explicit_override_needs_both_halves_and_unusable_role_degrades_only_under_auto() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SemanticConfig::default();
    assert!(
        build_embedder(&config, Ok(None), dir.path())
            .unwrap()
            .is_none()
    );
    // Construction preserves the diagnostic; open() handles Auto's lexical fallback.
    assert!(build_embedder(&config, Err("no endpoint".into()), dir.path()).is_err());
    config.embedding = EmbeddingMode::On;
    assert!(build_embedder(&config, Ok(None), dir.path()).is_err());
    assert!(build_embedder(&config, Err("no endpoint".into()), dir.path()).is_err());
    config.embedding = EmbeddingMode::Auto;
    config.embedding_base_url = "https://override.example/v1".into();
    assert!(build_embedder(&config, Err("no endpoint".into()), dir.path()).is_err());
}

#[test]
fn role_headers_are_validated_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let route = EmbeddingRoute {
        base_url: "https://api.example.com/v1".into(),
        model: "m".into(),
        api_key_env: None,
        explicit: false,
        wire: EmbeddingWire::OpenAi,
        allow_insecure: false,
        headers: vec![RequestHeader {
            name: "Authorization".into(),
            value: HeaderValue::Literal("Bearer x".into()),
        }],
    };
    assert!(build_embedder(&SemanticConfig::default(), Ok(Some(route)), dir.path()).is_err());
}

#[test]
fn role_wire_follows_the_endpoint_protocol() {
    use crate::llm::model_config::{ApiProtocol, ModelSelection};
    let mut config = crate::config::types::Config::default();
    let mut selection = ModelSelection::new("custom-embed", "embed-model");
    selection.base_url = Some("http://127.0.0.1:1234/v1".into());
    config
        .model_roles
        .insert("embedding".into(), selection.clone());
    let route = role_from_config(&config).unwrap().unwrap();
    assert_eq!(route.wire, EmbeddingWire::OpenAi);
    assert_eq!(route.model, "embed-model");
    selection.protocol = Some(ApiProtocol::Ollama);
    config
        .model_roles
        .insert("embedding".into(), selection.clone());
    assert_eq!(
        role_from_config(&config).unwrap().unwrap().wire,
        EmbeddingWire::Ollama
    );
    selection.protocol = Some(ApiProtocol::Anthropic);
    config.model_roles.insert("embedding".into(), selection);
    let reason = role_from_config(&config).unwrap_err();
    assert!(reason.contains("anthropic"), "{reason}");
}

#[test]
fn source_label_is_bounded_on_a_character_boundary() {
    let context = "é".repeat(2000);
    let label = source_label(Some("tool"), Some(&context));
    assert!(label.len() <= MAX_SOURCE_BYTES);
    assert!(label.starts_with("tool ("));
}

fn id_of(saved: Saved) -> i64 {
    match saved {
        Saved::Created(fact) | Saved::Existing(fact) => fact.id,
    }
}

fn token_set(text: &str) -> HashSet<String> {
    query_tokens(text).into_iter().collect()
}

#[test]
fn similarity_threshold_is_inclusive_at_eighty_percent() {
    let four = token_set("alpha beta gamma delta");
    let five = token_set("alpha beta gamma delta epsilon");
    assert!(similar(&four, &five), "4 of 5 shared tokens is exactly 0.8");
    let three = token_set("alpha beta gamma");
    assert!(!similar(&three, &four), "3 of 4 shared tokens is 0.75");
    assert!(!similar(&HashSet::new(), &HashSet::new()));
}

#[test]
fn merge_keeps_the_older_fact_and_forgets_the_newer_near_copy() {
    let mut store = store();
    let older = id_of(
        store
            .retain("Use the staging database for billing tests", "test", 1)
            .unwrap(),
    );
    let newer = id_of(
        store
            .retain("Use the staging database for billing tests now", "test", 2)
            .unwrap(),
    );
    let other = id_of(
        store
            .retain("Deploys happen on Tuesdays", "test", 3)
            .unwrap(),
    );
    assert_eq!(merge_duplicates(&mut store, 4).unwrap(), 1);
    let live = store.recent(10).unwrap();
    assert!(live.contains(&older));
    assert!(live.contains(&other));
    assert!(!live.contains(&newer));
    assert_eq!(
        merge_duplicates(&mut store, 5).unwrap(),
        0,
        "a second pass finds nothing"
    );
}

#[test]
fn merge_ignores_facts_too_short_to_compare() {
    let mut store = store();
    store.retain("use pnpm", "test", 1).unwrap();
    store.retain("use pnpm now", "test", 2).unwrap();
    assert_eq!(merge_duplicates(&mut store, 3).unwrap(), 0);
    assert_eq!(store.recent(10).unwrap().len(), 2);
}

#[test]
fn consolidation_is_due_only_after_a_positive_interval() {
    assert!(consolidation_due(0, 3600, 3600));
    assert!(!consolidation_due(100, 3599, 3600));
    assert!(!consolidation_due(0, 1, 0), "zero disables merging");
    assert!(
        !consolidation_due(i64::MAX, 5, 60),
        "a future marker never fires early"
    );
}

#[test]
fn extraction_keeps_only_well_formed_facts_with_scopes() {
    let reply = r#"{"facts":[{"content":" Use pnpm ","scope":"user"},{"content":"  ","scope":"project"},{"content":"Ship on Tuesdays"}]}"#;
    assert_eq!(
        parse_extracted(reply),
        vec![
            ("Use pnpm".to_owned(), true),
            ("Ship on Tuesdays".to_owned(), false)
        ]
    );
}

#[test]
fn merge_keeps_both_facts_when_their_numbers_differ() {
    let mut store = store();
    store.retain("API rate limit is 1 rps", "test", 1).unwrap();
    store.retain("API rate limit is 2 rps", "test", 2).unwrap();
    assert_eq!(merge_duplicates(&mut store, 3).unwrap(), 0);
    assert_eq!(store.recent(10).unwrap().len(), 2);
}

#[test]
fn extraction_accepts_one_markdown_fence_around_strict_json() {
    let fenced = "```json\n{\"facts\":[{\"content\":\"Use pnpm\"}]}\n```";
    assert_eq!(
        parse_extracted(fenced),
        vec![("Use pnpm".to_owned(), false)]
    );
    let bare = "```\n{\"facts\":[{\"content\":\"Use pnpm\"}]}\n```";
    assert_eq!(parse_extracted(bare).len(), 1);
    assert!(parse_extracted("```json\nnot json\n```").is_empty());
}

#[test]
fn malformed_extraction_yields_nothing_and_fact_count_is_capped() {
    assert!(parse_extracted("Sure! Here are the facts: pnpm").is_empty());
    assert!(parse_extracted(r#"{"facts":"pnpm"}"#).is_empty());
    let many: Vec<String> = (0..MAX_FACTS_PER_TURN + 4)
        .map(|index| format!(r#"{{"content":"fact {index}"}}"#))
        .collect();
    let reply = format!(r#"{{"facts":[{}]}}"#, many.join(","));
    assert_eq!(parse_extracted(&reply).len(), MAX_FACTS_PER_TURN);
}

#[test]
fn bounded_truncates_on_a_character_boundary() {
    assert_eq!(bounded("ééé", 2), "éé");
    assert_eq!(bounded("short", 10), "short");
}

#[test]
fn identical_turns_are_queued_once_and_finished_turns_leave_the_queue() {
    let mut store = store();
    store
        .enqueue_turn("session", "remember the vault path", "it is /srv/vault", 1)
        .unwrap();
    store
        .enqueue_turn("session", "remember the vault path", "it is /srv/vault", 2)
        .unwrap();
    let queued = store.next_turns(10).unwrap();
    assert_eq!(queued.len(), 1);
    store.finish_turn(queued[0].0).unwrap();
    assert!(store.next_turns(10).unwrap().is_empty());
}

#[test]
fn meta_values_round_trip_and_overwrite() {
    let mut store = store();
    assert_eq!(store.meta("last_consolidated").unwrap(), None);
    store.set_meta("last_consolidated", "10").unwrap();
    store.set_meta("last_consolidated", "20").unwrap();
    assert_eq!(
        store.meta("last_consolidated").unwrap().as_deref(),
        Some("20")
    );
}

fn request(query: &str) -> Request<'_> {
    Request {
        query,
        vector: None,
        embedding_error: None,
        limit: 8,
        token_budget: 1500,
    }
}

#[test]
fn repeated_recall_is_served_from_cache_until_the_store_changes() {
    let mut store = store();
    store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let mut cache = RecallCache::default();
    let (_, first) = cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    let (second, again) = cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    assert_eq!(first, CacheOutcome::Miss);
    assert_eq!(again, CacheOutcome::Hit);
    assert_eq!(second.facts.len(), 1);
}

#[test]
fn a_write_to_the_store_invalidates_cached_recall() {
    let mut store = store();
    store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let mut cache = RecallCache::default();
    cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    store
        .retain("the deploy vault key rotates weekly", "test", 2)
        .unwrap();
    let (recall, outcome) = cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    assert_eq!(outcome, CacheOutcome::Miss);
    assert_eq!(recall.facts.len(), 2);
}

#[test]
fn a_write_through_another_connection_invalidates_cached_recall() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite3");
    let mut store = Store::new(Connection::open(&path).unwrap()).unwrap();
    store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let mut cache = RecallCache::default();
    cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    let mut other = Store::new(Connection::open(&path).unwrap()).unwrap();
    other
        .retain("the deploy vault key rotates weekly", "test", 2)
        .unwrap();
    let (recall, outcome) = cached_ranked(&mut cache, &store, request("deploy vault"), 10).unwrap();
    assert_eq!(outcome, CacheOutcome::Miss);
    assert_eq!(recall.facts.len(), 2);
}

#[test]
fn temporal_and_degraded_recall_bypass_the_cache() {
    let mut store = store();
    store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let mut cache = RecallCache::default();
    let (_, temporal) =
        cached_ranked(&mut cache, &store, request("deploy vault yesterday"), 10).unwrap();
    let degraded = Request {
        embedding_error: Some("endpoint down".into()),
        ..request("deploy vault")
    };
    let (_, outcome) = cached_ranked(&mut cache, &store, degraded, 10).unwrap();
    assert_eq!(temporal, CacheOutcome::Bypass);
    assert_eq!(outcome, CacheOutcome::Bypass);
    assert_eq!(cache.stats().entries, 0);
}

#[test]
fn a_temporal_query_ranks_the_newest_facts_first() {
    let mut store = store();
    let now = 1_000_000;
    store
        .retain("deploy plan deploy plan from the old quarter", "test", 1)
        .unwrap();
    store
        .retain("deploy plan revised this morning", "test", now - 3600)
        .unwrap();
    let plain = ranked(&store, request("deploy plan"), now).unwrap();
    assert_eq!(plain.voices, ["lexical"]);
    assert!(plain.facts[0].content.contains("old quarter"));
    let temporal = ranked(&store, request("deploy plan yesterday"), now).unwrap();
    assert_eq!(temporal.voices, ["lexical", "recency"]);
    assert!(temporal.facts[0].content.contains("revised this morning"));
}

#[test]
fn status_reports_cache_use_and_the_last_recall() {
    let mut semantic = semantic_with(None);
    semantic
        .store
        .retain("the deploy vault token rotates monthly", "test", 1)
        .unwrap();
    let op = Operation::for_test(authority("s"), scope(), None);
    for _ in 0..2 {
        semantic
            .search(&op, "deploy vault", 8, &mut FakeRunner::new("{}"))
            .unwrap();
    }
    let status = semantic.status(&op).unwrap();
    assert_eq!(status["recall"]["cache"]["project"]["hits"], 1);
    assert_eq!(status["recall"]["cache"]["project"]["misses"], 1);
    assert_eq!(status["recall"]["last"]["projectCache"], "hit");
    assert_eq!(status["recall"]["last"]["items"], 1);
    assert_eq!(status["recall"]["last"]["voices"], json!(["lexical"]));
}
