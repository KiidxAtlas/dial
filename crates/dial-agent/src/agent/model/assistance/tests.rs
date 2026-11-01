use super::*;
fn candidates(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(name, description)| ((*name).into(), (*description).into()))
        .collect()
}

#[test]
fn exact_name_is_authoritative_even_when_other_descriptions_match() {
    let matches = discover(
        "  LSP  ",
        candidates(&[
            ("lsp", "Inspect definitions and references"),
            ("other", "LSP compatible symbol search"),
        ]),
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0, "lsp");
    assert!(matches[0].2);
}

#[test]
fn installed_mcp_inventory_is_discoverable_without_server_domain_words() {
    let config = Config::default();
    let loaded = crate::tools::integrations::mcp::McpLoad::default();
    let inventory = loaded.inventory_tool(&config);
    let description = inventory.schema()["function"]["description"]
        .as_str()
        .unwrap()
        .to_owned();
    let matches = discover(
        "list installed MCP servers",
        vec![
            (inventory.name().to_owned(), description),
            (
                "mcp__memory__search_nodes".into(),
                "memory: Search nodes".into(),
            ),
        ],
    );
    assert_eq!(matches[0].0, "mcp_servers");
    assert!(matches[0].2);
    assert!(
        !matches
            .iter()
            .any(|entry| entry.0 == "mcp__memory__search_nodes")
    );
}

#[test]
fn natural_capability_queries_ignore_filler_not_domain_evidence() {
    let matches = discover(
        "I need a tool to inspect symbols and definitions please",
        candidates(&[
            (
                "lsp",
                "Inspect symbols, definitions and references in source",
            ),
            ("web_search", "Search public web sources"),
            ("read", "Read source files"),
        ]),
    );
    assert_eq!(
        matches
            .iter()
            .map(|entry| entry.0.as_str())
            .collect::<Vec<_>>(),
        ["lsp"]
    );
    assert!(matches[0].2);
}

#[test]
fn strongest_name_evidence_wins_and_weaker_tier_is_not_activated() {
    let matches = discover(
        "web search",
        candidates(&[
            ("web_search", "Search public sources"),
            ("browser", "Browse and search web pages"),
            ("read", "Read files"),
        ]),
    );
    assert_eq!(matches[0].0, "web_search");
    assert!(matches[0].2);
    assert_eq!(matches[1].0, "browser");
    assert!(!matches[1].2);
    assert!(!matches.iter().any(|entry| entry.0 == "read"));
}

#[test]
fn discovery_is_bounded_and_ties_are_independent_of_catalog_order() {
    let catalog: Vec<_> = (0..40)
        .map(|index| (format!("cap_{index:02}"), "Inspect widgets".into()))
        .collect();
    let first = discover("widgets", catalog.clone());
    let mut reversed = catalog;
    reversed.reverse();
    assert_eq!(first, discover("widgets", reversed));
    assert_eq!(first.len(), 8);
    assert_eq!(first.iter().filter(|entry| entry.2).count(), 3);
    assert_eq!(first[0].0, "cap_00");
}

#[test]
fn readiness_breaks_relevance_ties_before_caps_without_promoting_weaker_matches() {
    let catalog: Vec<_> = (0..40)
        .map(|index| (format!("cap_{index:02}"), "Inspect widgets".into()))
        .collect();
    let ready_last = |name: &str| u8::from(name != "cap_39");
    let matches = discover_with_priority("widgets", catalog.clone(), &ready_last);
    assert_eq!(matches[0].0, "cap_39");
    assert_eq!(matches.len(), 8);
    assert_eq!(matches.iter().filter(|entry| entry.2).count(), 3);
    assert_eq!(discover("widgets", catalog)[0].0, "cap_00");

    let matches = discover_with_priority(
        "web search",
        candidates(&[
            ("web_search", "Search public sources"),
            ("browser", "Browse and search web pages"),
        ]),
        &|name| u8::from(name != "browser"),
    );
    assert_eq!(matches[0].0, "web_search");
    assert!(matches[0].2);
    assert!(
        !matches[1].2,
        "readiness cannot promote a weaker capability match"
    );
}

