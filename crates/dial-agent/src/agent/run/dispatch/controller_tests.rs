use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_default_agent;

#[derive(Default)]
struct Human {
    approvals: usize,
    allow: bool,
}
impl Ui for Human {
    fn interactive_input(&self) -> bool {
        true
    }
    fn approve(&mut self, _: &str, _: &str) -> Decision {
        self.approvals += 1;
        if self.allow {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }
}
fn run(agent: &mut Agent<'_>, name: &str, arguments: Value, ui: &mut dyn Ui) -> ToolOutput {
    let mut backend = agent.backend.take().unwrap();
    let output = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: "controller-regression".into(),
            name: name.into(),
            arguments,
            malformed_arguments: false,
        },
        ui,
    );
    agent.backend = Some(backend);
    output
}

#[test]
fn canonical_controller_dispatch_preserves_owners_and_approvals() {
    with_default_agent("", |agent| {
        let mut ui = Human::default();
        agent.approval = Approval::Auto;
        let catalog = agent.catalog();
        for name in [
            "tool_load",
            "skill_manage",
            "rewind",
            "side_request",
            "request_directory",
        ] {
            assert!(!catalog.iter().any(|entry| entry == name), "{name}");
        }
        let output = run(agent, "tool_search", json!({"names":["git"]}), &mut ui);
        assert!(!output.is_error, "{}", output.text);
        assert!(agent.enabled_tools.contains("git"));
        let output = run(agent, "tool_search", json!({"unload":["git"]}), &mut ui);
        assert!(!output.is_error, "{}", output.text);
        assert!(!agent.enabled_tools.contains("git"));
        let output = run(agent, "task", json!({"action":"question_status"}), &mut ui);
        assert!(!output.is_error, "{}", output.text);
        let output = run(
            agent,
            "task",
            json!({"action":"question_wait","id":"side-missing","timeout":0}),
            &mut ui,
        );
        assert!(
            output.is_error && output.text.contains("unknown side question"),
            "{}",
            output.text
        );
        for action in ["create", "update", "delete"] {
            assert!(!crate::agent::safety::approval::is_read_only(
                &agent.ext,
                "skill",
                &json!({"action":action})
            ));
        }
        assert!(crate::agent::safety::approval::is_read_only(
            &agent.ext,
            "skill",
            &json!({"action":"load"})
        ));
        agent.approval = Approval::Ask;
        let create = json!({"action":"create","name":"controller-regression","description":"A deterministic test procedure","body":"Inspect the supplied fixture and report the observed result."});
        let output = run(agent, "skill", create.clone(), &mut ui);
        assert!(output.is_error);
        assert_eq!(ui.approvals, 1);
        ui.allow = true;
        let output = run(agent, "skill", create, &mut ui);
        assert!(!output.is_error, "{}", output.text);
        let output = run(
            agent,
            "skill",
            json!({"action":"load","name":"controller-regression"}),
            &mut ui,
        );
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("Inspect the supplied fixture"));
        assert_eq!(
            ui.approvals, 2,
            "loading must not acquire mutation approval"
        );
        let outside = agent
            .workspace
            .root()
            .parent()
            .unwrap()
            .join("directory-regression");
        std::fs::create_dir(&outside).unwrap();
        ui.allow = false;
        let args = json!({"action":"directory","path":outside,"reason":"Read test fixtures"});
        let output = run(agent, "ask", args.clone(), &mut ui);
        assert!(output.is_error);
        assert!(agent.workspace.resolve(outside.to_str().unwrap()).is_err());
        ui.allow = true;
        let output = run(agent, "ask", args, &mut ui);
        assert!(!output.is_error, "{}", output.text);
        assert!(agent.workspace.resolve(outside.to_str().unwrap()).is_ok());
    });
}

#[test]
fn canonical_rewind_reaches_the_existing_lifecycle_and_checks_bounds() {
    with_default_agent("[checkpoint]\nenabled=true", |agent| {
        let mut ui = Human::default();
        let output = run(
            agent,
            "checkpoint",
            json!({"action":"start","goal":"Inspect one fixture"}),
            &mut ui,
        );
        assert!(!output.is_error, "{}", output.text);
        let pending = agent.exploration.pending.take().unwrap();
        let crate::agent::workflow::exploration::Request::Checkpoint { goal, .. } = pending else {
            panic!("not a checkpoint")
        };
        assert_eq!(goal, "Inspect one fixture");
        let output = run(
            agent,
            "checkpoint",
            json!({"action":"rewind","report":"Findings"}),
            &mut ui,
        );
        assert!(
            output.is_error && output.text.contains("No active checkpoint"),
            "{}",
            output.text
        );
    });
}

