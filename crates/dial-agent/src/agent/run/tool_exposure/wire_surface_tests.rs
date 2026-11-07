use super::default_offer_tests::{load_tools, with_default_agent as with_agent};
use super::*;
use crate::llm::tool_offer::{self, Exposure};

const HOSTED: &str = "[endpoint]\nbase_url='https://example.com/v1'";

fn request(agent: &Agent<'_>, messages: Vec<Value>) -> (Vec<Value>, Chat) {
    let offer = agent.schemas(&agent.offered()).unwrap();
    let chat = Chat {
        messages,
        tools: offer.clone(),
        temperature: 0.0,
        max_tokens: 16,
        seed: None,
    };
    (offer, chat)
}

fn name(schema: &Value) -> &str {
    schema["function"]["name"].as_str().unwrap()
}

/// What a deferring wire sends for each tool: its definition and whether it is deferred. Marks
/// for callability are harness bookkeeping and never reach the provider.
fn sent(tools: &[Value]) -> Vec<(Value, bool)> {
    tools
        .iter()
        .map(|tool| (tool["function"].clone(), tool_offer::mark_of(tool).deferred))
        .collect()
}

fn loaded_history(text: &str) -> Vec<Value> {
    vec![
        json!({"role":"user","content":"inspect the symbol"}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"call-load","type":"function","function":{"name":"tool_load","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"call-load","content":text}),
    ]
}

#[test]
fn a_tool_load_changes_no_definition_on_a_deferring_route() {
    with_agent(HOSTED, |agent| {
        let (offer, mut before) = request(agent, loaded_history("pending"));
        agent
            .wire_tool_surface(Exposure::Deferred, &offer, &mut before)
            .unwrap();
        let core = agent.stable_core_tools();
        for tool in &before.tools {
            let mark = tool_offer::mark_of(tool);
            let core_tool = core.iter().any(|entry| entry == name(tool));
            assert_eq!(mark.deferred, !core_tool, "{}", name(tool));
            assert_eq!(mark.callable, core_tool, "{}", name(tool));
        }
        let candidate = before
            .tools
            .iter()
            .map(name)
            .find(|tool| {
                ["ast_search", "lsp", "git"].contains(tool)
                    && agent.tool_readiness(tool).automatic_candidate()
            })
            .expect("a deferred specialist is ready")
            .to_owned();
        let output = load_tools(agent, json!({"names":[candidate]}));
        assert!(!output.is_error, "{}", output.text);
        let (offer, mut after) = request(agent, loaded_history(&output.text));
        agent
            .wire_tool_surface(Exposure::Deferred, &offer, &mut after)
            .unwrap();
        assert_eq!(
            sent(&before.tools),
            sent(&after.tools),
            "loading changes callability only"
        );
        assert!(
            after
                .tools
                .iter()
                .any(|tool| name(tool) == candidate && tool_offer::mark_of(tool).callable)
        );
        assert_eq!(tool_offer::references(&after.messages[2]), [candidate]);
        // History contracts and admission still see exactly the offered tools.
        assert_eq!(*tool_offer::callable(&after.tools), offer[..]);
    });
}

#[test]
fn narrowing_and_unloads_keep_the_deferred_surface() {
    with_agent(HOSTED, |agent| {
        let output = load_tools(agent, json!({"names":["ast_search"]}));
        assert!(!output.is_error, "{}", output.text);
        let (offer, mut full) = request(agent, loaded_history(&output.text));
        agent
            .wire_tool_surface(Exposure::Deferred, &offer, &mut full)
            .unwrap();
        // The planning gate offers only `todo` and `finish` for one request.
        let (offer, mut gated) = request(agent, loaded_history(&output.text));
        gated
            .tools
            .retain(|tool| matches!(name(tool), "todo" | "finish"));
        agent
            .wire_tool_surface(Exposure::Deferred, &offer, &mut gated)
            .unwrap();
        assert_eq!(sent(&full.tools), sent(&gated.tools));
        let callable = tool_offer::callable(&gated.tools);
        let callable: Vec<&str> = callable.iter().map(name).collect();
        assert_eq!(callable, ["finish", "todo"]);
        // Unloading keeps the referenced definition in place and makes it uncallable.
        let output = load_tools(agent, json!({"unload":["ast_search"]}));
        assert!(!output.is_error, "{}", output.text);
        let (offer, mut unloaded) = request(agent, loaded_history("loaded earlier"));
        agent
            .wire_tool_surface(Exposure::Deferred, &offer, &mut unloaded)
            .unwrap();
        assert_eq!(sent(&full.tools), sent(&unloaded.tools));
        let ast = unloaded
            .tools
            .iter()
            .find(|tool| name(tool) == "ast_search")
            .unwrap();
        assert!(!tool_offer::mark_of(ast).callable);
    });
}

#[test]
fn allowed_tools_routes_keep_mode_tools_and_narrow_by_callability() {
    with_agent(HOSTED, |agent| {
        let (offer, mut full) = request(agent, vec![json!({"role":"user","content":"x"})]);
        agent
            .wire_tool_surface(Exposure::AllowedTools, &offer, &mut full)
            .unwrap();
        let exit_plan = full
            .tools
            .iter()
            .find(|tool| name(tool) == "exit_plan")
            .expect("exit_plan stays in the array outside plan mode");
        assert!(!tool_offer::mark_of(exit_plan).callable);
        assert!(
            full.tools
                .iter()
                .all(|tool| !tool_offer::mark_of(tool).deferred)
        );
        assert_eq!(*tool_offer::callable(&full.tools), offer[..]);
        let (offer, mut gated) = request(agent, vec![json!({"role":"user","content":"x"})]);
        gated.tools.retain(|tool| name(tool) == "todo");
        agent
            .wire_tool_surface(Exposure::AllowedTools, &offer, &mut gated)
            .unwrap();
        let functions = |tools: &[Value]| -> Vec<Value> {
            tools.iter().map(|tool| tool["function"].clone()).collect()
        };
        assert_eq!(functions(&full.tools), functions(&gated.tools));
        assert_eq!(tool_offer::allowed_names(&gated.tools), Some(vec!["todo"]));
    });
}

#[test]
fn plain_routes_and_hermetic_runs_are_unchanged() {
    with_agent(HOSTED, |agent| {
        let (offer, mut chat) = request(agent, loaded_history("text"));
        let before = chat.clone();
        agent
            .wire_tool_surface(Exposure::Plain, &offer, &mut chat)
            .unwrap();
        assert_eq!(chat.tools, before.tools);
        assert_eq!(chat.messages, before.messages);
        agent.hermetic = true;
        for exposure in [Exposure::Deferred, Exposure::AllowedTools] {
            agent
                .wire_tool_surface(exposure, &offer, &mut chat)
                .unwrap();
            assert_eq!(chat.tools, before.tools);
        }
    });
}
