use super::capability_prose;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn capability_prose_includes_guidance_when_every_named_tool_is_present() {
    let offered = names(&[
        "tool_search",
        "working_state",
        "artifact_read",
        "task",
        "web_search",
        "web_fetch",
    ]);
    assert_eq!(
        capability_prose(&offered, &[]),
        "\ntool_search enables up to three strongest matches for the next model round. Prefer relevant eligible native or MCP specialists already offered over generic shell/web substitutes. If a needed capability is missing, search by the task and domain, then use the returned next-round schemas. Do not rediscover offered tools or make calls merely to exercise available tools; answer directly when no tool is needed. Discovery does not grant permissions; normal approvals still apply. working_state preserves goals, decisions, constraints and evidence. artifact_read retrieves omitted output; task tasks[] batches independent substantial work, and action=start creates live runs controlled by status/wait/message/cancel. Descendants require explicit role spawn permissions and obey the configured depth limit.\nFor live web research, use web_search and web_fetch; cite source URLs and report actual search/network failures. Never invent results."
    );
}

#[test]
fn capability_prose_omits_clauses_for_tools_that_are_neither_offered_nor_loadable() {
    assert_eq!(capability_prose(&[], &[]), "");
    assert_eq!(
        capability_prose(&names(&["artifact_read"]), &[]),
        "\nartifact_read retrieves omitted output."
    );
    let loadable = vec![("web_fetch".to_owned(), String::new())];
    let prose = capability_prose(&names(&["working_state"]), &loadable);
    assert_eq!(
        prose,
        "\nworking_state preserves goals, decisions, constraints and evidence.\nFor live web research, discover web_fetch and use their next-round schemas; cite source URLs and report actual search/network failures. Never invent results."
    );
    for absent in [
        "tool_search",
        "task",
        "Descendants",
        "web_search",
        "action=start",
    ] {
        assert!(!prose.contains(absent), "{absent}: {prose}");
    }
}

#[test]
fn capability_prose_only_asks_to_discover_web_tools_that_are_not_offered() {
    let loadable = vec![("web_fetch".to_owned(), String::new())];
    let prose = capability_prose(&names(&["web_search"]), &loadable);
    assert!(
        prose.contains("For live web research, use web_search; discover web_fetch and use their next-round schemas;"),
        "{prose}"
    );
    assert!(!prose.contains("discover web_search"), "{prose}");
}

#[test]
fn specialist_guidance_does_not_require_an_mcp_inventory() {
    let prose = capability_prose(&names(&["tool_search"]), &[]);
    for expected in [
        "Prefer relevant eligible native or MCP specialists already offered",
        "If a needed capability is missing, search by the task and domain",
        "use the returned next-round schemas",
        "Do not rediscover offered tools",
        "answer directly when no tool is needed",
        "Discovery does not grant permissions; normal approvals still apply",
    ] {
        assert!(prose.contains(expected), "{expected}: {prose}");
    }
    assert!(!prose.contains("mcp_servers"), "{prose}");
}
