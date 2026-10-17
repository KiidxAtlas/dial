use super::*;
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    (home, project, layout)
}

#[test]
fn home_workspace_loads_and_saves_existing_user_configuration() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    save(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "model".into(),
            value: "home-model".into(),
        }],
    )
    .unwrap();
    let loaded = crate::config::types::Config::load(&layout, None).unwrap();
    assert_eq!(loaded.model, "home-model");
    save(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "model".into(),
            value: "project-model".into(),
        }],
    )
    .unwrap();
    let loaded = crate::config::types::Config::load(&layout, None).unwrap();
    assert_eq!(loaded.model, "project-model");
}

#[test]
fn model_assignment_rejects_invalid_project_without_changing_global() {
    let (_home, _project, layout) = fixture();
    save_config(
        &layout,
        Scope::Global,
        &[Change::Set {
            path: "model".into(),
            value: "original".into(),
        }],
    )
    .unwrap();
    let original = fs::read(&layout.config_files()[0]).unwrap();
    save_config(&layout, Scope::Project, &[]).unwrap();
    write_private(&layout.config_files()[1], b"not valid TOML [").unwrap();
    assert!(
        save_model_config(
            &layout,
            Scope::Global,
            &[Change::Set {
                path: "model".into(),
                value: "replacement".into(),
            }],
            "main"
        )
        .is_err()
    );
    assert_eq!(fs::read(&layout.config_files()[0]).unwrap(), original);
}

#[test]
fn another_sessions_model_write_does_not_retarget_a_pinned_session() {
    let (_home, _project, layout) = fixture();
    let set = |path: &str, value: &str| Change::Set {
        path: path.into(),
        value: value.into(),
    };
    save_config(
        &layout,
        Scope::Global,
        &[set("model", "mine"), set("service_tier", "flex")],
    )
    .unwrap();
    let overlay = toml::Table::new();
    let mut pin = SessionModels::capture(&load_merged(&layout, &overlay).unwrap());
    // Another terminal picks a model and edits an unrelated setting.
    save_model_config(&layout, Scope::Global, &[set("model", "theirs")], "main").unwrap();
    save_config(&layout, Scope::Global, &[set("service_tier", "priority")]).unwrap();
    let config = load_session_config(&layout, &overlay, Some(&pin)).unwrap();
    assert_eq!(config.model, "mine");
    assert_eq!(config.service_tier, "priority");
    let mut reload = SettingsReload::new(&layout, &overlay, &config);
    assert_eq!(reload.reload(&layout, &overlay, Some(&pin)).0.model, "mine");
    // This session's own choice applies here and becomes the saved default.
    save_model_config(&layout, Scope::Global, &[set("model", "next")], "main").unwrap();
    pin.adopt_role(&load_merged(&layout, &overlay).unwrap(), "main");
    assert_eq!(
        load_session_config(&layout, &overlay, Some(&pin))
            .unwrap()
            .model,
        "next"
    );
    assert_eq!(load_config(&layout, &overlay).unwrap().model, "next");
}

#[test]
fn model_favorites_persist_and_project_empty_list_overrides_global() {
    use crate::llm::model_config::ModelSelection;
    let (_home, _project, layout) = fixture();
    let favorite = ModelSelection {
        provider: "openai".into(),
        model: "favorite-model".into(),
        base_url: Some("https://api.openai.com/v1".into()),
        ..ModelSelection::default()
    };
    let changes = [Change::Set {
        path: "model_favorites".into(),
        value: toml::Value::try_from(vec![favorite.clone()]).unwrap(),
    }];
    let saved = save_config(&layout, Scope::Global, &changes).unwrap();
    assert_eq!(saved.model_favorites, vec![favorite.clone()]);
    let reloaded = crate::config::types::Config::from_table(
        &layout,
        load_layer(&layout, Scope::Global).unwrap(),
    )
    .unwrap();
    assert_eq!(reloaded.model_favorites, vec![favorite.clone()]);
    let saved = save_config(
        &layout,
        Scope::Project,
        &[Change::Set {
            path: "model_favorites".into(),
            value: toml::Value::Array(vec![]),
        }],
    )
    .unwrap();
    assert!(saved.model_favorites.is_empty());
    let global: Vec<ModelSelection> =
        load_layer(&layout, Scope::Global).unwrap()["model_favorites"]
            .clone()
            .try_into()
            .unwrap();
    assert_eq!(global, vec![favorite]);
}

#[test]
fn patches_preserve_disjoint_keys_and_fail_closed() {
    let (_h, _p, l) = fixture();
    fs::write(
        l.config_files()[0].clone(),
        "unrelated = 42\n[model]\nbase_url = 'http://localhost:1234'\nname = 'old'\n",
    )
    .unwrap();
    save(
        &l,
        Scope::Global,
        &[
            Change::Set {
                path: "ui.theme".into(),
                value: toml::Value::String("light".into()),
            },
            Change::Remove {
                path: "model.base_url".into(),
            },
        ],
    )
    .unwrap();
    let text = fs::read_to_string(&l.config_files()[0]).unwrap();
    let parsed: toml::Table = text.parse().unwrap();
    assert_eq!(parsed["unrelated"].as_integer(), Some(42));
    assert!(parsed["model"].get("base_url").is_none());
    assert_eq!(parsed["model"]["name"].as_str(), Some("old"));
    fs::write(&l.config_files()[0], "[broken").unwrap();
    assert!(save(&l, Scope::Global, &[]).is_err());
    assert_eq!(fs::read_to_string(&l.config_files()[0]).unwrap(), "[broken");
}
#[test]
fn history_roundtrips_and_preserves_external_appends() {
    let (_h, _p, l) = fixture();
    load_history(&l, 5).unwrap();
    save_history(&l, &["a\n雪".into()], 5).unwrap();
    fs::write(
        history_path(&l).unwrap(),
        serde_json::to_vec(&vec!["a\n雪", "external"]).unwrap(),
    )
    .unwrap();
    save_history(&l, &["a\n雪".into(), "new".into(), "new".into()], 5).unwrap();
    assert_eq!(load_history(&l, 5).unwrap(), ["a\n雪", "external", "new"]);
    #[cfg(unix)]
    assert_eq!(
        platform_fs::mode(&fs::metadata(history_path(&l).unwrap()).unwrap()) & 0o777,
        0o600
    );
    #[cfg(windows)]
    assert!(platform_fs::is_owner_only(&File::open(history_path(&l).unwrap()).unwrap()).unwrap());
    save_history(&l, &["external".into(), "new".into(), "last".into()], 2).unwrap();
    assert_eq!(load_history(&l, 2).unwrap(), ["new", "last"]);
}
#[test]
fn refuses_symlink_and_hardlink_targets() {
    let (_h, _p, l) = fixture();
    let outside = tempfile::NamedTempFile::new().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), &l.config_files()[0]).unwrap();
        assert!(save(&l, Scope::Global, &[]).is_err());
        fs::remove_file(&l.config_files()[0]).unwrap();
    }
    fs::hard_link(outside.path(), &l.config_files()[0]).unwrap();
    assert!(save(&l, Scope::Global, &[]).is_err());
}
