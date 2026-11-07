use super::default_offer_tests::{load_tools, with_default_agent as with_agent};
use super::*;

const HOSTED: &str = "[endpoint]\nbase_url='https://example.com/v1'";

#[test]
fn enabled_interactive_questions_fit_without_crowding_out_hosted_core_capabilities() {
    with_agent(&format!("{HOSTED}\n[ask]\nenabled=true"), |agent| {
        let offered = agent.offered();
        for name in [
            "ask", "read", "write", "edit", "bash", "bg", "task", "message",
        ] {
            assert!(
                offered.iter().any(|tool| tool == name),
                "{name}: {offered:?}"
            );
        }
        if agent.automatic_eval_ready() {
            assert!(offered.iter().any(|tool| tool == "eval"), "{offered:?}");
        }
        assert!(
            serde_json::to_vec(&agent.schemas(&offered).unwrap())
                .unwrap()
                .len()
                <= Agent::HOSTED_CORE_SCHEMA_BYTES
        );
    });
}

#[test]
fn automatic_eval_exposure_can_be_ablated_without_refusing_an_explicit_load() {
    with_agent("[eval]\nauto_offer=false", |agent| {
        assert!(!agent.offered().iter().any(|name| name == "eval"));
        let output = load_tools(agent, json!({"names":["eval"]}));
        assert!(!output.is_error, "{}", output.text);
        assert!(agent.offered().iter().any(|name| name == "eval"));
    });
}

#[test]
fn missing_eval_interpreters_are_disclosed_before_loading_or_generating_code() {
    with_agent(
        "[eval]\nnode='/missing-dial-test-node'\n[kernel]\npython='/missing-dial-test-python'",
        |agent| {
            let before = agent.system_messages();
            let text = format!("{before:?}");
            if tools::exec::eval::embedded_js_available() {
                // The embedded engine keeps JavaScript resolved, so only Python can be missing.
                assert!(text.contains("JavaScript resolved"), "{text}");
                assert!(text.contains("Python not found"), "{text}");
                let containment = tools::exec::eval::prepare_containment();
                let catalog = agent.tool_catalog().unwrap();
                let readiness = &catalog.get("eval").unwrap().readiness;
                match containment {
                    Ok(()) => assert!(!matches!(
                        readiness,
                        tools::surface::catalog::Readiness::MissingPrerequisite { .. }
                    )),
                    // Without OS containment the only missing prerequisite is containment itself.
                    Err(reason) => assert!(
                        matches!(
                            readiness,
                            tools::surface::catalog::Readiness::MissingPrerequisite { reason: shown }
                                if *shown == reason
                        ),
                        "{readiness:?}"
                    ),
                }
                return;
            }
            assert!(text.contains("JavaScript not found"), "{text}");
            assert!(text.contains("Python not found"), "{text}");
            let result = search(agent, json!({"query":"select:eval"}));
            assert!(!result.is_error, "{}", result.text);
            let entries: Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(
                entries[0]["readiness"]["state"], "missing_prerequisite",
                "{entries}"
            );
            let catalog = agent.tool_catalog().unwrap();
            assert!(matches!(
                catalog.get("eval").unwrap().readiness,
                tools::surface::catalog::Readiness::MissingPrerequisite { .. }
            ));
            assert!(
                !load_tools(agent, json!({"names":["eval"]})).is_error,
                "loading preserves status/reset/cancel even without interpreters"
            );
            assert_eq!(
                agent.system_messages(),
                before,
                "loading must not churn the prefix"
            );
        },
    );
}

