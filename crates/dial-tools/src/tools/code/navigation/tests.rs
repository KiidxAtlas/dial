use super::*;

#[derive(Default)]
struct RetrievalWork {
    declarations: Vec<PathBuf>,
}
thread_local! {
    static RETRIEVAL_WORK: std::cell::RefCell<Option<RetrievalWork>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn record_declaration_parse(path: &Path) {
    RETRIEVAL_WORK.with_borrow_mut(|work| {
        if let Some(work) = work {
            work.declarations.push(path.to_owned());
        }
    });
}
fn capture_work<T>(run: impl FnOnce() -> T) -> (T, RetrievalWork) {
    RETRIEVAL_WORK.set(Some(RetrievalWork::default()));
    let result = run();
    (result, RETRIEVAL_WORK.take().unwrap())
}

#[test]
fn lexical_retrieval_parses_only_matching_sources_and_rechecks_nonmatches() {
    let (dir, ws) = crate::tools::testkit::workspace();
    for index in 0..16 {
        fs::write(
            dir.path().join(format!("unrelated_{index}.rs")),
            "pub fn unrelated() {}\n",
        )
        .unwrap();
    }
    fs::write(
        dir.path().join("network.rs"),
        "pub fn reconnect_network() {}\n",
    )
    .unwrap();
    let args = json!({"query":"reconnect network"});
    let (first, cold) = capture_work(|| retrieve(&ws, &args, false).unwrap());
    assert!(!first.results.is_empty());
    assert!(first.results.iter().all(|hit| hit.path == "network.rs"));
    assert_eq!(cold.declarations, [dir.path().join("network.rs")]);
    let (again, _) = capture_work(|| retrieve(&ws, &args, false).unwrap());
    assert_eq!(
        first
            .results
            .iter()
            .map(|hit| (&hit.path, hit.start, hit.end, hit.score))
            .collect::<Vec<_>>(),
        again
            .results
            .iter()
            .map(|hit| (&hit.path, hit.start, hit.end, hit.score))
            .collect::<Vec<_>>()
    );
    fs::write(
        dir.path().join("unrelated_0.rs"),
        "pub fn reconnect_network_again() {}\n",
    )
    .unwrap();
    let (refreshed, changed) = capture_work(|| retrieve(&ws, &args, false).unwrap());
    assert!(
        refreshed
            .results
            .iter()
            .any(|hit| hit.path == "unrelated_0.rs")
    );
    assert!(
        changed
            .declarations
            .contains(&dir.path().join("unrelated_0.rs"))
    );
    let other_query = retrieve(&ws, &json!({"query":"unrelated"}), false).unwrap();
    assert!(
        other_query
            .results
            .iter()
            .any(|hit| hit.path == "unrelated_1.rs")
    );
}

#[test]
fn lexical_prefilter_keeps_literal_only_and_multiline_phrase_matches() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("literal.txt"), "a\r\nb\r\n").unwrap();
    for args in [
        json!({"query":"unmatched", "keywords":["a\nb"]}),
        json!({"query":"\"a\nb\""}),
    ] {
        let retrieval = retrieve(&ws, &args, false).unwrap();
        assert!(
            retrieval
                .results
                .iter()
                .any(|hit| hit.path == "literal.txt")
        );
    }
    // Semantic retrieval must retain lexically unrelated candidates for reranking.
    assert!(
        !retrieve(&ws, &json!({"query":"unmatched"}), true)
            .unwrap()
            .results
            .is_empty()
    );
}

