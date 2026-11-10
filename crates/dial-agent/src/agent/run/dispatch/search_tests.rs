use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::tools::integrations::ext::Tool;

struct Probe(&'static str);
impl Tool for Probe {
    fn name(&self) -> &str {
        self.0
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.0,"description":"Perform an operation.","parameters":{
            "type":"object", "title":"metadataonlyquux", "default":{"sample":"defaultonlyquux"},
            "properties":{
                "action":{"type":"string","enum":["spectrographyquux"]},
                "settings":{"type":"array","items":{"type":"object","properties":{
                    "target":{"description":"Analyze fluorometryquux measurements", "type":"string"},
                    "chromatographyquux":{"type":"string"}
                }}}
            }
        }}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("tool_search must never execute discovered tools")
    }
}

fn dispatch_query(agent: &mut Agent<'_>, query: &str) -> ToolOutput {
    struct Quiet;
    impl Ui for Quiet {}
    let mut backend = agent.backend.take().unwrap();
    let output = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: "search-regression".into(),
            name: "tool_search".into(),
            arguments: json!({"query":query}),
            malformed_arguments: false,
        },
        &mut Quiet,
    );
    agent.backend = Some(backend);
    output
}

fn search(agent: &mut Agent<'_>, query: &str) -> Vec<Value> {
    let output = dispatch_query(agent, query);
    assert!(!output.is_error, "{}", output.text);
    let value: Value = serde_json::from_str(&output.text).unwrap();
    if value["listing"] == true {
        // A strict miss returns non-activated partial matches beside the catalog page.
        for entry in value["search_matches"].as_array().unwrap() {
            assert_eq!(entry["enabled_next_round"], false, "{entry}");
        }
        Vec::new()
    } else {
        serde_json::from_value(value).unwrap()
    }
}

/// One listing page as parsed JSON; a refusal fails the caller.
fn listing(agent: &mut Agent<'_>, query: &str) -> Value {
    let output = dispatch_query(agent, query);
    assert!(!output.is_error, "{query}: {}", output.text);
    serde_json::from_str(&output.text).unwrap()
}

/// Walks `*` and each returned `next_query` until the listing reports no next page.
fn listing_pages(agent: &mut Agent<'_>) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut query = "*".to_owned();
    loop {
        let page = listing(agent, &query);
        pages.push(page.clone());
        assert!(pages.len() <= 100, "listing did not terminate");
        match page["next_query"].as_str() {
            Some(next) => query = next.to_owned(),
            None => return pages,
        }
    }
}

fn all_entries(agent: &mut Agent<'_>) -> Vec<Value> {
    listing_pages(agent)
        .iter()
        .flat_map(|page| page["tools"].as_array().unwrap().clone())
        .collect()
}

/// Leaked names are test-only and give the probes the `&'static str` the fixture requires.
fn register_probes(agent: &mut Agent<'_>, prefix: &str, count: usize) {
    for index in 0..count {
        let name: &'static str = Box::leak(format!("{prefix}{index:02}").into_boxed_str());
        agent.ext.try_register(Arc::new(Probe(name))).unwrap();
    }
}

#[test]
fn parameter_capabilities_are_searchable_without_changing_display_or_executing() {
    with_agent("", |agent| {
        agent
            .ext
            .try_register(Arc::new(Probe("mcp__lab__operate")))
            .unwrap();
        for query in ["fluorometryquux", "spectrographyquux", "chromatographyquux"] {
            let matches = search(agent, query);
            assert_eq!(matches.len(), 1, "{query}: {matches:?}");
            assert_eq!(matches[0]["name"], "mcp__lab__operate");
            assert_eq!(matches[0]["description"], "Perform an operation.");
            assert_eq!(matches[0]["enabled_next_round"], true);
        }
        for query in ["metadataonlyquux", "defaultonlyquux"] {
            assert!(search(agent, query).is_empty(), "{query}");
        }
    });
}

#[test]
fn exact_name_beats_parameter_matches_and_activation_stays_bounded() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            for name in [
                "fluorometryquux",
                "mcp__lab__operate",
                "native_probe",
                "other_probe",
                "last_probe",
            ] {
                agent.ext.try_register(Arc::new(Probe(name))).unwrap();
            }
            let exact = search(agent, "  FLUOROMETRYQUUX  ");
            assert_eq!(exact.len(), 1);
            assert_eq!(exact[0]["name"], "fluorometryquux");
            let matches = search(agent, "spectrographyquux");
            assert_eq!(matches.len(), 5);
            assert_eq!(
                matches
                    .iter()
                    .map(|entry| entry["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                [
                    "fluorometryquux",
                    "last_probe",
                    "mcp__lab__operate",
                    "native_probe",
                    "other_probe"
                ]
            );
            assert_eq!(agent.discovered_tools().len(), 3);
            assert_eq!(
                matches
                    .iter()
                    .filter(|entry| entry["enabled_next_round"] == true)
                    .count(),
                3
            );
        },
    );
}

