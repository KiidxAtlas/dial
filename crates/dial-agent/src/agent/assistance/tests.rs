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
    let loaded = crate::tools::mcp::McpLoad::default();
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
