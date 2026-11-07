use super::capability_prose;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn capability_prose_names_only_tools_that_are_offered_or_loadable() {
    assert_eq!(capability_prose(&[], &[]), "");
    // Tools whose own descriptions carry their mechanics add no system prose.
    assert_eq!(
        capability_prose(&names(&["working_state", "task", "todo"]), &[]),
        ""
    );
    let all = capability_prose(&names(&["tool_search", "web_search", "web_fetch"]), &[]);
    // Discovery is stated once in the environment section, not repeated here.
    assert!(!all.contains("tool_search"), "{all}");
    for expected in ["use web_search and web_fetch", "cite source URLs"] {
        assert!(all.contains(expected), "{expected}: {all}");
    }
    assert!(!all.contains("load web"), "{all}");
    let loadable = vec![("web_fetch".to_owned(), String::new())];
    let prose = capability_prose(&[], &loadable);
    assert!(prose.contains("load web_fetch first"), "{prose}");
    for absent in ["tool_search", "web_search"] {
        assert!(!prose.contains(absent), "{absent}: {prose}");
    }
}

#[test]
fn capability_prose_only_asks_to_load_web_tools_that_are_not_offered() {
    let loadable = vec![("web_fetch".to_owned(), String::new())];
    let prose = capability_prose(&names(&["web_search"]), &loadable);
    assert!(
        prose.contains("use web_search; load web_fetch first;"),
        "{prose}"
    );
    assert!(!prose.contains("load web_search"), "{prose}");
    assert!(!prose.contains("mcp_servers"), "{prose}");
}

#[test]
fn skill_loader_line_names_only_offered_tools() {
    let catalog = "## Skills\n- review: reviews diffs\n";
    let direct = super::with_skill_loader(catalog, &names(&["skill", "tool_load"]));
    assert!(direct.contains("with the `skill` tool"), "{direct}");
    let deferred = super::with_skill_loader(catalog, &names(&["tool_load"]));
    assert!(
        deferred.contains("`tool_load`") && !deferred.contains("with the `skill` tool"),
        "{deferred}"
    );
    assert_eq!(super::with_skill_loader(catalog, &[]), catalog);
    assert_eq!(super::with_skill_loader("", &names(&["skill"])), "");
}