#[test]
fn related_reference_quota_stops_unusable_syntax_work() {
    use sha2::{Digest, Sha256};
    let (dir, ws) = crate::tools::testkit::workspace();
    let source = "pub fn reconnect_network(_session: Session) {}\n";
    fs::write(dir.path().join("network.rs"), source).unwrap();
    for index in 0..48 {
        fs::write(
            dir.path().join(format!("caller_{index:02}.rs")),
            format!("pub fn caller_{index}() {{ reconnect_network(); }}\n"),
        )
        .unwrap();
    }
    fs::write(dir.path().join("z_type.rs"), "pub struct Session;\n").unwrap();
    let targets = [related::Target {
        path: "network.rs".into(),
        start: 1,
        end: 1,
        sha256: format!("{:x}", Sha256::digest(source)),
    }];
    let (out, work) = capture_work(|| related::render(&ws, ws.root(), &targets, 6 * 1024).text);
    assert!(out.contains("call-site caller_00.rs"), "{out}");
    assert!(out.contains("call-site caller_03.rs"), "{out}");
    assert!(!out.contains("call-site caller_04.rs"), "{out}");
    assert!(out.contains("type-definition z_type.rs"), "{out}");
    for index in 40..48 {
        assert!(
            !work
                .declarations
                .contains(&dir.path().join(format!("caller_{index:02}.rs"))),
            "reference quota is full; this source cannot contribute a type or config row"
        );
    }
}

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
fn find_read_only_roots_admit_sources_but_exclude_private_paths() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let extra = tempfile::tempdir().unwrap();
    let root = extra.path().canonicalize().unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(
        root.join("nested/public.rs"),
        "pub fn readable_canary() {}\n",
    )
    .unwrap();
    let private = root.join("private");
    fs::create_dir(&private).unwrap();
    fs::write(
        private.join("secret.rs"),
        "pub fn readable_canary_secret() {}\n",
    )
    .unwrap();
    let args = json!({"query": "readable_canary", "path": root, "mode": "exact"});
    assert!(find(&ws, &args).is_err());
    let ws = ws
        .with_protected_paths(&[private])
        .with_read_only_roots(std::slice::from_ref(&root))
        .unwrap();
    let text = find(&ws, &args).unwrap();
    assert!(text.contains("public.rs"), "{text}");
    assert!(!text.contains("secret.rs"), "{text}");
    assert!(
        ws.resolve(root.join("nested/public.rs").to_str().unwrap())
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
fn find_excerpts_witness_only_returned_rows_and_survive_resume() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
    let source = format!("def compute_total():\n{}", "    value = 1\n".repeat(44));
    fs::write(dir.path().join("calc.py"), &source).unwrap();
    let args = json!({"query":"compute_total", "related":false});
    let result = find(&ws, &args).unwrap();
    let excerpts = result.split_once(SOURCE_SECTION).unwrap().1;
    assert!(excerpts.contains("40:    value = 1"), "{excerpts}");
    assert!(!excerpts.contains("41:    value = 1"), "{excerpts}");
    let tag = crate::tools::edit::hashline::tag_of(&source);
    let unseen = format!("[calc.py#{tag}]\nPUT 41.=41:\n+    value = 2\n");
    let resumed = Workspace::new(dir.path()).unwrap();
    crate::tools::edit::hashline::rehydrate(&resumed, [("find", &args, result.as_str())]);
    let refused = crate::tools::edit::hashline::edit(&resumed, &unseen).unwrap_err();
    assert!(refused.contains("have not been shown"), "{refused}");
    let edit = format!("[calc.py#{tag}]\nPUT 2.=2:\n+    value = 2\n");
    crate::tools::edit::hashline::edit(&resumed, &edit).unwrap();
    assert!(
        fs::read_to_string(dir.path().join("calc.py"))
            .unwrap()
            .starts_with("def compute_total():\n    value = 2\n")
    );
}

#[test]
fn stale_find_excerpt_does_not_restore_edit_witnesses() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
    let original = "def compute_total():\n    return 1\n";
    fs::write(dir.path().join("calc.py"), original).unwrap();
    let args = json!({"query":"compute_total", "related":false});
    let result = find(&ws, &args).unwrap();
    fs::write(
        dir.path().join("calc.py"),
        "def compute_total():\n    return 3\n",
    )
    .unwrap();
    let resumed = Workspace::new(dir.path()).unwrap();
    crate::tools::edit::hashline::rehydrate(&resumed, [("find", &args, result.as_str())]);
    let tag = crate::tools::edit::hashline::tag_of(original);
    let error = crate::tools::edit::hashline::edit(
        &resumed,
        &format!("[calc.py#{tag}]\nPUT 2.=2:\n+    return 2\n"),
    )
    .unwrap_err();
    assert!(error.contains("unknown snapshot tag"), "{error}");
    assert_eq!(
        fs::read_to_string(dir.path().join("calc.py")).unwrap(),
        "def compute_total():\n    return 3\n"
    );
}