#[test]
fn discovery_results_state_load_status_and_the_exact_next_action() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            for name in [
                "fluorometryquux",
                "mcp__lab__operate",
                "native_probe",
                "other_probe",
                "last_probe",
            ] {
                agent.ext.try_register(Arc::new(Probe(name))).unwrap();
            }
            let matches = search(agent, "spectrographyquux");
            assert_eq!(matches.len(), 5);
            for entry in &matches {
                let (status, next) = if entry["enabled_next_round"] == true {
                    ("loaded_next_request", "exact name on the next request")
                } else {
                    ("not_loaded", "tool_search")
                };
                assert_eq!(entry["status"], status, "{entry:?}");
                assert!(
                    entry["next"]
                        .as_str()
                        .is_some_and(|text| text.contains(next)),
                    "{entry:?}"
                );
            }
            assert_eq!(
                matches
                    .iter()
                    .filter(|e| e["status"] == "loaded_next_request")
                    .count(),
                3
            );
        },
    );
}

#[test]
fn parameter_help_cannot_bypass_tool_eligibility() {
    for mode in ["allowlist", "plan"] {
        with_agent("", |agent| {
            agent
                .ext
                .try_register(Arc::new(Probe("mcp__lab__operate")))
                .unwrap();
            if mode == "allowlist" {
                agent.modes.launch.tools = Some(["tool_search".to_owned()].into());
            } else {
                agent.plan_mode = true;
            }
            assert!(!agent.tool_eligible("mcp__lab__operate"), "{mode}");
            assert!(search(agent, "fluorometryquux").is_empty(), "{mode}");
            let matches = search(agent, "mcp__lab__operate");
            assert!(
                matches
                    .iter()
                    .all(|entry| entry["name"] != "mcp__lab__operate"),
                "{mode}: {matches:?}"
            );
            assert!(
                !agent
                    .discovered_tools()
                    .contains(&"mcp__lab__operate".to_owned())
            );
        });
    }
}

#[test]
fn schema_index_is_bounded_and_ignores_unrelated_metadata() {
    let schema = json!({"function":{"description":"x".repeat(100_000),"parameters":{
        "properties":{"capability":{"description":"🦀".repeat(100_000), "enum":["enumvalue"]}},
        "examples":["exampleonlyquux"], "$defs":{"ignored":{"description":"definitiononlyquux"}}
    }}});
    let text = search_index::text(&schema);
    assert!(text.chars().count() <= 8192);
    assert!(text.contains("capability"));
    assert!(text.contains("enumvalue"));
    assert!(!text.contains("exampleonlyquux"));
    assert!(!text.contains("definitiononlyquux"));
}

#[test]
fn listing_pages_cover_the_eligible_catalog_once_in_sorted_order() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            register_probes(agent, "listprobe_", 20);
            let expected: std::collections::BTreeSet<String> = agent
                .catalog()
                .into_iter()
                .filter(|name| agent.tool_eligible(name))
                .collect();
            let pages = listing_pages(agent);
            assert!(pages.len() > 1, "fixture must span several pages");
            assert_eq!(pages.len(), expected.len().div_ceil(16));
            let mut seen = Vec::new();
            for (index, page) in pages.iter().enumerate() {
                assert_eq!(page["page"].as_u64(), Some(index as u64));
                assert_eq!(page["total"].as_u64(), Some(expected.len() as u64));
                let tools = page["tools"].as_array().unwrap();
                assert!(!tools.is_empty() && tools.len() <= 16, "page {index}");
                seen.extend(
                    tools
                        .iter()
                        .map(|tool| tool["name"].as_str().unwrap().to_owned()),
                );
            }
            assert!(
                seen.windows(2).all(|pair| pair[0] < pair[1]),
                "pages must be sorted with no overlap: {seen:?}"
            );
            assert_eq!(seen.len(), expected.len());
            assert_eq!(
                seen.iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>(),
                expected
            );
        },
    );
}

#[test]
fn malformed_or_out_of_range_listing_cursors_are_refused_without_activation() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            register_probes(agent, "listprobe_", 20);
            let total = listing(agent, "*")["total"].as_u64().unwrap();
            let past_end = format!("*:{}", total.div_ceil(16));
            for query in [
                "*:",
                "*:x",
                "*:-1",
                "*: 1",
                "*:1x",
                "*:+1",
                "*:99999999999999999999999999",
                past_end.as_str(),
            ] {
                let output = dispatch_query(agent, query);
                assert!(output.is_error, "{query}: {}", output.text);
                assert!(
                    output.text.contains("listing cursor"),
                    "{query}: {}",
                    output.text
                );
            }
            assert!(agent.discovered_tools().is_empty());
        },
    );
}

