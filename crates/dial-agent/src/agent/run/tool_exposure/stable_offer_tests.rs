use super::default_offer_tests::{load_tools, with_default_agent as with_agent};
use super::*;

#[test]
fn hosted_and_local_core_offers_obey_the_byte_budget_and_ignore_task_routing() {
    for overlay in [
        "[assistance]\nlean_local=false",
        "[assistance]\nlean_local=true",
        "[endpoint]\nbase_url='https://example.com/v1'",
    ] {
        with_agent(overlay, |agent| {
            let offered = agent.offered();
            let schemas = agent.schemas(&offered).unwrap();
            let bytes = serde_json::to_vec(&schemas).unwrap().len();
            eprintln!("core: {} tools, {bytes} schema bytes", offered.len());
            assert!(bytes <= Agent::CORE_SCHEMA_BYTES, "{bytes}: {offered:?}");
            assert!(!offered.iter().any(|name| matches!(
                name.as_str(),
                "task" | "eval" | "git" | "working_state"
            ) || name.starts_with("mcp__")));
            let system = agent.system_messages();
            for query in [
                "Search documentation with Context7",
                "Inspect the browser screenshot",
                "Run a complex audit using task and git",
            ] {
                agent.begin_tool_routing(query);
                agent.rebuild_system();
                assert_eq!(agent.offered(), offered);
                assert_eq!(agent.schemas(&offered).unwrap(), schemas);
                assert_eq!(agent.system_messages(), system);
            }
        });
    }
}

#[test]
fn a_specialist_joins_only_after_an_explicit_load_and_preserves_the_system_band() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        let before = agent.offered();
        let system = agent.system_messages();
        let output = load_tools(agent, json!({"names":["git"]}));
        assert!(!output.is_error, "{}", output.text);
        let after = agent.offered();
        assert!(after.contains(&"git".to_owned()));
        assert!(before.iter().all(|name| after.contains(name)));
        assert_eq!(
            agent.system_messages(),
            system,
            "loading changes schemas, not cached policy"
        );
        let output = load_tools(agent, json!({"unload":["git"]}));
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(agent.offered(), before);
    });
}

#[test]
fn searching_a_specialist_leaves_the_offer_and_cached_prefix_unchanged() {
    with_agent("", |agent| {
        struct Quiet;
        impl Ui for Quiet {}
        let before = agent.offered();
        let system = agent.system_messages();
        let mut backend = agent.backend.take().unwrap();
        let result = agent.dispatch(
            &mut backend,
            &ToolCall {
                id: "search".into(),
                name: "tool_search".into(),
                arguments: json!({"query":"git"}),
                malformed_arguments: false,
            },
            &mut Quiet,
        );
        agent.backend = Some(backend);
        assert!(!result.is_error, "{}", result.text);
        let entries: Value = serde_json::from_str(&result.text).unwrap();
        assert!(
            entries
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "git"
                    && entry["status"] == "not_loaded"
                    && entry["next"].as_str().unwrap().contains("tool_load"))
        );
        assert_eq!(agent.offered(), before);
        assert_eq!(agent.system_messages(), system);
        assert!(agent.discovered_tools().is_empty());
    });
}