#[test]
fn automatic_excerpt_does_not_disclose_or_witness_screened_source() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
    let source = "def compute_total():\n    value = 1\n    value += 2\n    value += 3\n    value += 4\n    value += 5\n    value += 6\n    key = 'sk-abcdefghijklmnopqrstuvwxyz123456'\n    return value\n";
    fs::write(dir.path().join("calc.py"), source).unwrap();
    let result = find(&ws, &json!({"query":"compute_total", "related":false})).unwrap();
    let excerpts = result
        .split_once(SOURCE_SECTION)
        .map_or("", |(_, text)| text);
    assert!(!excerpts.contains("abcdefghijklmnopqrstuvwxyz"));
    let tag = crate::tools::edit::hashline::tag_of(source);
    let edit = format!("[calc.py#{tag}]\nPUT 2.=2:\n+    value = 2\n");
    assert!(crate::tools::edit::hashline::edit(&ws, &edit).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("calc.py")).unwrap(),
        source
    );
}

fn discovery_fixture(root: &Path) {
    fs::write(
        root.join("network.rs"),
        "pub fn reconnect_network() {\n    // anchor behavior\n    retry();\n}\n",
    )
    .unwrap();
    for index in 0..6 {
        fs::write(
            root.join(format!("caller_{index}.rs")),
            format!("fn caller_{index}() {{ reconnect_network(); }}\n"),
        )
        .unwrap();
    }
    fs::write(
        root.join("test_network.py"),
        format!(
            "def verify_network():\n    reconnect_network()\n{}",
            "    value = 1\n".repeat(44)
        ),
    )
    .unwrap();
    fs::write(
        root.join("settings.json"),
        "{\"handler\": \"reconnect_network\"}\n",
    )
    .unwrap();
}

#[test]
fn related_discovery_previews_test_and_supported_config_with_resume_witnesses() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
    discovery_fixture(dir.path());
    let args = json!({"query":"\"anchor behavior\"", "limit":1});
    let result = find(&ws, &args).unwrap();
    let (ranked, excerpts) = result.split_once(SOURCE_SECTION).unwrap();
    assert!(ranked.contains("network.rs:1-4 score="), "{ranked}");
    assert!(!ranked.contains("test_network.py:1-46 score="), "{ranked}");
    assert!(ranked.contains("provenance=lexical"), "{ranked}");
    assert_eq!(
        source_paths(&ws, &result),
        ["network.rs", "test_network.py", "settings.json"]
    );
    assert!(excerpts.contains("2:    reconnect_network()"), "{excerpts}");
    assert!(
        excerpts.contains("1:{\"handler\": \"reconnect_network\"}"),
        "{excerpts}"
    );
    assert!(excerpts.contains("40:    value = 1"), "{excerpts}");
    assert!(!excerpts.contains("41:    value = 1"), "{excerpts}");
    assert!(excerpts.len() <= SOURCE_BYTES);
    // The same files may also be ranked hits; each file still receives only one excerpt.
    let overlapping = find(&ws, &json!({"query":"reconnect_network", "limit":100})).unwrap();
    let paths = source_paths(&ws, &overlapping);
    assert_eq!(paths.len(), 3);
    assert_eq!(paths.iter().collect::<BTreeSet<_>>().len(), paths.len());
    let resumed = Workspace::new(dir.path()).unwrap();
    crate::tools::edit::hashline::rehydrate(&resumed, [("find", &args, result.as_str())]);
    let test_source = fs::read_to_string(dir.path().join("test_network.py")).unwrap();
    let tag = crate::tools::edit::hashline::tag_of(&test_source);
    let error = crate::tools::edit::hashline::edit(
        &resumed,
        &format!("[test_network.py#{tag}]\nPUT 41.=41:\n+    value = 2\n"),
    )
    .unwrap_err();
    assert!(error.contains("have not been shown"), "{error}");
    for (path, line, replacement) in [
        ("test_network.py", 2, "    reconnect_network(2)"),
        ("settings.json", 1, "{\"handler\": \"other\"}"),
    ] {
        let source = fs::read_to_string(dir.path().join(path)).unwrap();
        let tag = crate::tools::edit::hashline::tag_of(&source);
        crate::tools::edit::hashline::edit(
            &resumed,
            &format!("[{path}#{tag}]\nPUT {line}.={line}:\n+{replacement}\n"),
        )
        .unwrap();
        assert!(
            fs::read_to_string(dir.path().join(path))
                .unwrap()
                .contains(replacement)
        );
    }
    // Disabling related discovery, or exact mode's default, keeps the old ranked-hit previews.
    for args in [
        json!({"query":"\"anchor behavior\"", "limit":1, "related":false}),
        json!({"query":"anchor behavior", "limit":1, "mode":"exact"}),
    ] {
        let plain = find(&ws, &args).unwrap();
        assert_eq!(source_paths(&ws, &plain), ["network.rs"]);
    }
}

