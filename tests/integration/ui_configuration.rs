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
fn small_setting_persists_and_project_blank_follows_main() {
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
    assert_eq!(global.architecture_config().model, "local-summary");
    assert!(global.architecture_config().endpoint.is_none());
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[set("codebase.architecture_model", "")],
    )
    .unwrap();
    let project = Config::load(&layout, None).unwrap();
    assert_eq!(
        project.architecture_config().selection(),
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
            set("model_roles.small.provider", "endpoint"),
            set("model_roles.small.model", "hosted-helper"),
            set("model_roles.small.base_url", "https://provider.example/v1"),
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
            set("model_roles.small.provider", "mlx"),
            set("model_roles.small.model", "local-helper"),
        ],
    )
    .unwrap();
    let after = Config::load(&layout, None).unwrap();
    assert!(
        after.endpoint.is_none(),
        "a local model must not be sent to the inherited hosted endpoint"
    );
    assert!(after.for_role("small").endpoint.is_none());
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
            set("model_roles.coach.model", "helper"),
            set("model_roles.coach.provider", "endpoint"),
            set("model_roles.coach.base_url", "https://first.example/v1"),
        ],
    )
    .unwrap();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            set("endpoint.base_url", "http://127.0.0.1:1234/v1"),
            set("endpoint.api_key_env", ""),
            set("model_roles.coach.provider", "inherit"),
            set("model_roles.coach.model", ""),
        ],
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(config.endpoint.as_ref().unwrap().api_key_env.is_none());
    assert!(!config.model_roles.contains_key("coach"));
    assert_eq!(config.for_role("coach").model, "main");
    assert_eq!(
        config.for_role("coach").endpoint.unwrap().base_url,
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
    assert!(!config.for_role("small").adaptive.enabled);
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
    for role in ["small", "coach"] {
        let auxiliary = config.for_role(role);
        assert!(!auxiliary.steering.enabled && !auxiliary.steering.automatic);
    }
}
