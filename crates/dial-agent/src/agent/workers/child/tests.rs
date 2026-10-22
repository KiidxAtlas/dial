use super::*;
use crate::{
    agent::{safety::approval::Denial, workers::subagents},
    config::types::{Config, Harness},
    tools::{
        Workspace,
        integrations::ext::{Registry, Tool},
    },
};
use std::sync::{Arc, atomic::AtomicBool};

struct Fake {
    name: &'static str,
    asks: bool,
}

impl Tool for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,"description":"fake","parameters":{"type":"object","properties":{}}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        self.asks
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        ToolOutput {
            execution: Default::default(),
            data: None,
            text: format!("{} ran", self.name),
            is_error: false,
        }
    }
}

struct Reviewer {
    allow: bool,
    asked: Vec<String>,
}

impl Approver for Reviewer {
    fn authorize(
        &mut self,
        call: &llm::ToolCall,
        summary: &str,
        needs_approval: bool,
    ) -> Result<(), approval::Denial> {
        if !needs_approval {
            return Ok(());
        }
        self.asked.push(format!("{}: {summary}", call.name));
        if self.allow {
            Ok(())
        } else {
            Err(Denial::User)
        }
    }
}

/// Run one call as a child of type `agent`, with a harmless and an approval-needing extension tool installed.
fn call_as(agent: &str, tool: &str, allow: bool) -> (ToolOutput, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (config, mut harness, interrupt) = (
        Config::default(),
        Harness::default(),
        Arc::new(AtomicBool::new(false)),
    );
    // The parent has explicitly loaded these specialists before delegating.
    harness
        .tools
        .extend(["fixture_lookup".into(), "deploy".into()]);
    let mut ext = Registry::new();
    ext.register(Arc::new(Fake {
        name: "fixture_lookup",
        asks: false,
    }));
    ext.register(Arc::new(Fake {
        name: "deploy",
        asks: true,
    }));
    let env = Env {
        config: &config,
        workspace: &workspace,
        ext: &ext,
        layout: None,
        harness: &harness,
        interrupt: &interrupt,
        usage: None,
        context_cap: None,
        memory: None,
        agents_messages: &[],
        plan_mode: false,
    };
    let request = subagents::parse(
        &json!({"agent": agent, "task": "p", "solution_space": "s"}),
        &config,
    )
    .ok()
    .unwrap();
    let job = &request.jobs[0];
    let names = toolset(job.spec, &harness.tools, &ext);
    let schemas = schemas(env, &names).unwrap();
    let call = llm::ToolCall {
        id: "1".into(),
        name: tool.into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let mut reviewer = Reviewer {
        allow,
        asked: Vec::new(),
    };
    (
        call_tool(env, job, &schemas, &names, &call, &mut reviewer, None, None),
        reviewer.asked,
    )
}

#[test]
fn read_only_agents_use_extension_tools_that_need_no_approval_and_nothing_that_does() {
    let (out, asked) = call_as("scout", "fixture_lookup", false);
    assert_eq!(
        (out.text.as_str(), out.is_error, asked.len()),
        ("fixture_lookup ran", false, 0)
    );
    let (out, asked) = call_as("scout", "deploy", true);
    assert!(
        out.is_error && out.text.contains("unknown or unavailable tool: deploy"),
        "{}",
        out.text
    );
    assert!(asked.is_empty());
}

#[test]
fn a_workers_approval_needing_extension_call_goes_to_the_parents_approver() {
    let (out, asked) = call_as("worker", "deploy", true);
    assert_eq!((out.text.as_str(), out.is_error), ("deploy ran", false));
    assert!(
        asked.len() == 1 && asked[0].starts_with("deploy: [worker]"),
        "{asked:?}"
    );
    let (out, _) = call_as("worker", "deploy", false);
    assert!(
        out.is_error && out.text.starts_with("denied by the user"),
        "{}",
        out.text
    );
}

#[test]
fn tools_outside_the_allowlist_and_nested_tasks_are_refused() {
    let (out, asked) = call_as("planner", "write", true);
    assert!(
        out.is_error && out.text.contains("unknown or unavailable tool: write"),
        "{}",
        out.text
    );
    let (out, _) = call_as("worker", "task", true);
    assert!(
        out.is_error && out.text.contains("unknown or unavailable tool: task"),
        "{}",
        out.text
    );
    assert!(asked.is_empty());
}

#[cfg(unix)]
#[test]
fn rewritten_child_reads_recheck_schema_and_path_authority() {
    struct PathReviewer(Vec<String>);
    impl Approver for PathReviewer {
        fn authorize(
            &mut self,
            call: &llm::ToolCall,
            _: &str,
            _: bool,
        ) -> Result<(), approval::Denial> {
            let path = call.arguments["path"].as_str().unwrap().to_owned();
            self.0.push(path.clone());
            if path == "denied.txt" {
                Err(Denial::Rule)
            } else {
                Ok(())
            }
        }
    }
    for (replacement, expected, authorized) in [
        (json!({"path":"allowed.txt"}), "allowed evidence", 2),
        (json!({"path":"denied.txt"}), "denied", 2),
        (
            json!({"path":"allowed.txt", "offset":"invalid"}),
            "fails its schema",
            1,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("allowed.txt"), "allowed evidence\n").unwrap();
        std::fs::write(dir.path().join("denied.txt"), "secret evidence\n").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let mut config = Config::default();
        config.hooks.push(crate::config::types::HookConfig {
            event: "pre_tool_use".into(),
            matcher: Some("read".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "cat >/dev/null; printf '%s' '{}'",
                    json!({"input":replacement})
                ),
            ],
            timeout_secs: 5,
            fail_closed: None,
        });
        let harness = Harness::default();
        let ext = Registry::new();
        let interrupt = Arc::new(AtomicBool::new(false));
        let env = Env {
            config: &config,
            workspace: &workspace,
            ext: &ext,
            layout: None,
            harness: &harness,
            interrupt: &interrupt,
            usage: None,
            context_cap: None,
            memory: None,
            agents_messages: &[],
            plan_mode: false,
        };
        let request = subagents::parse(
            &json!({"agent":"scout", "task":"inspect", "solution_space":"files"}),
            &config,
        )
        .unwrap();
        let job = &request.jobs[0];
        let names = toolset(job.spec, &harness.tools, &ext);
        let schemas = schemas(env, &names).unwrap();
        let call = llm::ToolCall {
            id: "read".into(),
            name: "read".into(),
            arguments: json!({"path":"original.txt"}),
            malformed_arguments: false,
        };
        let mut reviewer = PathReviewer(Vec::new());
        let output = call_tool(env, job, &schemas, &names, &call, &mut reviewer, None, None);
        assert!(output.text.contains(expected), "{}", output.text);
        assert_eq!(output.is_error, expected != "allowed evidence");
        assert!(!output.text.contains("secret evidence"));
        assert_eq!(reviewer.0.len(), authorized);
    }
}

