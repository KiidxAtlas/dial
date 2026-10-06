use dial::{
    config::types::Config,
    storage::paths::Layout,
    storage::settings_store::{self, Change, Scope},
};
use std::fs;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    (home, project, layout)
}

fn set(path: &str, value: impl Into<toml::Value>) -> Change {
    Change::Set {
        path: path.into(),
        value: value.into(),
    }
}

#[test]
fn architecture_setting_persists_and_project_blank_follows_main() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("model", "hosted-model"),
            set("provider", "endpoint"),
            set("endpoint.base_url", "https://provider.example/v1"),
            set("codebase.architecture_model", "local-summary"),
        ],
    )
    .unwrap();
    let global = Config::load(&layout, None).unwrap();
    assert_eq!(global.for_role("architecture").model, "local-summary");
    assert!(global.for_role("architecture").endpoint.is_none());
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[set("codebase.architecture_model", "")],
    )
    .unwrap();
    let project = Config::load(&layout, None).unwrap();
    assert_eq!(
        project.for_role("architecture").selection(),
        project.selection()
    );
    assert_eq!(global.codebase.architecture_model, "local-summary");
}

#[test]
fn project_local_model_overrides_inherited_remote_routing_and_credentials() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("model", "hosted-model"),
            set("provider", "endpoint"),
            set("endpoint.base_url", "https://provider.example/v1"),
            set("endpoint.api_key_env", "CLOUD_KEY"),
            set("model_roles.subagent.provider", "endpoint"),
            set("model_roles.subagent.model", "hosted-helper"),
            set(
                "model_roles.subagent.base_url",
                "https://provider.example/v1",
            ),
        ],
    )
    .unwrap();
    let before = Config::load(&layout, None).unwrap();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            set("model", "local-checkpoint"),
            set("provider", "mlx"),
            Change::Remove {
                path: "endpoint".into(),
            },
            set("model_roles.subagent.provider", "mlx"),
            set("model_roles.subagent.model", "local-helper"),
        ],
    )
    .unwrap();
    let after = Config::load(&layout, None).unwrap();
    assert!(
        after.endpoint.is_none(),
        "a local model must not be sent to the inherited hosted endpoint"
    );
    assert!(after.for_role("subagent").endpoint.is_none());
    assert_ne!(
        before.state_dir, after.state_dir,
        "adapters and learning history are model-scoped"
    );
    assert_eq!(after.selection().provider, "mlx");
    let global: toml::Table = fs::read_to_string(layout.config_files()[0].clone())
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(global["model"].as_str(), Some("hosted-model"));
}

#[test]
fn invalid_setting_does_not_modify_disk_and_removals_validate_future_state() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(&layout, Scope::Global, &[set("model", "local-checkpoint")])
        .unwrap();
    let path = &layout.config_files()[0];
    let before = fs::read(path).unwrap();
    assert!(
        settings_store::save_config(&layout, Scope::Global, &[set("context_window", 0)]).is_err()
    );
    assert_eq!(fs::read(path).unwrap(), before);
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("provider", "endpoint"),
            set("endpoint.base_url", "https://provider.example/v1"),
        ],
    )
    .unwrap();
    let before = fs::read(path).unwrap();
    assert!(
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[Change::Remove {
                path: "model".into()
            }]
        )
        .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn inherited_roles_and_credentials_can_be_cleared_at_project_scope() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("model", "main"),
            set("endpoint.base_url", "https://first.example/v1"),
            set("endpoint.api_key_env", "OLD_PROVIDER_KEY"),
            set("model_roles.feedback.model", "helper"),
            set("model_roles.feedback.provider", "endpoint"),
            set("model_roles.feedback.base_url", "https://first.example/v1"),
        ],
    )
    .unwrap();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            set("endpoint.base_url", "http://127.0.0.1:1234/v1"),
            set("endpoint.api_key_env", ""),
            set("model_roles.feedback.provider", "inherit"),
            set("model_roles.feedback.model", ""),
        ],
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(config.endpoint.as_ref().unwrap().api_key_env.is_none());
    assert!(!config.model_roles.contains_key("feedback"));
    assert_eq!(config.for_role("feedback").model, "main");
    assert_eq!(
        config.for_role("feedback").endpoint.unwrap().base_url,
        "http://127.0.0.1:1234/v1"
    );
}

#[test]
fn adaptive_controls_persist_layered_pins_and_reject_invalid_budget_without_writing() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("model", "local-model"),
            set("adaptive.sampling", true),
            set("adaptive.editing_temperature", 0.25),
        ],
    )
    .unwrap();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            set("adaptive.pin_sampling", true),
            set("adaptive.phase", "verification"),
        ],
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(config.adaptive.sampling && config.adaptive.pin_sampling);
    assert_eq!(config.adaptive.editing_temperature, 0.25);
    assert_eq!(config.adaptive.phase, dial::adaptive::Phase::Verification);
    assert!(
        settings_store::save_config(
            &layout,
            Scope::Project,
            &[set("adaptive.verification_tokens", 0)]
        )
        .is_err()
    );
    assert_eq!(
        Config::load(&layout, None)
            .unwrap()
            .adaptive
            .verification_tokens,
        1536
    );
    assert!(!config.for_role("architecture").adaptive.enabled);
}

#[test]
fn automatic_steering_setting_persists_and_auxiliary_roles_remain_unsteered() {
    let (_home, _project, layout) = fixture();
    settings_store::save_config(&layout, Scope::Project, &[set("steering.automatic", false)])
        .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(!config.steering.automatic);
    settings_store::save_config(&layout, Scope::Project, &[set("steering.automatic", true)])
        .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(config.steering.automatic);
    for role in ["architecture", "compaction", "feedback"] {
        let auxiliary = config.for_role(role);
        assert!(!auxiliary.steering.enabled && !auxiliary.steering.automatic);
    }
}
