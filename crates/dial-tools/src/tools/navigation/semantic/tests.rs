use super::*;
use std::fs;
// A deterministic fake provider tests the request/response protocol, not model quality.
fn fake_provider(system: &str, prompt: &str) -> String {
    assert!(system.contains("untrusted DATA"));
    let request: Value = serde_json::from_str(prompt).unwrap();
    assert_eq!(
        request["query"],
        "reuse an existing value without doing the expensive work again"
    );
    let candidates = request["candidates"].as_array().unwrap();
    let hit = candidates
        .iter()
        .find(|candidate| {
            candidate["source_evidence"]
                .as_str()
                .unwrap()
                .contains("memo.get(key)")
        })
        .expect("disjoint-word declaration must be submitted");
    assert!(
        hit["source_evidence"]
            .as_str()
            .unwrap()
            .contains("fn cached_lookup")
    );
    json!({"indices":[hit["index"]]}).to_string()
}
#[test]
fn synonym_and_disjoint_query_ranking_remains_source_grounded() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("cache.rs"), "fn cached_lookup(key: &str) {\n    if let Some(hit) = memo.get(key) { return hit; }\n    calculate(key)\n}\n").unwrap();
    fs::write(
        dir.path().join("render.py"),
        "def paint():\n    canvas.flush()\n",
    )
    .unwrap();
    let args = json!({"query":"reuse an existing value without doing the expensive work again"});
    let request = semantic_request(&ws, &args).unwrap().unwrap();
    assert!(request.has_candidates());
    let response = fake_provider(&request.system, &request.prompt);
    let rendered = request.render_response(&response).unwrap();
    assert!(rendered.contains("cache.rs:1-4 semantic_rank=1"));
    assert!(!rendered.contains("render.py:"));
    assert!(request.render_response("{\"indices\":[9999]}").is_err());
    assert!(request.render_response("{\"indices\":[0,0]}").is_err());
    assert!(
        request
            .render_response("{\"indices\":[0],\"path\":\"invented.rs\"}")
            .is_err()
    );
    assert!(request.render_response("{\"indices\":[-1]}").is_err());
    assert!(request.render_response("{\"indices\":[0.5]}").is_err());
}
#[test]
fn large_high_ranked_file_does_not_crowd_out_semantic_paths() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let large: String = (0..1200)
        .map(|index| format!("fn expensive_work_{index}() {{ calculate(); }}\n"))
        .collect();
    assert!(super::super::declarations(std::path::Path::new("a.rs"), &large).len() > 1000);
    fs::write(dir.path().join("a.rs"), &large).unwrap();
    fs::write(dir.path().join("z_cache.rs"), "fn cached_lookup(key: &str) {\n    if let Some(hit) = memo.get(key) { return hit; }\n    calculate(key)\n}\n").unwrap();
    let args = json!({"query":"reuse an existing value without doing the expensive work again"});
    let lexical = retrieve(&ws, &args, false).unwrap();
    assert_eq!(lexical.results.len(), 1000);
    assert!(lexical.results.iter().all(|hit| hit.path == "a.rs"));
    let request = semantic_request(&ws, &args).unwrap().unwrap();
    let payload: Value = serde_json::from_str(&request.prompt).unwrap();
    let candidates = payload["candidates"].as_array().unwrap();
    assert!(candidates.len() <= MAX_CANDIDATES);
    assert!(request.prompt.len() <= MAX_PROMPT);
    assert!(
        candidates
            .iter()
            .filter(|hit| hit["path"] == "a.rs")
            .count()
            <= 4
    );
    assert!(
        candidates.iter().any(|hit| hit["path"] == "z_cache.rs"),
        "a lower-lexical-score file must survive before semantic path diversity"
    );
    let response = fake_provider(&request.system, &request.prompt);
    let rendered = request.render_response(&response).unwrap();
    assert!(rendered.contains("z_cache.rs:1-4 semantic_rank=1"));
    assert!(!rendered.contains("a.rs:"));
    fs::write(dir.path().join("z_cache.rs"), "fn changed() {}\n").unwrap();
    let stale = request.render_response(&response).unwrap();
    assert!(!stale.contains("z_cache.rs:1-4"));
    assert!(stale.contains("model-selected candidates omitted"));
}
#[test]
fn semantic_diversity_preserves_global_and_prompt_bounds() {
    let (dir, ws) = crate::tools::testkit::workspace();
    for index in 0..260 {
        fs::write(
            dir.path().join(format!("file_{index:03}.rs")),
            "fn work_a() {}\nfn work_b() {}\nfn work_c() {}\nfn work_d() {}\nfn work_e() {}\n",
        )
        .unwrap();
    }
    let args = json!({"query":"work"});
    let retrieved = retrieve(&ws, &args, true).unwrap();
    assert_eq!(retrieved.results.len(), 1000);
    let mut counts = BTreeMap::new();
    for hit in &retrieved.results {
        *counts.entry(&hit.path).or_insert(0) += 1;
    }
    assert_eq!(counts.len(), 250);
    assert!(counts.values().all(|count| *count == MAX_PER_PATH));
    assert!(retrieved.results.iter().all(|hit| hit.start <= 4));
    let request = semantic_request(&ws, &args).unwrap().unwrap();
    assert_eq!(request.candidates.len(), MAX_CANDIDATES);
    assert!(request.prompt.len() <= MAX_PROMPT);
}
#[test]
fn exact_and_explicit_lexical_never_request_a_model_and_empty_is_honest() {
    let (dir, ws) = crate::tools::testkit::workspace();
    assert!(
        semantic_request(&ws, &json!({"query":"x", "mode":"exact"}))
            .unwrap()
            .is_none()
    );
    assert!(
        semantic_request(&ws, &json!({"query":"x", "ranking":"lexical"}))
            .unwrap()
            .is_none()
    );
    let request = semantic_request(&ws, &json!({"query":"x"}))
        .unwrap()
        .unwrap();
    assert!(!request.has_candidates());
    assert!(
        request
            .render_response("{\"indices\":[]}")
            .unwrap()
            .contains("No matching evidence")
    );
    assert!(request.render_response("{\"indices\":[0]}").is_err());
    fs::write(dir.path().join("a.rs"), "fn cached() { return stored; }\n").unwrap();
    let limited = semantic_request(&ws, &json!({"query":"memoize", "limit":1}))
        .unwrap()
        .unwrap();
    assert!(limited.render_response("{\"indices\":[0,1]}").is_err());
    assert!(limited.prompt.len() <= MAX_PROMPT);
}
#[test]
#[ignore = "requires explicit DIAL_SEMANTIC_SMOKE_BASE_URL and DIAL_SEMANTIC_SMOKE_MODEL real endpoint configuration"]
fn configured_model_semantic_smoke() {
    use crate::llm::{
        Chat,
        provider::{Backend, ProviderKind},
    };
    let base_url = std::env::var("DIAL_SEMANTIC_SMOKE_BASE_URL")
        .expect("configure a real OpenAI-compatible base URL");
    let model =
        std::env::var("DIAL_SEMANTIC_SMOKE_MODEL").expect("configure the real endpoint model");
    let api_key = std::env::var("DIAL_SEMANTIC_SMOKE_API_KEY").ok();
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("cache.rs"), "fn cached_lookup(key: &str) {\n    if let Some(hit) = memo.get(key) { return hit; }\n    calculate(key)\n}\n").unwrap();
    fs::write(
        dir.path().join("render.py"),
        "def paint():\n    canvas.flush()\n",
    )
    .unwrap();
    let request = semantic_request(&ws, &json!({"query":"reuse an existing value without doing the expensive work again", "limit":1})).unwrap().unwrap();
    let chat = Chat {
        messages: vec![
            json!({"role":"system", "content":request.system}),
            json!({"role":"user", "content":request.prompt}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 256,
        seed: None,
    };
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url,
            model,
            api_key,
        },
        "python3".into(),
        15,
    );
    let reply = backend
        .chat(None, &chat, 60, None)
        .expect("real semantic ranking endpoint failed");
    assert!(reply.tool_calls.is_empty());
    let rendered = request
        .render_response(&reply.text)
        .expect("real model failed strict grounding protocol");
    assert!(
        rendered.contains("cache.rs:1-4 semantic_rank=1"),
        "{rendered}"
    );
}