#[test]
fn related_preview_witnesses_refuse_stale_screened_sensitive_and_unsupported_sources() {
    for path in ["test_network.py", "settings.json"] {
        let (dir, ws) = crate::tools::testkit::workspace();
        let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
        discovery_fixture(dir.path());
        let args = json!({"query":"\"anchor behavior\"", "limit":1});
        let source = fs::read_to_string(dir.path().join(path)).unwrap();
        let result = find(&ws, &args).unwrap();
        fs::write(dir.path().join(path), format!("{source}\n")).unwrap();
        let resumed = Workspace::new(dir.path()).unwrap();
        crate::tools::edit::hashline::rehydrate(&resumed, [("find", &args, result.as_str())]);
        let tag = crate::tools::edit::hashline::tag_of(&source);
        let error = crate::tools::edit::hashline::edit(
            &resumed,
            &format!("[{path}#{tag}]\nPUT 1.=1:\n+changed\n"),
        )
        .unwrap_err();
        assert!(error.contains("unknown snapshot tag"), "{path}: {error}");

        // Screening checks the whole snapshot, including a secret beyond the excerpt window.
        let screened = format!(
            "{source}{}\n# key = 'sk-abcdefghijklmnopqrstuvwxyz123456'\n",
            "\n".repeat(45)
        );
        fs::write(dir.path().join(path), &screened).unwrap();
        fs::write(
            dir.path().join("settings.cfg"),
            "handler = reconnect_network\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("secrets.json"),
            "{\"handler\":\"reconnect_network\"}\n",
        )
        .unwrap();
        let result = find(&ws, &args).unwrap();
        let paths = source_paths(&ws, &result);
        for omitted in [path, "settings.cfg", "secrets.json"] {
            assert!(!paths.iter().any(|shown| shown == omitted), "{paths:?}");
        }
        let tag = crate::tools::edit::hashline::tag_of(&screened);
        assert!(
            crate::tools::edit::hashline::edit(
                &ws,
                &format!("[{path}#{tag}]\nPUT 1.=1:\n+changed\n")
            )
            .is_err()
        );
    }
}

