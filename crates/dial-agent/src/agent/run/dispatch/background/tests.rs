use super::*;
use crate::tools::integrations::ext::Tool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Probe(Arc<AtomicUsize>);
impl Tool for Probe {
    fn name(&self) -> &str {
        "bg_probe"
    }
    fn schema(&self) -> Value {
        crate::runtime::schema::tool_schema(
            self.name(),
            "background admission fixture",
            json!({"value":{"type":"integer"}}),
            &["value"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn supports_background(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::structured(json!({"answer":args["value"]}))
    }
}

fn backend() -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
}

fn with_agent(f: impl FnOnce(&mut Agent<'_>, Arc<AtomicUsize>)) {
    with_config(Config::default(), f);
}

fn with_config(mut config: Config, f: impl FnOnce(&mut Agent<'_>, Arc<AtomicUsize>)) {
    let root = tempfile::tempdir().unwrap();
    config.harness.tools = vec!["bg".into()];
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent = Agent::hermetic(&config, backend(), &version, root.path(), None, None).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    for tool in tools::exec::background::tools(&config) {
        agent.ext.register(tool);
    }
    agent.ext.register(Arc::new(Probe(hits.clone())));
    agent.version.harness.tools.push("bg_probe".into());
    agent.hermetic = false; // Use the live registered-tool catalog without host/session storage.
    agent.approval = Approval::Auto;
    f(&mut agent, hits);
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "background-fixture".into(),
        name: "bg".into(),
        arguments,
        malformed_arguments: false,
    }
}

#[test]
fn admitted_tool_call_returns_a_managed_job_and_structured_result() {
    with_agent(|agent, hits| {
        let launch = json!({"action":"start","name":"analysis","call":{"name":"bg_probe","arguments":{"value":42}}});
        assert!(
            agent
                .run_background_tool("bg", &launch, &mut NullUi, Some(&mut backend()))
                .is_none(),
            "nested launch must be admitted on the live controller"
        );
        let start = agent.dispatch(&mut backend(), &call(json!({"action":"start","name":"analysis","call":{"name":"bg_probe","arguments":{"value":42}}})), &mut NullUi);
        assert!(!start.is_error, "{start:?}");
        assert_eq!(start.data.as_ref().unwrap()["name"], "analysis");
        let output = agent.dispatch(
            &mut backend(),
            &call(json!({"action":"wait","name":"analysis","timeout":5})),
            &mut NullUi,
        );
        assert!(!output.is_error, "{output:?}");
        assert_eq!(output.data.as_ref().unwrap()["answer"], 42);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(agent.ext.get("bg").unwrap().awaited_work().is_empty());
    });
}

#[test]
fn invalid_or_unavailable_nested_calls_start_no_work() {
    for nested in [
        json!({"name":"bg_probe","arguments":{}}),
        json!({"name":"bg_probe","arguments":{"value":"wrong type"}}),
        json!({"name":"not_offered","arguments":{}}),
        json!({"name":"bg","arguments":{"action":"list"}}),
        json!({"name":"finish","arguments":{}}),
    ] {
        with_agent(|agent, hits| {
            let output = agent.dispatch(
                &mut backend(),
                &call(json!({"action":"start","call":nested})),
                &mut NullUi,
            );
            assert!(output.is_error, "{output:?}");
            assert_eq!(hits.load(Ordering::SeqCst), 0);
            assert!(!agent.ext.has_live_work());
        });
    }
}

#[test]
fn nested_approval_denial_cannot_be_replaced_by_bg_approval() {
    struct Answers(Vec<String>);
    impl Ui for Answers {
        fn approve(&mut self, name: &str, _: &str) -> Decision {
            self.0.push(name.into());
            if name == "bg" {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
    }
    with_agent(|agent, hits| {
        agent.approval = Approval::Ask;
        let mut ui = Answers(Vec::new());
        let output = agent.dispatch(
            &mut backend(),
            &call(json!({"action":"start","call":{"name":"bg_probe","arguments":{"value":1}}})),
            &mut ui,
        );
        assert!(output.is_error, "{output:?}");
        assert_eq!(ui.0, ["bg", "bg_probe"]);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!agent.ext.has_live_work());
    });
}

#[test]
fn disabled_background_capability_does_not_launch_a_native_mutation() {
    with_agent(|agent, hits| {
        let output = agent.dispatch(&mut backend(), &call(json!({"action":"start","call":{"name":"write","arguments":{"path":"unexpected","content":"no"}}})), &mut NullUi);
        assert!(output.is_error, "{output:?}");
        assert!(!agent.workspace.root().join("unexpected").exists());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn real_data_tool_runs_through_the_same_job_owner() {
    with_agent(|agent, _| {
        std::fs::write(agent.workspace.root().join("sample.csv"), "x\n2\n5\n").unwrap();
        for tool in tools::knowledge::data::tools(agent.config) {
            agent.ext.register(tool);
        }
        agent.version.harness.tools.push("data".into());
        agent.enabled_tools.insert("data".into());
        let launch = call(
            json!({"action":"start","name":"stats","call":{"name":"data","arguments":{"path":"sample.csv","action":"aggregate","columns":["x"]}}}),
        );
        let started = agent.dispatch(&mut backend(), &launch, &mut NullUi);
        assert!(!started.is_error, "{started:?}");
        let output = agent.dispatch(
            &mut backend(),
            &call(json!({"action":"wait","name":"stats","timeout":5})),
            &mut NullUi,
        );
        assert!(!output.is_error, "{output:?}");
        assert_eq!(
            output.data.as_ref().unwrap()["groups"][0]["statistics"]["x"]["sum"],
            7.0
        );
        assert!(output.process_outcome().is_none());
    });
}

#[test]
fn completed_result_hooks_fail_closed_before_background_execution() {
    let mut config = Config::default();
    config.hooks.push(crate::config::types::HookConfig {
        event: "post_tool_use".into(),
        matcher: Some("bg_probe".into()),
        command: vec!["must-not-start".into()],
        timeout_secs: 5,
        fail_closed: Some(true),
    });
    with_config(config, |agent, hits| {
        let output = agent.dispatch(
            &mut backend(),
            &call(json!({"action":"start","call":{"name":"bg_probe","arguments":{"value":1}}})),
            &mut NullUi,
        );
        assert!(
            output.is_error && output.text.contains("post_tool_use"),
            "{output:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!agent.ext.has_live_work());
    });
}