#[test]
fn listing_reports_status_and_never_activates_or_grants_eligibility() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            register_probes(agent, "listprobe_", 20);
            let offered_before = agent.offered();
            for entry in all_entries(agent) {
                let name = entry["name"].as_str().unwrap();
                let loaded = offered_before.iter().any(|offered| offered == name);
                let (status, next) = if loaded {
                    ("offered", "exact name")
                } else {
                    ("not_loaded", "tool_search")
                };
                assert_eq!(entry["enabled_next_round"], loaded, "{entry:?}");
                assert_eq!(entry["status"], status, "{entry:?}");
                assert!(entry["next"].as_str().unwrap().contains(next), "{entry:?}");
            }
            assert!(agent.discovered_tools().is_empty());
            assert_eq!(agent.offered(), offered_before);
            // An exact search activates one tool; the later listing must report exactly that state.
            let matched = search(agent, "listprobe_07");
            assert_eq!(matched[0]["name"], "listprobe_07");
            let entry = all_entries(agent)
                .into_iter()
                .find(|entry| entry["name"] == "listprobe_07")
                .unwrap();
            assert_eq!(entry["status"], "offered");
            assert_eq!(entry["enabled_next_round"], true);
        },
    );
}

#[test]
fn listing_only_shows_tools_eligible_under_the_current_boundary() {
    for mode in ["allowlist", "plan"] {
        with_agent("", |agent| {
            agent
                .ext
                .try_register(Arc::new(Probe("mcp__lab__operate")))
                .unwrap();
            register_probes(agent, "listprobe_", 3);
            if mode == "allowlist" {
                agent.modes.launch.tools = Some(["tool_search".to_owned()].into());
            } else {
                agent.plan_mode = true;
            }
            let entries = all_entries(agent);
            assert!(
                entries.iter().any(|entry| entry["name"] == "tool_search"),
                "{mode}: {entries:?}"
            );
            for entry in &entries {
                let name = entry["name"].as_str().unwrap();
                assert!(agent.tool_eligible(name), "{mode}: {name}");
                assert!(
                    name != "mcp__lab__operate" && !name.starts_with("listprobe_"),
                    "{mode}: {name}"
                );
            }
            assert!(agent.discovered_tools().is_empty(), "{mode}");
        });
    }
}

#[test]
fn ordinary_queries_that_resemble_listing_syntax_still_search() {
    with_agent("", |agent| {
        register_probes(agent, "listprobe_", 2);
        for query in ["*x", "a*", "list*"] {
            let output = dispatch_query(agent, query);
            assert!(!output.is_error, "{query}: {}", output.text);
            let value: Value = serde_json::from_str(&output.text).unwrap();
            assert!(
                value.is_array() || value["search_matches"] == json!([]),
                "{query}: {value}"
            );
        }
    });
}

#[test]
fn search_miss_returns_bounded_eligible_catalog_without_loading_tools() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            register_probes(agent, "fallback_probe_", 20);
            let before = agent.offered();
            // Use an absent token: words like "no" or "such" legitimately rank catalog prose.
            let page = listing(agent, "zqjxvnomatchzz");
            assert_eq!(page["search_matches"], json!([]));
            assert_eq!(page["listing"], true);
            assert_eq!(page["page"], 0);
            assert!(page["tools"].as_array().unwrap().len() <= TOOL_LIST_PAGE);
            assert!(page["next_query"].is_string());
            for entry in page["tools"].as_array().unwrap() {
                assert!(agent.tool_eligible(entry["name"].as_str().unwrap()));
            }
            assert_eq!(agent.offered(), before);
            assert!(agent.discovered_tools().is_empty());
        },
    );
}

#[test]
fn discovery_reports_readiness_without_executing_or_hiding_unknown_tools() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            agent
                .ext
                .try_register(Arc::new(Probe("mcp__lab__operate")))
                .unwrap();
            let before = agent.offered();
            let entries = all_entries(agent);
            assert_eq!(agent.offered(), before);
            let probe = entries
                .iter()
                .find(|entry| entry["name"] == "mcp__lab__operate")
                .unwrap();
            assert_eq!(probe["readiness"]["state"], "unknown");
            let read = entries
                .iter()
                .find(|entry| entry["name"] == "read")
                .unwrap();
            assert_eq!(read["readiness"]["state"], "ready");
            let matches = search(agent, "mcp__lab__operate");
            assert_eq!(matches[0]["readiness"]["state"], "unknown");
            assert_eq!(matches[0]["enabled_next_round"], true);
        },
    );
}