#[test]
fn empty_filler_and_unknown_queries_activate_nothing() {
    for query in ["", "  ", "I need a tool please", "quuxflorb"] {
        assert!(discover(query, candidates(&[("read", "Read files")])).is_empty());
    }
}

#[test]
fn discovery_cannot_recover_profile_excluded_candidates() {
    let eligible = candidates(&[("read", "Read files")]);
    assert!(discover("web_search", eligible.clone()).is_empty());
    assert!(discover("search the public web", eligible).is_empty());
    assert!(discover("read", Vec::new()).is_empty());
}

#[test]
fn task_and_skill_survive_compaction_without_new_enabled_state() {
    let loaded = HashSet::new();
    let mut names = vec![
        "find".into(),
        "skill".into(),
        "task".into(),
        "unrelated".into(),
    ];
    compact(&mut names, &loaded, 2, &BTreeMap::new());
    assert_eq!(names, ["find", "skill", "task"]);
    assert!(loaded.is_empty());
}

#[test]
fn zero_budget_does_not_truncate_eligible_configured_tools() {
    let mut names = vec!["lsp".into(), "read".into(), "browser".into()];
    compact(&mut names, &HashSet::new(), 0, &BTreeMap::new());
    assert_eq!(names, ["browser", "lsp", "read"]);
}

#[test]
fn explicit_tools_and_core_survive_budget() {
    let mut names = vec![
        "read".into(),
        "finish".into(),
        "tool_load".into(),
        "lsp".into(),
        "unused".into(),
    ];
    compact(
        &mut names,
        &HashSet::from(["lsp".into()]),
        2,
        &BTreeMap::new(),
    );
    assert!(names.contains(&"lsp".into()));
    assert!(names.contains(&"tool_load".into()));
    assert!(!names.contains(&"unused".into()));
}
#[test]
fn check_failure_invalidates_capture() {
    let mut state = State::default();
    state.begin("repair");
    state.check("cargo test", true);
    assert_eq!(state.checks.len(), 1);
    state.check("cargo test", false);
    assert!(state.checks.is_empty());
}

#[test]
fn singular_and_plural_capabilities_share_evidence_without_activating_unrelated_tools() {
    let catalog = candidates(&[
        ("lsp", "Inspect symbols definitions references"),
        ("browser", "Capture screenshots of rendered pages"),
        ("read", "Read files"),
        ("mcp_servers", "Inspect installed servers"),
    ]);
    for (query, expected) in [
        ("symbol definition", "lsp"),
        ("rendered page screenshot", "browser"),
        ("read file", "read"),
        ("installed server", "mcp_servers"),
    ] {
        let matches = discover(query, catalog.clone());
        assert_eq!(matches[0].0, expected, "{query}");
        assert!(matches[0].2);
        assert_eq!(matches.iter().filter(|entry| entry.2).count(), 1, "{query}");
    }
    assert!(discover("symbol definitions", candidates(&[("read", "Read files")])).is_empty());
    let exact = discover(
        "references",
        candidates(&[("references", "Custom tool"), ("lsp", "Find references")]),
    );
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].0, "references");
}

#[test]
fn research_corpus_activates_web_search_and_never_incidental_nuxt_matches() {
    let catalog = candidates(&[
        ("web_search", "Search the public web for sources"),
        ("web_fetch", "Fetch a web page by URL"),
        ("mcp__nuxt__list-blog-posts", "nuxt: List blog posts"),
        (
            "mcp__nuxt__list-deploy-providers",
            "nuxt: List deployment providers",
        ),
    ]);
    for query in [
        "web search research fetch",
        "web search",
        "Search the web for Gemma 4 E4B versus Qwen 3.5 9B",
    ] {
        let activated: Vec<String> = discover(query, catalog.clone())
            .into_iter()
            .filter(|entry| entry.2)
            .map(|entry| entry.0)
            .collect();
        assert!(
            activated.iter().any(|name| name == "web_search"),
            "{query}: {activated:?}"
        );
        assert!(
            activated
                .iter()
                .all(|name| !name.starts_with("mcp__nuxt__")),
            "{query}: {activated:?}"
        );
    }
}

