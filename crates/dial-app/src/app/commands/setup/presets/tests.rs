use super::*;
use crate::config::paths::Layout;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Layout, Config) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let config = Config::load(&layout, None).unwrap();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &crate::config::selection::changes(
            &config,
            &ModelSelection::new("mlx", "chosen-local"),
            "main",
        )
        .unwrap(),
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    (home, project, layout, config)
}

fn remote() -> ModelSelection {
    ModelSelection {
        base_url: Some("https://models.example/v1".into()),
        ..ModelSelection::new("endpoint", "chosen-coach")
    }
}

#[test]
fn local_setup_persists_all_text_roles_and_masks_inherited_fallbacks() {
    for scope in [Scope::Global, Scope::Project] {
        let (_home, _project, layout, config) = fixture();
        let mut inherited = crate::config::selection::changes(&config, &remote(), "coach").unwrap();
        inherited.push(Change::Set {
            path: "retry.fallback_chains".into(),
            value: toml::toml! { main = ["endpoint/remote"] }.into(),
        });
        settings_store::save_config(&layout, Scope::Global, &inherited).unwrap();
        let config = Config::load(&layout, None).unwrap();
        let main = config.selection();
        let edits = changes(&config, Preset::Local).unwrap();
        settings_store::save_config_checked(&layout, scope, &edits, &toml::Table::new(), |cfg| {
            validate(cfg, Preset::Local, &main, &main)
        })
        .unwrap();
        let saved = Config::load(&layout, None).unwrap();
        validate(&saved, Preset::Local, &main, &main).unwrap();
        assert_eq!(saved.approval, config.approval);
        assert_eq!(saved.sandbox.mode, config.sandbox.mode);
        assert!(saved.edit.enforce_seen_lines);
    }
}

#[test]
fn assisted_setup_requires_explicit_coach_and_preserves_readonly() {
    let (_home, _project, layout, mut config) = fixture();
    assert!(changes(&config, Preset::Assisted).is_err());
    config.model_roles.insert("coach".into(), remote());
    for approval in [Approval::Auto, Approval::Readonly] {
        config.approval = approval;
        let edits = changes(&config, Preset::Assisted).unwrap();
        let future =
            settings_store::preview_config(&layout, Scope::Project, &edits, &toml::Table::new())
                .unwrap();
        validate(&future, Preset::Assisted, &config.selection(), &remote()).unwrap();
        assert_eq!(
            future.approval,
            if approval == Approval::Readonly {
                Approval::Readonly
            } else {
                Approval::Ask
            }
        );
    }
}

#[test]
fn conflicting_project_or_cli_override_refuses_before_publication() {
    let (_home, _project, layout, config) = fixture();
    let main = config.selection();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &crate::config::selection::changes(&config, &remote(), "coach").unwrap(),
    )
    .unwrap();
    let edits = changes(&config, Preset::Local).unwrap();
    let before = settings_store::load_layer(&layout, Scope::Global).unwrap();
    let check = |cfg: &Config| validate(cfg, Preset::Local, &main, &main);
    assert!(
        settings_store::save_config_checked(
            &layout,
            Scope::Global,
            &edits,
            &toml::Table::new(),
            check
        )
        .is_err()
    );
    assert_eq!(
        settings_store::load_layer(&layout, Scope::Global).unwrap(),
        before
    );
    let before = settings_store::load_layer(&layout, Scope::Project).unwrap();
    let overlay = toml::toml! { [escalation] enabled = true };
    assert!(
        settings_store::save_config_checked(&layout, Scope::Project, &edits, &overlay, check)
            .is_err()
    );
    assert_eq!(
        settings_store::load_layer(&layout, Scope::Project).unwrap(),
        before
    );
}

#[test]
fn hosted_setup_uses_only_the_already_selected_hosted_model() {
    let (_home, _project, layout, config) = fixture();
    assert!(changes(&config, Preset::Hosted).is_err());
    settings_store::save_config(
        &layout,
        Scope::Project,
        &crate::config::selection::changes(&config, &remote(), "main").unwrap(),
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    let main = config.selection();
    let edits = changes(&config, Preset::Hosted).unwrap();
    let future =
        settings_store::preview_config(&layout, Scope::Project, &edits, &toml::Table::new())
            .unwrap();
    validate(&future, Preset::Hosted, &main, &main).unwrap();
    assert_eq!(future.approval, config.approval);
}

#[test]
fn locality_uses_the_endpoint_not_a_provider_label() {
    let config = Config::default();
    let mut model = ModelSelection::new("ollama", "configured");
    assert!(local(&config, &model));
    model.base_url = Some("https://models.example".into());
    assert!(!local(&config, &model));
    model.base_url = Some("http://192.168.1.2:11434".into());
    assert!(!local(&config, &model));
    model.base_url = Some("http://[::1]:11434".into());
    assert!(local(&config, &model));
}
