use super::*;
#[test]
fn protected_trees_are_not_retrieved_from_warm_cache() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::write(private.join("secret.rs"), "fn private_canary() {}\n").unwrap();
    let args = json!({"query":"private_canary","mode":"exact"});
    assert!(find(&ws, &args).unwrap().contains("secret.rs"));
    let ws = ws.with_protected_paths(&[private]);
    assert!(!find(&ws, &args).unwrap().contains("secret.rs"));
    assert!(
        find(
            &ws,
            &json!({"query":"private_canary","path":"private","mode":"exact"})
        )
        .is_err()
    );
}
#[test]
fn finds_declarations_and_literal_multiline_deterministically() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("retry.rs"),
        "// reconnect network after a timeout\npub fn reconnect_network() {\n    retry();\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("other.py"),
        "def render_page():\n    pass\n",
    )
    .unwrap();
    let args = json!({"query":"network reconnect timeout"});
    let first = find(&ws, &args).unwrap();
    assert_eq!(first, find(&ws, &args).unwrap());
    assert!(first.contains("retry.rs:1-4"));
    let exact = find(&ws, &json!({"query":"retry();\n}","mode":"exact"})).unwrap();
    assert!(exact.contains("retry.rs:3-4"));
    assert!(!exact.contains("other.py:"));
}
#[test]
fn language_declarations_and_optional_server_contract() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("network.rs"),
        "pub fn reconnect_network() {\n    retry();\n}\n",
    )
    .unwrap();
    let evidence = find(&ws, &json!({"query":"reconnect network"})).unwrap();
    assert!(evidence.contains("network.rs:1-3"));
    assert!(evidence.contains("function_item"));
    assert!(lsp_args(&json!({"query":"timeout", "lsp_query":"timeout"})).is_err());
    let args = lsp_args(&json!({"query":"network behavior", "lsp_path":"network.rs", "lsp_query":"reconnect_network"})).unwrap().unwrap();
    assert_eq!(args["action"], "workspace_symbols");
    assert_eq!(args["query"], "reconnect_network");
    let tool = tools(&Config::default()).remove(0);
    assert!(!tool.needs_approval(&json!({"query":"network"})));
    assert_eq!(tool.schema()["function"]["name"], "find");
}
#[test]
fn confines_scope_and_honors_quoted_phrases() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("a.txt"), "network timeout\n").unwrap();
    assert!(find(&ws, &json!({"query":"timeout", "path":"../"})).is_err());
    assert!(
        find(&ws, &json!({"query":"\"timeout network\""}))
            .unwrap()
            .contains("No matching")
    );
    assert!(find(&ws, &json!({"query":"\"unclosed"})).is_err());
}