#[test]
fn hosted_and_local_core_offers_obey_the_byte_budget_and_ignore_task_routing() {
    for (overlay, hosted) in [
        ("[assistance]\nlean_local=false", false),
        ("[assistance]\nlean_local=true", false),
        (HOSTED, true),
    ] {
        with_agent(overlay, |agent| {
            assert_eq!(agent.hosted_route(), hosted);
            let budget = if hosted {
                Agent::HOSTED_CORE_SCHEMA_BYTES
            } else {
                Agent::CORE_SCHEMA_BYTES
            };
            assert_eq!(agent.core_schema_budget(), budget);
            let offered = agent.offered();
            let schemas = agent.schemas(&offered).unwrap();
            let bytes = serde_json::to_vec(&schemas).unwrap().len();
            eprintln!(
                "core ({overlay:?}): {} tools, {bytes} schema bytes: {offered:?}",
                offered.len()
            );
            assert!(bytes <= budget, "{bytes}: {offered:?}");
            assert!(
                !offered
                    .iter()
                    .any(|name| matches!(name.as_str(), "git" | "working_state")
                        || name.starts_with("mcp__"))
            );
            // A runnable `eval` is core on hosted routes; local budgets admit it only if it fits.
            if hosted {
                assert_eq!(
                    offered.iter().any(|name| name == "eval"),
                    agent.automatic_eval_ready(),
                    "{offered:?}"
                );
            }
            // Delegation is a hosted-route default; job control joins wherever it fits.
            for delegation in ["task", "message"] {
                assert_eq!(
                    offered.iter().any(|name| name == delegation),
                    hosted,
                    "{offered:?}"
                );
            }
            if hosted {
                assert!(offered.iter().any(|name| name == "bg"), "{offered:?}");
            }
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

/// Unattended hosted runs (print mode, benchmarks) defer session helpers but keep a runnable
/// `eval` in the first request, so code-heavy work needs no `tool_load` round.
#[test]
fn unattended_hosted_runs_offer_a_runnable_eval_up_front() {
    with_agent(HOSTED, |agent| {
        agent.modes.launch.unattended = true;
        let offered = agent.offered();
        assert_eq!(
            offered.iter().any(|name| name == "eval"),
            agent.automatic_eval_ready(),
            "{offered:?}"
        );
        let schemas = agent.schemas(&offered).unwrap();
        assert!(serde_json::to_vec(&schemas).unwrap().len() <= Agent::HOSTED_CORE_SCHEMA_BYTES);
    });
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

#[test]
fn bash_names_bg_as_loadable_only_where_the_core_offer_lacks_it() {
    with_agent(HOSTED, |agent| {
        assert!(!agent.bg_loadable());
        let bash = &agent.schemas(&["bash".into()]).unwrap()[0];
        let text = bash["function"]["description"].as_str().unwrap();
        assert!(!text.contains("not offered yet"), "{text}");
        // Hosted routes keep the tools' own texts: dialect, output format, sandbox.
        let grep = &agent.schemas(&["grep".into()]).unwrap()[0];
        assert!(
            grep["function"]["description"]
                .as_str()
                .unwrap()
                .starts_with("grep: search file contents with a Rust regex")
        );
        assert!(text.contains("$TMPDIR") || cfg!(windows), "{text}");
    });
    with_agent("[assistance]\nlean_local=true", |agent| {
        assert!(!agent.offered().iter().any(|name| name == "bg"));
        assert!(agent.bg_loadable());
        let bash = &agent.schemas(&["bash".into()]).unwrap()[0];
        let text = bash["function"]["description"].as_str().unwrap();
        assert!(
            text.contains("Dial loads it for the next request"),
            "{text}"
        );
        // The shorter local texts keep the facts a first call depends on.
        assert!(text.contains("$TMPDIR") || cfg!(windows), "{text}");
        assert!(!text.contains("automatically"), "{text}");
        let grep = &agent.schemas(&["grep".into()]).unwrap()[0];
        let grep = grep["function"]["description"].as_str().unwrap();
        for fact in [
            "Rust regex",
            "lookaround",
            "path:line:text",
            "`;`-separated",
            "next_cursor",
        ] {
            assert!(grep.contains(fact), "{fact}: {grep}");
        }
        let glob = &agent.schemas(&["glob".into()]).unwrap()[0];
        assert!(
            glob["function"]["description"]
                .as_str()
                .unwrap()
                .contains("next_cursor")
        );
    });
}

#[test]
fn a_background_job_loads_bg_through_the_selection_owner_in_the_stable_offer() {
    with_agent("[assistance]\nlean_local=true", |agent| {
        assert!(agent.stable_tool_offer());
        let system = agent.system_messages();
        assert!(agent.load_bg_for_background_job());
        assert!(agent.offered().iter().any(|name| name == "bg"));
        assert!(agent.enabled_tools.contains("bg"));
        assert!(!agent.bg_loadable());
        assert!(!agent.load_bg_for_background_job(), "loads once");
        assert_eq!(agent.system_messages(), system);
    });
}

fn search(agent: &mut Agent<'_>, arguments: Value) -> ToolOutput {
    struct Quiet;
    impl Ui for Quiet {}
    let mut backend = agent.backend.take().unwrap();
    let result = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: "search".into(),
            name: "tool_search".into(),
            arguments,
            malformed_arguments: false,
        },
        &mut Quiet,
    );
    agent.backend = Some(backend);
    result
}

#[test]
fn tool_search_with_load_selects_the_match_in_one_step() {
    with_agent("", |agent| {
        let before = agent.offered();
        assert!(!before.iter().any(|name| name == "git"));
        let result = search(agent, json!({"query":"git","load":true}));
        assert!(!result.is_error, "{}", result.text);
        let entries: Value = serde_json::from_str(&result.text).unwrap();
        let git = entries
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "git")
            .unwrap();
        assert_eq!(git["status"], "loaded_next_request", "{git}");
        assert!(
            git["parameters"].as_str().unwrap().contains("action*:"),
            "{git}"
        );
        let after = agent.offered();
        assert!(after.contains(&"git".to_owned()));
        assert!(agent.enabled_tools.contains("git"));
        assert!(before.iter().all(|name| after.contains(name)));
    });
}

#[test]
fn select_queries_name_exact_tools_and_report_unknown_ones() {
    with_agent("", |agent| {
        let result = search(agent, json!({"query":"select:lsp, nonexistent"}));
        assert!(!result.is_error, "{}", result.text);
        let entries: Value = serde_json::from_str(&result.text).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0]["name"], "lsp");
        assert_eq!(entries[0]["status"], "not_loaded");
        assert_eq!(entries[1]["name"], "nonexistent");
        assert_eq!(entries[1]["status"], "unavailable");
        assert!(!agent.offered().iter().any(|name| name == "lsp"));
        let loaded = search(agent, json!({"query":"select:lsp","load":true}));
        assert!(!loaded.is_error, "{}", loaded.text);
        assert!(agent.offered().iter().any(|name| name == "lsp"));
        assert!(search(agent, json!({"query":"select:"})).is_error);
    });
}

