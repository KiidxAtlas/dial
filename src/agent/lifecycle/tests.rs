use super::capability_prose;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn capability_prose_is_byte_identical_when_every_named_tool_is_present() {
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
        "\ntool_search enables up to three strongest matches for the next model round. working_state preserves goals, decisions, constraints and evidence. artifact_read retrieves omitted output; task tasks[] batches independent substantial work, and action=start creates live runs controlled by status/wait/message/cancel. Descendants require explicit role spawn permissions and obey the configured depth limit.\nFor live web research, discover web_search and web_fetch and use their next-round schemas; cite source URLs and report actual search/network failures. Never invent results."
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
