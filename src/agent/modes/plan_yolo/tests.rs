use crate::agent::{Agent, NullUi};
use crate::config::types::{Config, Version};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::PathBuf;

fn endpoint() -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
}

#[test]
fn plan_yolo_approves_the_plan_and_hands_off_to_the_target() {
    let mut config = Config {
        model: "test".into(),
        ..Config::default()
    };
    config.endpoint = Some(crate::config::types::EndpointConfig {
        headers: None,
        base_url: "http://127.0.0.1:9/v1".into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    let workspace = tempfile::tempdir().unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, endpoint(), &version, workspace.path(), None, None).unwrap();
    agent.plan_yolo_startup("@main").unwrap();
    // Hermetic agents refuse plan mode through `set_plan_mode`; set the flag directly.
    agent.plan_mode = true;
    assert!(agent.plan_yolo_auto_approves());
    agent.plan_yolo_approve("PLAN", &mut NullUi).unwrap();
    assert!(!agent.plan_mode);
    assert!(
        !agent.plan_yolo_auto_approves(),
        "the arm is consumed by approval"
    );
    assert!(
        agent
            .pending_plan
            .as_deref()
            .is_some_and(|plan| plan.starts_with("PLAN"))
    );
    assert!(
        agent.modes.prewalk.handoff.is_some(),
        "execution handed to the target"
    );
}

#[test]
fn mid_session_arm_hands_off_once_and_disarm_restores_approval() {
    let mut config = Config {
        model: "test".into(),
        ..Config::default()
    };
    config.endpoint = Some(crate::config::types::EndpointConfig {
        headers: None,
        base_url: "http://127.0.0.1:9/v1".into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    let workspace = tempfile::tempdir().unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, endpoint(), &version, workspace.path(), None, None).unwrap();
    // Startup already ran for this session; `/plan-yolo` must still arm.
    agent.modes.plan_yolo.started = true;
    let target = agent.plan_yolo_arm("@main").unwrap();
    assert_eq!(agent.plan_yolo_target().as_deref(), Some(target.as_str()));
    agent.plan_mode = true;
    assert!(agent.plan_yolo_auto_approves());
    assert!(agent.plan_yolo_disarm().unwrap());
    assert!(!agent.plan_yolo_auto_approves(), "disarmed plans ask again");
    assert!(agent.plan_yolo_target().is_none());
    assert!(!agent.plan_yolo_disarm().unwrap(), "nothing left to disarm");
}