#[test]
fn computer_is_plan_eligible_and_plan_authorization_admits_only_read_only_actions() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    with_agent("integrations=[\"computer\"]", |agent| {
        agent.plan_mode = true;
        assert!(agent.tool_eligible("computer"));
        let call = |arguments: Value| ToolCall {
            id: "computer-plan".into(),
            name: "computer".into(),
            arguments,
            malformed_arguments: false,
        };
        // Only the read-only screenshot chain is admitted; no capture runs during authorization.
        let screenshot = call(json!({"action":"call","chain":[{"method":"screenshot","args":[]}]}));
        assert!(agent.authorize_call(&screenshot, &mut NullUi).is_ok());
        for arguments in [
            json!({"action":"call","chain":[{"method":"click","args":[1,2]}]}),
            json!({"action":"run","code":"1"}),
            json!({"action":"run","code":"1","read_only":true}),
        ] {
            assert!(
                matches!(
                    agent.authorize_call(&call(arguments.clone()), &mut NullUi),
                    Err(crate::agent::safety::approval::Denial::ReadOnly)
                ),
                "{arguments}"
            );
        }
    });
}

#[test]
fn recipe_credit_follows_only_the_observed_check_verdict() {
    use crate::runtime::execution::{Completeness, OutputIntegrity, ProcessOutcome};
    use crate::runtime::tool_output::ToolOutput;
    use crate::tools::surface::outcome::Effects;
    let check = FocusedCheck {
        name: "unit".into(),
        command: "cargo test".into(),
        timeout_secs: 1,
    };
    let exited = |code: Option<i32>| ProcessOutcome {
        started: true,
        exit_code: code,
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    };
    let pass = "running 2 tests\ntest result: ok. 2 passed; 0 failed\n";
    let credit = |out: &ToolOutput, cancelled: bool| {
        recipe_check_earns_credit(&check, None, out, cancelled, None)
    };

    // Printed success without an observed process is prose, not evidence.
    assert!(!credit(
        &ToolOutput::ok(format!("{pass}[exit code: 0]\n")),
        false
    ));
    // Contradictory process outcomes: printed success never overrides the observed exit.
    let printed_pass = ToolOutput::ok(format!("{pass}[exit code: 0]\n"));
    assert!(!credit(
        &printed_pass.clone().with_process(exited(Some(1))),
        false
    ));
    assert!(!credit(
        &ToolOutput::err(pass).with_process(exited(Some(0))),
        false
    ));
    assert!(!credit(
        &printed_pass.clone().with_process(exited(None)),
        false
    ));
    // Empty suites and cancellation never earn credit, even with a zero exit.
    let empty =
        ToolOutput::ok("Ran 0 tests\n[exit code: 0]\n".into()).with_process(exited(Some(0)));
    assert!(!credit(&empty, false));
    assert!(!credit(
        &printed_pass.clone().with_process(exited(Some(0))),
        true
    ));
    // Unknown effects block credit even when the process exited cleanly.
    let mut unknown = printed_pass.clone().with_process(exited(Some(0)));
    unknown.execution.effects = Effects::Unknown;
    assert!(!credit(&unknown, false));
    // Retained or truncated evidence is unavailable, not a pass.
    let mut clipped = printed_pass.with_process(exited(Some(0)));
    clipped.execution.output = Some(OutputIntegrity {
        capture: Completeness::Partial,
        retention: Completeness::Complete,
        presentation: Completeness::Complete,
        original_bytes: None,
        retained_bytes: None,
        omitted_bytes: None,
        presented_bytes: None,
        artifact: None,
    });
    assert!(!credit(&clipped, false));
    // Normal observed pass with non-empty tests earns credit.
    assert!(credit(
        &ToolOutput::ok(pass.into()).with_process(exited(Some(0))),
        false
    ));
}
