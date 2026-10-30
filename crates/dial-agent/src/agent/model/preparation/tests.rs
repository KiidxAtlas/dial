use super::*;
use crate::llm::provider::ProviderKind;

fn agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        Default::default(),
        1,
    );
    let mut agent = Agent::hermetic(config, backend, &version, root, None, None).unwrap();
    agent.hermetic = false;
    agent.session_lifecycle = true;
    agent.approval = config.approval;
    agent
}

#[test]
fn preparation_is_inert_by_default_and_yields_to_foreground_or_sealed_runs() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let a = agent(&config, root.path());
    assert_eq!(a.preparation_action(false), None);
    drop(a);
    let mut config = config;
    config.preparation.local_prefix = true;
    config.preparation.cargo_check = true;
    let mut a = agent(&config, root.path());
    assert_eq!(a.preparation_action(false), Some(Action::LocalPrefix));
    assert_eq!(a.preparation_action(true), None);
    for gate in ["hermetic", "plan", "lifecycle", "interrupt"] {
        a.hermetic = gate == "hermetic";
        a.plan_mode = gate == "plan";
        a.session_lifecycle = gate != "lifecycle";
        a.interrupt.store(gate == "interrupt", Ordering::SeqCst);
        assert_eq!(a.preparation_action(false), None, "{gate}");
    }
}

#[test]
fn failed_startup_work_is_consumed_once_and_does_not_create_a_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.preparation.cargo_check = true;
    let mut a = agent(&config, root.path());
    assert!(a.prepare_session_idle(&mut NullUi).unwrap());
    assert!(!a.prepare_session_idle(&mut NullUi).unwrap());
    assert_eq!(a.session.message_count(), 0);
    assert!(a.backend.is_some());
    assert!(!a.interrupt.load(Ordering::SeqCst));
    assert!(!a.compaction_runtime.has_live_worker());
}

#[test]
fn prefix_uses_session_context_without_history_and_config_change_invalidates_attempts() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.preparation.local_prefix = true;
    let mut a = agent(&config, root.path());
    a.system_messages = vec![json!({"role":"system","content":"session prefix"})];
    a.agents_messages = vec![json!({"role":"system","content":"project context"})];
    let request = a.startup_prefix_request().unwrap();
    assert_eq!(request.max_tokens, 1);
    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0]["content"], "session prefix");
    assert_eq!(request.messages[1]["content"], "project context");
    a.preparation.identity = a.preparation_identity();
    a.preparation.attempted.insert(Action::LocalPrefix.key());
    assert_eq!(a.preparation_action(false), None);
    a.preparation.identity = "old model/root/session".into();
    assert_eq!(a.preparation_action(false), Some(Action::LocalPrefix));
    assert_eq!(a.session.message_count(), 0);
}

#[test]
fn local_warming_discards_reply_and_preserves_backend_without_a_conversation_turn() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let event = json!({"choices":[{"delta":{"content":"discard me"},"finish_reason":"stop"}]});
    let server = llm::fake::serve(vec![llm::fake::Script::Sse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    ))]);
    let mut a = agent(&config, root.path());
    a.hermetic = true; // No user telemetry, memory, hooks or persistence in this fixture.
    a.backend = Some(Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "fixture".into(),
            api_key: None,
        },
        Default::default(),
        5,
    ));
    let interrupt = Arc::new(AtomicBool::new(false));
    a.prepare_local_prefix(&mut NullUi, &interrupt).unwrap();
    assert!(a.backend.is_some());
    assert_eq!(a.session.message_count(), 0);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let payload = llm::fake::body(&requests[0]);
    assert_eq!(payload["max_tokens"], 1);
    assert!(!payload["messages"].to_string().contains("discard me"));
}

#[test]
fn configured_cargo_preparation_keeps_ordinary_shell_approval() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut config = Config::default();
    config.approval = crate::config::types::Approval::Ask;
    let mut a = agent(&config, root.path());
    struct Deny(usize);
    impl Ui for Deny {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.0 += 1;
            Decision::Deny
        }
    }
    let mut deny = Deny(0);
    let error = a
        .prepare_startup_tool(&Action::Cargo, 1, &mut deny)
        .expect_err("denied Cargo preparation must not start");
    assert_eq!(
        deny.0, 1,
        "request stopped before the approval surface: {error:#}"
    );
    assert!(!root.path().join("target").exists());
    assert!(a.backend.is_some());
    assert_eq!(a.session.message_count(), 0);
}
