use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::tools::integrations::{
    ext::{Tool, ToolContext},
    plugin::Plugin,
};
use std::sync::atomic::AtomicUsize;

struct FixturePlugin {
    capabilities: Vec<&'static str>,
}
impl Plugin for FixturePlugin {
    fn name(&self) -> &str {
        "fixture"
    }
    fn events(&self) -> &[String] {
        &[]
    }
    fn negotiated(&self) -> Negotiated {
        Negotiated {
            version: 2,
            capabilities: self.capabilities.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
    fn on_event(&self, _: &str, _: &Value, _: &AtomicBool) -> Result<Option<Value>> {
        Ok(None)
    }
}
fn host(capabilities: &[&'static str]) -> Host {
    let host = Host::default();
    host.register(Arc::new(FixturePlugin {
        capabilities: capabilities.to_vec(),
    }))
    .unwrap();
    host
}
struct DenyUi;
impl Ui for DenyUi {}

struct Action(Arc<AtomicUsize>);
impl Tool for Action {
    fn name(&self) -> &str {
        "fixture_action"
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":"fixture_action","description":"Test action.","parameters":{"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext<'_>, _: &Value) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok("executed".to_owned())
    }
}

#[test]
fn admission_rejects_unchanged_invalid_arguments_before_approval_or_execution() {
    with_agent("", |agent| {
        let host = host(&["host.tools"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        agent.ext.register(Arc::new(Action(count.clone())));
        agent.enabled_tools.insert("fixture_action".into());
        let mut backend = agent.backend.take().unwrap();
        for arguments in [
            json!({}),
            json!({"count":"1"}),
            json!({"count":0}),
            json!({"count":1,"extra":true}),
        ] {
            let error = agent
                .extension_dispatch(
                    &mut backend,
                    &mut DenyUi,
                    &owner,
                    "fixture_action",
                    arguments.clone(),
                    false,
                )
                .unwrap_err();
            assert!(!error.to_string().is_empty(), "{arguments}");
        }
        let denied = agent
            .extension_dispatch(
                &mut backend,
                &mut DenyUi,
                &owner,
                "fixture_action",
                json!({"count":1}),
                false,
            )
            .unwrap();
        assert!(
            denied.is_error,
            "approval denial remains a structured tool result"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        agent.backend = Some(backend);
    });
}

#[test]
fn callbacks_require_capabilities_and_current_registration_even_for_metadata() {
    with_agent("", |agent| {
        let host = host(&[]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        let error = agent
            .extension_callback(&mut backend, &mut DenyUi, &owner, "context.get", &json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("host.context"));
        assert!(host.unregister("fixture"));
        host.register(Arc::new(FixturePlugin {
            capabilities: vec!["host.context"],
        }))
        .unwrap();
        assert!(
            owner
                .require("host.context")
                .unwrap_err()
                .to_string()
                .contains("obsolete")
        );
        agent.backend = Some(backend);
    });
}

#[test]
fn task_queries_cannot_operate_on_another_extension_or_generation() {
    with_agent("", |agent| {
        let host = host(&["host.tasks"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        agent.extension_jobs.insert(
            (
                agent.session.id().to_owned(),
                "someone-else".into(),
                owner.generation,
            ),
            ["task-1".to_owned()].into(),
        );
        agent.extension_jobs.insert(
            (
                agent.session.id().to_owned(),
                "fixture".into(),
                owner.generation + 1,
            ),
            ["task-2".to_owned()].into(),
        );
        let mut backend = agent.backend.take().unwrap();
        for method in ["tasks.status", "tasks.cancel"] {
            for id in ["task-1", "task-2", "unknown"] {
                let error = agent
                    .extension_callback(
                        &mut backend,
                        &mut DenyUi,
                        &owner,
                        method,
                        &json!({"id":id}),
                    )
                    .unwrap_err();
                assert!(
                    error.to_string().contains("not owned"),
                    "{method} {id}: {error}"
                );
            }
        }
        agent.backend = Some(backend);
    });
}

#[test]
fn model_admission_declines_without_contacting_provider_and_checks_schema_first() {
    with_agent("", |agent| {
        let host = host(&["host.models"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        for (params, expected) in [
            (json!({"prompt":"hello"}), "declined"),
            (
                json!({"prompt":"hello","schema":{"unevaluatedProperties":false}}),
                "unsupported",
            ),
            (json!({"prompt":"hello","max_tokens":5000}), "1–4096"),
            (json!({"prompt":"hello","model":"other"}), "active model"),
        ] {
            let error = agent
                .extension_callback(&mut backend, &mut DenyUi, &owner, "models.call", &params)
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{params}: {error:#}");
        }
        agent.backend = Some(backend);
    });
}

#[test]
fn structured_model_results_validate_json_and_preserve_text() {
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    for text in ["not json", "{}", "{\"ok\":1}", "{\"ok\":true,\"extra\":1}"] {
        assert!(model_result(text.into(), Some(&schema)).is_err(), "{text}");
    }
    let text = "{\"ok\":true}";
    assert_eq!(
        model_result(text.into(), Some(&schema)).unwrap(),
        json!({"text":text,"data":{"ok":true}})
    );
}

#[test]
fn cancellation_and_nested_dispatch_fail_without_running_tools() {
    with_agent("", |agent| {
        let host = host(&["host.tools"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        for name in [
            "task",
            "side_request",
            "eval",
            "ext__fixture__tool",
            "solve",
        ] {
            let error = agent
                .extension_tool(&mut backend, &mut DenyUi, &owner, name, json!({}))
                .unwrap_err();
            assert!(
                error.to_string().contains("nested execution"),
                "{name}: {error}"
            );
        }
        agent.interrupt.store(true, Ordering::Release);
        let error = agent
            .extension_callback(&mut backend, &mut DenyUi, &owner, "tools.list", &json!({}))
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        agent.interrupt.store(false, Ordering::Release);
        agent.backend = Some(backend);
    });
}

#[test]
fn output_envelope_preserves_partial_effects_and_structured_data_on_errors() {
    let mut output = ToolOutput::err("partial failure");
    output.data = Some(json!({"written":1}));
    output = output.with_effects(crate::runtime::execution::Effects::Unknown);
    let expected_execution = serde_json::to_value(&output.execution).unwrap();
    let value = output_json(output);
    assert_eq!(value["is_error"], true);
    assert_eq!(value["data"], json!({"written":1}));
    assert_eq!(value["execution"], expected_execution);
}

#[test]
fn context_metadata_omits_text_and_selected_messages_need_explicit_approval() {
    with_agent("", |agent| {
        agent
            .session
            .append(json!({"role":"user","content":"private user text"}))
            .unwrap();
        agent.session.append(json!({"role":"assistant","content":"private assistant text","reasoning_content":"never disclose"})).unwrap();
        let host = host(&["host.context", "host.conversation"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        let metadata = agent
            .extension_callback(&mut backend, &mut DenyUi, &owner, "context.get", &json!({}))
            .unwrap();
        assert!(!metadata.to_string().contains("private"));
        assert!(!metadata.to_string().contains("never disclose"));
        let id = metadata["conversation"][0]["id"].as_str().unwrap();
        let error = agent
            .extension_callback(
                &mut backend,
                &mut DenyUi,
                &owner,
                "context.get",
                &json!({"entries":[id]}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("declined"));
        let snapshots = conversation_snapshot(&agent.session);
        assert!(
            snapshots
                .iter()
                .any(|message| message["role"] == "assistant")
        );
        assert!(
            !serde_json::to_string(&snapshots)
                .unwrap()
                .contains("never disclose")
        );
        agent.backend = Some(backend);
    });
}

#[test]
fn profiles_cannot_add_unoffered_tools_or_enable_nested_execution() {
    let offered = ["read", "task", "ext__other__run"].map(str::to_owned);
    for (requested, expected) in [
        (vec![], vec!["read"]),
        (
            vec!["read", "write", "task", "ext__other__run"],
            vec!["read"],
        ),
        (vec!["write"], vec![]),
    ] {
        let profile = crate::runtime::extensions::Agent {
            id: "limited".into(),
            title: "Limited profile".into(),
            model: None,
            tools: requested.into_iter().map(str::to_owned).collect(),
            guidance: "Work on the requested task.".into(),
        };
        let definition =
            profile_definition("extension-limited", "fixture", &profile, &offered).unwrap();
        assert_eq!(definition.tools.unwrap(), expected);
        assert_eq!(definition.spawns, Some(vec![]));
        assert!(definition.model.is_empty());
    }
}

#[test]
fn workflow_agent_prompt_supplies_task_without_replacing_explicit_arguments() {
    for (input, task) in [
        (json!({}), "Review current changes"),
        (json!({"task":"Review parser only"}), "Review parser only"),
    ] {
        let mut input = input.as_object().unwrap().clone();
        input.insert("action".into(), json!("start"));
        input.insert("profile".into(), json!("unregistered"));
        let arguments =
            workflow_task_arguments(Some("reviewer"), Some("Review current changes"), input)
                .unwrap();
        assert_eq!(arguments["task"], task);
        assert_eq!(arguments["action"], "run");
        assert_eq!(arguments["profile"], "reviewer");
    }
    assert!(workflow_task_arguments(None, Some("review"), Default::default()).is_err());
}

struct AllowUi;
impl Ui for AllowUi {
    fn approve(&mut self, _: &str, _: &str) -> Decision {
        Decision::Allow
    }
}

#[test]
fn native_guidance_intercepts_and_latches_remaining_extension_callbacks() {
    with_agent("", |agent| {
        let rules = agent.workspace.root().join(".dial/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("action.md"), "---\ncondition: count\nscope: tool:fixture_action\n---\nInspect the action before execution.\n").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        agent.ext.register(Arc::new(Action(count.clone())));
        agent.enabled_tools.insert("fixture_action".into());
        let host = host(&["host.tools"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        let first = agent
            .extension_dispatch(
                &mut backend,
                &mut AllowUi,
                &owner,
                "fixture_action",
                json!({"count":1}),
                false,
            )
            .unwrap();
        assert!(
            first.is_error && first.text.contains("Portable guidance"),
            "{}",
            first.text
        );
        // Even a different, otherwise admitted call cannot resume this invocation.
        let later = agent
            .extension_dispatch(
                &mut backend,
                &mut AllowUi,
                &owner,
                "read",
                json!({"path":"missing.txt"}),
                false,
            )
            .unwrap();
        assert_eq!(later.text, first.text);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(agent.portable_callback_intercepted);
        agent.backend = Some(backend);
    });
}

#[test]
fn runtime_journaling_failure_refuses_extension_action_before_effects() {
    with_agent("", |agent| {
        let tree = agent.session.runtime();
        let journal = tree.stream_writer().unwrap();
        let read_only = std::fs::File::open(agent.session.path().unwrap()).unwrap();
        journal.fail_writes_for_test(read_only);
        tree.start("worker", "failed publication", None)
            .append("cannot persist");
        assert!(tree.check_persistence().is_err());
        let count = Arc::new(AtomicUsize::new(0));
        agent.ext.register(Arc::new(Action(count.clone())));
        agent.enabled_tools.insert("fixture_action".into());
        let host = host(&["host.tools"]);
        let owner = Owner::new(&host, "fixture").unwrap();
        let mut backend = agent.backend.take().unwrap();
        let output = agent
            .extension_dispatch(
                &mut backend,
                &mut AllowUi,
                &owner,
                "fixture_action",
                json!({"count":1}),
                false,
            )
            .unwrap();
        assert!(
            output.is_error && output.text.contains("before execution"),
            "{}",
            output.text
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        agent.backend = Some(backend);
    });
}

#[test]
fn extension_callback_ledger_preserves_source_and_execution_and_reports_result_failure() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let config = Config::load(
        &layout,
        Some(toml::from_str("model='test'\n[improve]\nenabled=false").unwrap()),
    )
    .unwrap();
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    let session = Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    );
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        Some(&store),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let trajectory = store
        .begin_trajectory(agent.version.id, "coding", "callback", "test")
        .unwrap();
    agent.trajectory = Some((trajectory, "coding".into(), "callback".into()));
    let count = Arc::new(AtomicUsize::new(0));
    agent.ext.register(Arc::new(Action(count.clone())));
    agent.enabled_tools.insert("fixture_action".into());
    let host = host(&["host.tools"]);
    let owner = Owner::new(&host, "fixture").unwrap();
    let mut backend = agent.backend.take().unwrap();
    struct LedgerUi<'a>(&'a Store, i64);
    impl Ui for LedgerUi<'_> {
        fn tool_call_with_args(&mut self, _: &str, _: &str, _: &str, _: &Value) {
            let events = self.0.trajectory_events(self.1).unwrap();
            assert_eq!(
                events.last().unwrap().0,
                "code_tool_call",
                "call evidence exists before admission to effects"
            );
        }
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            Decision::Allow
        }
    }
    let mut ui = LedgerUi(&store, trajectory);
    let output = agent
        .extension_dispatch(
            &mut backend,
            &mut ui,
            &owner,
            "fixture_action",
            json!({"count":1}),
            false,
        )
        .unwrap();
    assert!(!output.is_error, "{}", output.text);
    let events = store.trajectory_events(trajectory).unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.0.as_str())
            .collect::<Vec<_>>(),
        ["code_tool_call", "code_tool_result"]
    );
    assert!(
        events
            .iter()
            .all(|event| event.2["parent"] == "extension:fixture")
    );
    assert_eq!(
        events[1].2["execution"],
        serde_json::to_value(&output.execution).unwrap()
    );
    let db = rusqlite::Connection::open(config.state_dir.join("state.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_callback_result BEFORE INSERT ON events WHEN NEW.kind = 'code_tool_result' BEGIN SELECT RAISE(FAIL, 'callback result persistence regression'); END;").unwrap();
    let output = agent
        .extension_dispatch(
            &mut backend,
            &mut ui,
            &owner,
            "fixture_action",
            json!({"count":1}),
            false,
        )
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(output.is_error && output.text.contains("inspect state before retrying"));
    assert_eq!(
        output.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    agent.backend = Some(backend);
}