#[test]
fn retrieval_identity_changes_only_with_supporting_source() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("a.txt");
    fs::write(&path, "network timeout\n").unwrap();
    let query = json!({"query":"network", "mode":"exact"});
    let first = find(&ws, &query).unwrap();
    assert!(first.contains("source_sha256="));
    let identity = |text: String| {
        text.split("source_sha256=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    fs::write(dir.path().join("unrelated.txt"), "other\n").unwrap();
    assert_eq!(
        identity(first.clone()),
        identity(find(&ws, &query).unwrap())
    );
    fs::write(&path, "network timeout changed\n").unwrap();
    assert_ne!(identity(first), identity(find(&ws, &query).unwrap()));
}

/// A language server that fails every request, as one that exits during startup does.
struct FailingMetadata;

impl Tool for FailingMetadata {
    fn name(&self) -> &str {
        "lsp"
    }
    fn schema(&self) -> Value {
        json!({})
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }
    fn call(&self, _ctx: &ToolContext, _args: &Value) -> ToolOutput {
        ToolOutput::err("language server exited during startup")
    }
}

#[test]
fn a_failing_language_server_leaves_the_lexical_find_successful() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("retry.rs"),
        "pub fn reconnect_network() {}\n",
    )
    .unwrap();
    let config = Config::default();
    let tool = Find {
        lsp: Arc::new(FailingMetadata),
    };
    let out = tool.call(
        &ToolContext {
            workspace: &ws,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({"query":"reconnect_network","mode":"exact","lsp_path":"retry.rs"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("retry.rs"), "{}", out.text);
    assert!(
        out.text.contains("language-server metadata failed"),
        "{}",
        out.text
    );
}

#[test]
fn related_bundle_states_provenance_and_is_bounded() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("retry.rs"),
        "pub struct RetryPolicy {\n    attempts: u32,\n}\npub fn reconnect_network(policy: &RetryPolicy) {\n    retry();\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("caller.rs"),
        "fn run() {\n    reconnect_network(&policy);\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("settings.toml"),
        "handler = \"reconnect_network\"\n",
    )
    .unwrap();
    let out = find(&ws, &json!({"query":"reconnect network"})).unwrap();
    assert!(out.contains("via=lexical"), "{out}");
    assert!(out.contains("Related evidence"), "{out}");
    assert!(out.contains("NOT edit witnesses"), "{out}");
    assert!(
        out.contains("call-site caller.rs:1-3 provenance=lexical"),
        "{out}"
    );
    assert!(
        out.contains("type-definition retry.rs:1-3 provenance=syntax"),
        "{out}"
    );
    assert!(
        out.contains("config-reference settings.toml:1-1 provenance=lexical"),
        "{out}"
    );
    assert!(out.len() <= MAX_OUTPUT + 6 * 1024, "{}", out.len());
    // Opting out restores the plain hit rows; exact mode omits the bundle by default.
    let plain = find(&ws, &json!({"query":"reconnect network","related":false})).unwrap();
    assert!(!plain.contains("Related evidence"), "{plain}");
    let exact = find(&ws, &json!({"query":"reconnect_network(","mode":"exact"})).unwrap();
    assert!(exact.contains("via=exact-text") && !exact.contains("Related evidence"));
    assert!(find(&ws, &json!({"query":"reconnect","related":"yes"})).is_err());
}

#[test]
fn related_bundle_never_names_protected_or_changed_sources() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::write(
        private.join("secret.rs"),
        "fn x() { reconnect_network(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("retry.rs"),
        "pub fn reconnect_network() {\n    retry();\n}\n",
    )
    .unwrap();
    let ws = ws.with_protected_paths(&[private]);
    let out = find(&ws, &json!({"query":"reconnect network"})).unwrap();
    assert!(!out.contains("secret.rs"), "{out}");
    assert!(out.contains("none found in scope"), "{out}");
}

#[test]
fn related_bundle_never_surfaces_environment_or_credential_files() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("retry.rs"),
        "pub fn reconnect_network() {\n    retry();\n}\n",
    )
    .unwrap();
    fs::write(dir.path().join(".env"), "reconnect_network=hunter2\n").unwrap();
    fs::write(dir.path().join("prod.env"), "reconnect_network=hunter3\n").unwrap();
    fs::write(
        dir.path().join("secrets.toml"),
        "reconnect_network = \"hunter4\"\n",
    )
    .unwrap();
    fs::write(dir.path().join("server.pem"), "reconnect_network\n").unwrap();
    fs::write(
        dir.path().join("app.toml"),
        "handler = \"reconnect_network\"\n",
    )
    .unwrap();
    let out = find(&ws, &json!({"query":"reconnect network"})).unwrap();
    let related = out.split("Related evidence").nth(1).unwrap();
    for hidden in ["hunter", ".env", "prod.env", "secrets.toml", "server.pem"] {
        assert!(!related.contains(hidden), "{hidden}: {related}");
    }
    assert!(related.contains("config-reference app.toml"), "{related}");
}