#[test]
fn member_only_selection_and_successor_denials_do_not_widen_operations() {
    with_default_agent("", |agent| {
        let mut ui = Human {
            allow: true,
            ..Default::default()
        };
        agent.modes.launch.tools = Some(["side_request".to_owned()].into());
        assert!(agent.tool_eligible("task"));
        let output = run(agent, "task", json!({"action":"question_status"}), &mut ui);
        assert!(!output.is_error, "{}", output.text);
        let output = run(agent, "task", json!({"action":"status"}), &mut ui);
        assert!(
            output.is_error && output.text.contains("unavailable"),
            "{}",
            output.text
        );
        agent.modes.launch.tools = None;
        agent
            .always
            .push(serde_json::from_value(json!({"tool":"skill","effect":"deny"})).unwrap());
        let output = run(
            agent,
            "skill",
            json!({"action":"create","name":"blocked","description":"Test","body":"Must not be written"}),
            &mut ui,
        );
        assert!(output.is_error);
        assert_eq!(ui.approvals, 0);
        agent.always.clear();
        agent.modes.vibe = true;
        let output = run(
            agent,
            "skill",
            json!({"action":"create","name":"blocked","description":"Test","body":"Must not be written"}),
            &mut ui,
        );
        assert!(output.is_error);
        assert_eq!(ui.approvals, 0);
    });
}

struct CodebaseProbe(Arc<std::sync::atomic::AtomicUsize>);
impl crate::tools::integrations::ext::Tool for CodebaseProbe {
    fn name(&self) -> &str {
        "codebase_memory"
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":"codebase_memory","description":"Synthetic saved project understanding.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["recall","remember"]},"member_only":{"type":"string","enum":["kept"]}},"required":["action","member_only"],"additionalProperties":false}}})
    }
    fn needs_approval(&self, args: &Value) -> bool {
        args["action"] == "remember"
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolOutput::ok("synthetic codebase owner".into())
    }
}

#[test]
fn backend_memory_does_not_intercept_its_codebase_facade_member() {
    with_default_agent("[memory]\nbackend='local'\nautolearn=true", |agent| {
        assert!(
            agent
                .memory_runtime
                .as_ref()
                .is_some_and(|runtime| runtime.offers_tool())
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        agent.ext.register(Arc::new(NativeMemoryProbe(
            crate::storage::memory_backend::schema(&agent.config.memory).unwrap(),
        )));
        agent.ext.register(Arc::new(CodebaseProbe(calls.clone())));
        assert!(agent.ext.is_folded("codebase_memory"));
        let schema = agent.full_schemas(&["memory".into()]).unwrap().remove(0);
        let args = json!({"store":"codebase","action":"recall","member_only":"kept"});
        crate::tools::surface::catalog::validate_arguments(&schema, &args).unwrap();
        let mut ui = Human::default();
        agent.approval = Approval::Ask;
        let output = run(agent, "memory", args, &mut ui);
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("synthetic codebase owner"));
        let output = run(
            agent,
            "memory",
            json!({"store":"codebase","action":"recall","member_only":"invalid"}),
            &mut ui,
        );
        assert!(output.is_error);
        let output = run(
            agent,
            "memory",
            json!({"store":"codebase","action":"remember","member_only":"kept"}),
            &mut ui,
        );
        assert!(output.is_error);
        assert_eq!(ui.approvals, 1);
        let output = run(
            agent,
            "memory",
            json!({"action":"recall","query":"no matching fixture"}),
            &mut ui,
        );
        assert!(
            output.is_error && output.text.contains("not offered by local"),
            "{}",
            output.text
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    });
}

struct NativeMemoryProbe(Value);
impl crate::tools::integrations::ext::Tool for NativeMemoryProbe {
    fn name(&self) -> &str {
        "memory"
    }
    fn schema(&self) -> Value {
        self.0.clone()
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("ordinary backend memory must retain its controller handler")
    }
}