struct ReadinessProbe(&'static str, crate::tools::surface::catalog::Readiness);
impl Tool for ReadinessProbe {
    fn name(&self) -> &str {
        self.0
    }
    fn schema(&self) -> Value {
        Probe(self.0).schema()
    }
    fn readiness(&self, _: &Config) -> crate::tools::surface::catalog::Readiness {
        self.1.clone()
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("discovery must not execute or probe readiness tools")
    }
}

#[test]
fn capability_search_prefers_ready_ties_before_activation_and_result_caps() {
    use crate::tools::surface::catalog::Readiness;
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            for index in 0..10 {
                let name = Box::leak(format!("cap_{index:02}").into_boxed_str());
                let readiness = if index == 9 {
                    Readiness::Ready
                } else {
                    Readiness::MissingPrerequisite {
                        reason: "fixture prerequisite missing".into(),
                    }
                };
                agent
                    .ext
                    .try_register(Arc::new(ReadinessProbe(name, readiness)))
                    .unwrap();
            }
            let entries = search(agent, "spectrographyquux");
            assert_eq!(entries.len(), 8);
            assert_eq!(entries[0]["name"], "cap_09");
            assert_eq!(entries[0]["readiness"]["state"], "ready");
            assert_eq!(entries[0]["enabled_next_round"], true);
            assert_eq!(entries[1]["name"], "cap_00");
            assert_eq!(entries[1]["readiness"]["state"], "missing_prerequisite");
            assert_eq!(entries[2]["name"], "cap_01");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry["enabled_next_round"] == true)
                    .count(),
                3
            );
            assert_eq!(agent.discovered_tools(), ["cap_09", "cap_00", "cap_01"]);
            let listed = all_entries(agent);
            assert_eq!(
                listed
                    .iter()
                    .filter(|entry| entry["name"].as_str().unwrap().starts_with("cap_"))
                    .count(),
                10
            );
        },
    );
}

#[test]
fn exact_blocked_name_remains_discoverable_but_cannot_bypass_eligibility() {
    use crate::tools::surface::catalog::Readiness;
    for (readiness, expected) in [
        (
            Readiness::MissingPrerequisite {
                reason: "fixture prerequisite missing".into(),
            },
            "missing_prerequisite",
        ),
        (
            Readiness::Unsupported {
                reason: "fixture platform unsupported".into(),
            },
            "unsupported",
        ),
    ] {
        with_agent(
            "[assistance]\ncompact_tools=false\nlean_local=true",
            |agent| {
                agent
                    .ext
                    .try_register(Arc::new(ReadinessProbe("blocked_probe", readiness)))
                    .unwrap();
                let entries = search(agent, "  BLOCKED_PROBE  ");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0]["name"], "blocked_probe");
                assert_eq!(entries[0]["readiness"]["state"], expected);
                assert_eq!(entries[0]["enabled_next_round"], true);
                assert!(agent.offered().contains(&"blocked_probe".to_owned()));
                agent.modes.launch.tools = Some(["tool_search".to_owned()].into());
                assert!(!agent.tool_eligible("blocked_probe"));
                let entries = search(agent, "blocked_probe");
                assert!(
                    entries.iter().all(|entry| {
                        let name = entry["name"].as_str().unwrap();
                        name != "blocked_probe" && agent.tool_eligible(name)
                    }),
                    "{entries:?}"
                );
                assert!(!agent.offered().contains(&"blocked_probe".to_owned()));
            },
        );
    }
}

#[test]
fn explicit_loading_keeps_large_eligible_selections_and_complete_receipts() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            register_probes(agent, "many_", 70);
            let names: Vec<_> = (0..70).map(|index| format!("many_{index:02}")).collect();
            // `names` is bounded at 32 per call (the model-facing contract); a large selection
            // arrives over several calls, each with a complete receipt, and all of it stays offered.
            for batch in names.chunks(32) {
                let output = crate::agent::run::tool_exposure::default_offer_tests::load_tools(
                    agent,
                    json!({"names":batch}),
                );
                assert!(!output.is_error, "{}", output.text);
                let entries = output.data.as_ref().unwrap()["tools"].as_array().unwrap();
                assert_eq!(entries.len(), batch.len());
                for (entry, name) in entries.iter().zip(batch) {
                    assert_eq!(entry["name"], *name);
                    assert_eq!(entry["enabled_next_round"], true);
                }
            }
            let offered = agent.offered();
            for name in &names {
                assert!(offered.contains(name), "{name}");
            }
            let output = crate::agent::run::tool_exposure::default_offer_tests::load_tools(
                agent,
                json!({"names":names}),
            );
            assert!(
                output.is_error,
                "names beyond the declared bound are refused"
            );
        },
    );
}