/// Static dependency-ranking ablation, deliberately not production policy.
#[test]
#[ignore = "offline retrieval measurement; run with --ignored --nocapture --test-threads=1"]
fn retrieval_measurement_graph() {
    use std::time::Instant;
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../../examples/benchmarks/retrieval-fixture.json"
    ))
    .unwrap();
    let (dir, ws) = crate::tools::testkit::workspace();
    for (name, source) in fixture["files"].as_object().unwrap() {
        fs::write(dir.path().join(name), source.as_str().unwrap()).unwrap();
    }
    for case in fixture["queries"].as_array().unwrap() {
        let args = json!({"query":case["query"], "limit":100, "related":false});
        let gold = case["gold"].as_array().unwrap();
        for graph in [false, true] {
            let mut elapsed = Vec::new();
            let mut last = Value::Null;
            for _ in 0..20 {
                let started = Instant::now();
                let retrieved = retrieve(&ws, &args, false).unwrap();
                // One best lexical span per file, identical candidates in both arms.
                let mut candidates = BTreeMap::new();
                for hit in &retrieved.results {
                    candidates.entry(hit.path.clone()).or_insert(hit);
                }
                let sum = candidates.values().map(|h| h.score).sum::<usize>() as f64;
                assert!(sum > 0.0);
                let seeds: BTreeMap<_, _> = candidates
                    .iter()
                    .map(|(path, hit)| (path.clone(), hit.score as f64 / sum))
                    .collect();
                let mut scores = seeds.clone();
                if graph {
                    // Aider-inspired propagation, NOT Aider or resolved Rust symbols.
                    // Rebuild explicit crate::<file>:: heuristic edges from current source.
                    let edges: BTreeMap<_, Vec<_>> = candidates
                        .keys()
                        .map(|path| {
                            let source = fs::read_to_string(dir.path().join(path)).unwrap();
                            let targets = candidates
                                .keys()
                                .filter(|target| {
                                    source.contains(&format!(
                                        "crate::{}::",
                                        target.trim_end_matches(".rs")
                                    ))
                                })
                                .cloned()
                                .collect();
                            (path.clone(), targets)
                        })
                        .collect();
                    for _ in 0..8 {
                        let mut next: BTreeMap<_, _> = seeds
                            .iter()
                            .map(|(path, score)| (path.clone(), score * 0.15))
                            .collect();
                        for (path, targets) in &edges {
                            if targets.is_empty() {
                                *next.get_mut(path).unwrap() += 0.85 * scores[path];
                            } else {
                                for target in targets {
                                    *next.get_mut(target).unwrap() +=
                                        0.85 * scores[path] / targets.len() as f64;
                                }
                            }
                        }
                        scores = next;
                    }
                }
                let mut ranked: Vec<_> = candidates.values().copied().collect();
                ranked.sort_by(|a, b| {
                    scores[&b.path]
                        .total_cmp(&scores[&a.path])
                        .then(a.path.cmp(&b.path))
                });
                let rows: Vec<_> = ranked
                    .into_iter()
                    .take(2)
                    .map(|hit| {
                        assert!(hit.verify_current(), "stale evidence must be withheld");
                        json!({"path":hit.path,"start":hit.start,"end":hit.end,
                        "source_sha256":hit.source_sha256,"snippet":hit.snippet,
                        "via":if graph { "experimental-static-dependency" } else { "lexical" }})
                    })
                    .collect();
                elapsed.push(started.elapsed().as_secs_f64() * 1000.0);
                let found = gold
                    .iter()
                    .filter(|path| rows.iter().any(|r| &r["path"] == *path))
                    .count();
                let bytes = serde_json::to_vec(&rows).unwrap().len();
                last = json!({"query":case["query"],"ranking":if graph {"graph"} else {"lexical"},
                    "recall_at_2":found as f64 / gold.len() as f64,
                    "all_gold_at_2":found == gold.len(),"bytes":bytes,
                    "estimated_tokens":bytes.div_ceil(4),"rows":rows});
            }
            elapsed.sort_by(f64::total_cmp);
            last["p50_ms"] = json!(elapsed[9]);
            last["p95_ms"] = json!(elapsed[18]);
            println!("RETRIEVAL_GRAPH {last}");
        }
    }
    let stale = retrieve(&ws, &json!({"query":"backoff_delay"}), false).unwrap();
    assert!(stale.results.iter().any(|h| h.path == "clock.rs"));
    fs::write(dir.path().join("clock.rs"), "pub fn changed() {}\n").unwrap();
    assert!(
        stale
            .results
            .iter()
            .filter(|h| h.path == "clock.rs")
            .all(|h| !h.verify_current())
    );
}