#[test]
fn related_preview_truncated_rows_are_not_edit_witnesses() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(crate::config::types::EditMode::Hashline);
    discovery_fixture(dir.path());
    let source = format!(
        "def verify_network():\n    reconnect_network()\n    # {}\n    value = 1\n",
        "x".repeat(2100)
    );
    fs::write(dir.path().join("test_network.py"), &source).unwrap();
    let args = json!({"query":"\"anchor behavior\"", "limit":1});
    let result = find(&ws, &args).unwrap();
    let excerpts = result.split_once(SOURCE_SECTION).unwrap().1;
    assert!(excerpts.contains("[line truncated]"), "{excerpts}");
    let tag = crate::tools::edit::hashline::tag_of(&source);
    for resumed in [false, true] {
        let restored = Workspace::new(dir.path()).unwrap();
        let target = if resumed {
            crate::tools::edit::hashline::rehydrate(&restored, [("find", &args, result.as_str())]);
            &restored
        } else {
            &ws
        };
        let error = crate::tools::edit::hashline::edit(
            target,
            &format!("[test_network.py#{tag}]\nPUT 3.=3:\n+    value = 2\n"),
        )
        .unwrap_err();
        assert!(error.contains("have not been shown"), "{error}");
    }
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
        embedding: Arc::new(EmbeddingSlot::unavailable("not configured".into())),
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
fn related_maps_literal_contracts_across_layers_without_extra_searches() {
    for source in [
        "impl Tool for Memory {\n    // memory contract anchor\n    fn name(&self) -> &str { \"recall\" }\n}\n",
        "// memory contract anchor\nfn migrate_memory_tool_names() { const REPLACEMENTS: &[&str] = &[\"recall\"]; }\n",
    ] {
        let (dir, ws) = crate::tools::testkit::workspace();
        fs::write(dir.path().join("memory.rs"), source).unwrap();
        for layer in [
            "a_callers",
            "config",
            "storage",
            "tools",
            "agent",
            "history",
        ] {
            fs::create_dir(dir.path().join(layer)).unwrap();
            for index in 0..10 {
                fs::write(
                    dir.path().join(format!("{layer}/use_{index:02}.rs")),
                    "fn dispatch() { route(\"recall\"); }\n",
                )
                .unwrap();
            }
        }
        fs::write(
            dir.path().join("noise.rs"),
            "fn recall() {} // recall is an ordinary identifier here\n",
        )
        .unwrap();
        for index in 0..20 {
            let nested = dir.path().join(format!("a_callers/nested_{index:02}"));
            fs::create_dir(&nested).unwrap();
            fs::write(
                nested.join("use.rs"),
                "fn route() { dispatch(\"recall\"); }\n",
            )
            .unwrap();
        }
        fs::write(
            dir.path().join("config/settings.toml"),
            "tool = \"recall\"\n",
        )
        .unwrap();
        let args = json!({"query":"\"memory contract anchor\"", "limit":1});
        let out = find(&ws, &args).unwrap();
        let related = out
            .split("Related evidence")
            .nth(1)
            .unwrap()
            .split(SOURCE_SECTION)
            .next()
            .unwrap();
        assert!(related.contains("contract names: recall"), "{out}");
        for layer in [
            "a_callers",
            "config",
            "storage",
            "tools",
            "agent",
            "history",
        ] {
            let count = related
                .matches(&format!("contract-reference {layer}/"))
                .count();
            assert_eq!(count, if layer == "a_callers" { 4 } else { 1 }, "{out}");
        }
        assert!(related.contains("not confirmed owners"), "{out}");
        assert!(!related.contains("contract-reference noise.rs"), "{out}");
        assert!(related.contains("scan truncated=false"), "{out}");
        let scoped = find(
            &ws,
            &json!({"query":"\"memory contract anchor\"", "path":"memory.rs"}),
        )
        .unwrap();
        assert!(!scoped.contains("contract-reference"), "{scoped}");
        let disabled = find(
            &ws,
            &json!({"query":"\"memory contract anchor\"", "related":false}),
        )
        .unwrap();
        assert!(!disabled.contains("contract-reference"), "{disabled}");
        let ws = ws.with_protected_paths(&[dir.path().join("history")]);
        let protected = find(&ws, &args).unwrap();
        assert!(!protected.contains("history/"), "{protected}");
    }
}

#[test]
fn contract_discovery_does_not_invent_computed_names() {
    for body in [
        "format!(\"recall\")",
        "lookup(\"recall\")",
        "if yes { \"recall\" } else { \"retain\" }",
    ] {
        let text = format!("impl Tool for Memory {{ fn name(&self) -> &str {{ {body} }} }}");
        assert!(
            related::contract_names(Path::new("memory.rs"), &text, 1, 1).is_empty(),
            "{body}"
        );
    }
}

#[test]
fn contract_discovery_maps_name_registries_and_bounds_expansion() {
    let source = "pub const TOOL_NAMES: &[&str] = &[\"retain\", \"recall\", computed(\"not_a_literal_name\")];";
    assert_eq!(
        related::contract_names(Path::new("memory.rs"), source, 1, 1),
        ["retain", "recall"]
    );
    let source = format!(
        "const TOOL_NAMES: &[&str] = &[{}];",
        (0..20)
            .map(|i| format!("\"tool_{i}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        related::contract_names(Path::new("memory.rs"), &source, 1, 1).len(),
        8
    );
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