#[test]
fn private_carry_does_not_repeat_retained_assignment_or_coordination() {
    let brief = "Task: inspect source\nScope: worker only";
    let mut messages = vec![json!({"role":"user","content":brief,"dial_context_request":true})];
    assert_eq!(
        child_instruction_carry(&messages, "inspect source", &json!(brief)),
        json!({})
    );

    let coordination = json!("Also inspect request budgets");
    let carry = child_instruction_carry(&messages, "inspect source", &coordination);
    assert!(carry.get("original_assignment").is_none());
    assert_eq!(carry["current_instruction"], coordination);
    messages.push(json!({"role":"user","content":coordination,"dial_child_coordination":true}));
    assert_eq!(
        child_instruction_carry(&messages, "inspect source", &coordination),
        json!({})
    );
}

#[test]
fn private_carry_restores_instructions_missing_after_history_replacement() {
    let messages = vec![json!({"role":"assistant","content":"inspect source"})];
    assert_eq!(
        child_instruction_carry(&messages, "inspect source", &json!("current coordination")),
        json!({"original_assignment":"inspect source","current_instruction":"current coordination"})
    );
    assert_eq!(
        child_instruction_carry(&[], "inspect source", &Value::Null),
        json!({"original_assignment":"inspect source"})
    );
}