#[test]
fn tool_search_load_is_an_additive_contract_property() {
    with_agent("", |agent| {
        let current = agent.full_schemas(&["tool_search".into()]).unwrap();
        let mut before = current[0].clone();
        before["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("load");
        let registry = crate::tools::integrations::ext::Registry::new();
        let old = crate::tools::surface::contracts::identities(&[before], &registry)["tool_search"]
            .clone();
        assert_ne!(
            old,
            crate::tools::surface::contracts::identities(&current, &registry)["tool_search"]
        );
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
        crate::tools::surface::contracts::validate_history(&history, &current, &registry).unwrap();
    });
}

#[test]
fn core_tools_keep_short_parameter_descriptions_and_specialists_stay_bare() {
    with_agent(HOSTED, |agent| {
        let schemas = agent
            .schemas(&[
                "bash".into(),
                "grep".into(),
                "bg".into(),
                "write".into(),
                "git".into(),
            ])
            .unwrap();
        let property = |tool: usize, name: &str| {
            schemas[tool]["function"]["parameters"]["properties"][name]["description"]
                .as_str()
                .map(str::to_owned)
        };
        assert_eq!(
            property(0, "timeout").as_deref(),
            Some("Seconds to wait before the command is killed")
        );
        assert_eq!(
            property(1, "limit").as_deref(),
            Some("Most matching lines (default 400; otherwise at most 20 per file)")
        );
        assert!(property(2, "lifetime").is_some());
        for schema in schemas.iter().take(3) {
            for (name, value) in schema["function"]["parameters"]["properties"]
                .as_object()
                .unwrap()
            {
                if let Some(text) = value["description"].as_str() {
                    assert!(text.split_whitespace().count() <= 15, "{name}: {text}");
                }
                // Nested explanations stay out of the offer.
                assert!(value["properties"].as_object().is_none_or(|nested| {
                    nested
                        .values()
                        .all(|child| child.get("description").is_none())
                }));
            }
        }
        assert!(property(4, "action").is_none());
    });
}

#[test]
fn parameter_summaries_mark_required_parameters_and_bound_enums() {
    let schema = json!({"function":{"parameters":{"type":"object","required":["action"],"properties":{
        "action":{"type":"string","enum":["a","b","c","d","e","f","g","h","i","j","k","l","m"]},
        "path":{"type":"string"},
    }}}});
    assert_eq!(
        super::parameter_summary(&schema),
        "action*: a|b|c|d|e|f|g|h|i|j|k|l|…; path: string"
    );
}
