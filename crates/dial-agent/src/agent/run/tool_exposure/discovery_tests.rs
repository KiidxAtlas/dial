use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::tools::integrations::ext::{Tool, ToolContext};

/// A deferred catalog tool with the given parameter names; never executed.
struct Probe {
    name: &'static str,
    properties: &'static [&'static str],
}
impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> Value {
        let properties: serde_json::Map<String, Value> = self
            .properties
            .iter()
            .map(|key| ((*key).to_owned(), json!({"type":"string"})))
            .collect();
        json!({"type":"function","function":{"name":self.name,"description":"Probe tool",
            "parameters":{"type":"object","properties":properties,"additionalProperties":false}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("discovery must never execute a tool")
    }
}

fn register(agent: &mut Agent<'_>, name: &'static str, properties: &'static [&'static str]) {
    agent.ext.register(Arc::new(Probe { name, properties }));
}

fn bash(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".into(),
        name: "bash".into(),
        arguments,
        malformed_arguments: false,
    }
}

/// A key the offered tool rejects routes to the unoffered catalog tool that declares it; keys no
/// catalog tool declares, valid calls and unloaded owners route nowhere.
#[test]
fn rejected_arguments_route_to_the_unoffered_tool_that_declares_them() {
    with_agent("", |agent| {
        register(agent, "zprobe_owner", &["command", "frobnicate"]);
        assert!(!agent.offered().contains(&"zprobe_owner".to_owned()));
        let offer = agent.schemas(&agent.offered()).unwrap();
        let routed =
            agent.argument_owners(&bash(json!({"command":"ls","frobnicate":"yes"})), &offer);
        assert_eq!(
            routed,
            Some((
                vec!["frobnicate".to_owned()],
                vec!["zprobe_owner".to_owned()]
            ))
        );
        assert_eq!(
            agent.argument_owners(&bash(json!({"command":"ls","nobody_has":"x"})), &offer),
            None
        );
        assert_eq!(
            agent.argument_owners(&bash(json!({"command":"ls"})), &offer),
            None
        );
        agent.tool_routing.unload(&["zprobe_owner".to_owned()]);
        assert_eq!(
            agent.argument_owners(&bash(json!({"command":"ls","frobnicate":"yes"})), &offer),
            None
        );
    });
}

/// The bash description names `bg` as a Dial tool to load only when it is not offered and
/// `tool_search` is.
#[test]
fn bash_description_says_to_load_bg_only_when_it_is_loadable() {
    const NOTE: &str = "`bg` is a Dial tool, not a shell command, and is not offered yet";
    let description = |agent: &Agent<'_>| {
        let schemas = agent.schemas(&["bash".to_owned()]).unwrap();
        schemas[0]["function"]["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    with_agent("", |agent| {
        assert!(agent.offered().contains(&"bg".to_owned()));
        assert!(!description(agent).contains(NOTE));
        // The small local tier offers a fixed minimal set without `bg`.
        agent.lean_tools = true;
        assert!(
            !agent.offered().contains(&"bg".to_owned()),
            "{:?}",
            agent.offered()
        );
        assert!(agent.offered().contains(&"tool_search".to_owned()));
        assert!(description(agent).contains(NOTE), "{}", description(agent));
    });
}
