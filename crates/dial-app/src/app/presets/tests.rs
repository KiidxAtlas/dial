use super::*;
use crate::app::selection::assignment_thinking_changes;
use crate::llm::model_config::ModelSelection;

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    (project, home, layout)
}

fn save(layout: &Layout, changes: &[Change]) -> Config {
    settings_store::save_config(layout, Scope::Global, changes).unwrap()
}

fn with_level(provider: &str, model: &str, level: Option<&str>) -> ModelSelection {
    let mut selection = ModelSelection::new(provider, model);
    selection.thinking = level.map(str::to_owned);
    selection
}

#[test]
fn a_preset_saves_and_restores_every_role_with_its_thinking_level() {
    let (_project, _home, layout) = setup();
    let set = |path: &str, value: &str| Change::Set {
        path: path.into(),
        value: value.into(),
    };
    let config = save(
        &layout,
        &[
            set("provider", "anthropic"),
            set("model", "claude-opus-5-5"),
            set("model_roles.task.provider", "anthropic"),
            set("model_roles.task.model", "claude-haiku-5-5"),
            set("model_roles.task.thinking", "medium"),
        ],
    );
    let config = save(&layout, &main_thinking_changes(&config, "high").unwrap());
    assert_eq!(
        crate::config::thinking::main_level(&config).as_deref(),
        Some("high")
    );

    assert!(save_changes(&config, "1bad", Save::Create).is_err());
    assert!(save_changes(&config, "work", Save::Update).is_err());
    let config = save(
        &layout,
        &save_changes(&config, "work", Save::Create).unwrap(),
    );
    assert!(save_changes(&config, "work", Save::Create).is_err());
    let saved = &config.model_presets["work"];
    assert_eq!(saved.main.as_ref().unwrap().model, "claude-opus-5-5");
    assert_eq!(
        saved.main.as_ref().unwrap().thinking.as_deref(),
        Some("high")
    );
    assert_eq!(saved.roles["task"].thinking.as_deref(), Some("medium"));

    // Move away: another main model at its default level, and no task role.
    let other = with_level("anthropic", "claude-haiku-5-5", None);
    let mut changes = crate::config::selection::changes(&config, &other, "main").unwrap();
    changes.extend(main_thinking_changes(&config.with_selection(&other), "default").unwrap());
    changes.push(Change::Remove {
        path: "model_roles.task".into(),
    });
    let moved = save(&layout, &changes);
    assert_eq!(moved.model, "claude-haiku-5-5");
    assert_eq!(crate::config::thinking::main_level(&moved), None);
    assert!(moved.role_selection("task").is_none());

    let restored = save(&layout, &apply_named(&moved, "work").unwrap());
    assert_eq!(restored.model, "claude-opus-5-5");
    assert_eq!(
        crate::config::thinking::main_level(&restored).as_deref(),
        Some("high")
    );
    let task = restored.role_selection("task").unwrap();
    assert_eq!(task.model, "claude-haiku-5-5");
    assert_eq!(task.thinking.as_deref(), Some("medium"));

    let renamed = save(
        &layout,
        &rename_changes(&layout, &restored, "work", "fast").unwrap(),
    );
    assert!(renamed.model_presets.contains_key("fast"));
    assert!(!renamed.model_presets.contains_key("work"));
    assert!(rename_changes(&layout, &renamed, "work", "x").is_err());
    let deleted = save(&layout, &delete_changes(&layout, &renamed, "fast").unwrap());
    assert!(deleted.model_presets.is_empty());
}

#[test]
fn assigning_a_model_sets_or_clears_the_roles_thinking_level() {
    let (_project, _home, layout) = setup();
    let config = save(
        &layout,
        &[
            Change::Set {
                path: "provider".into(),
                value: "anthropic".into(),
            },
            Change::Set {
                path: "model".into(),
                value: "claude-opus-5-5".into(),
            },
        ],
    );
    let picked = with_level("anthropic", "claude-haiku-5-5", Some("low"));
    let mut changes = crate::config::selection::changes(&config, &picked, "task").unwrap();
    changes.extend(assignment_thinking_changes(&config, "task", &picked).unwrap());
    let config = save(&layout, &changes);
    assert_eq!(
        config.role_selection("task").unwrap().thinking.as_deref(),
        Some("low")
    );
    // A later pick at the model default does not inherit the old level.
    let plain = with_level("anthropic", "claude-haiku-5-5", None);
    let config = save(
        &layout,
        &assignment_thinking_changes(&config, "task", &plain).unwrap(),
    );
    assert_eq!(config.role_selection("task").unwrap().thinking, None);
    assert!(
        assignment_thinking_changes(&config, "task", &with_level("anthropic", "x", Some("auto")))
            .is_err(),
        "auto is main-only"
    );
    assert!(
        crate::llm::catalog::documented_thinking_levels("anthropic", "claude-haiku-5-5", true)
            .contains(&"low")
    );
}

#[test]
fn describe_lists_each_role_with_its_level() {
    let preset = ModelPreset {
        main: Some(with_level("anthropic", "claude-opus-5-5", Some("high"))),
        roles: [("small".to_owned(), with_level("openai", "gpt-5", None))].into(),
        ..Default::default()
    };
    let rows = describe(&preset);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].ends_with("· thinking high"), "{rows:?}");
    assert!(!rows[1].contains("thinking"), "{rows:?}");
}
