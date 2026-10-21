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

fn search(agent: &mut Agent<'_>, query: &str) -> Vec<Value> {
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
    assert!(!output.is_error, "{}", output.text);
    serde_json::from_str(&output.text).unwrap()
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
    with_agent("[assistance]\ncompact_tools=true", |agent| {
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
    });
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
