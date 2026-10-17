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
