use super::*;

fn migrated(text: &str) -> toml::Table {
    let mut table: toml::Table = toml::from_str(text).unwrap();
    migrate_model_role_table(&mut table);
    table
}

fn role_table(table: &toml::Table) -> toml::Table {
    table
        .get("model_roles")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default()
}

fn resolve(text: &str) -> Result<Config> {
    let project = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned())?;
    Config::from_table(&layout, toml::from_str(text)?)
}

#[test]
fn legacy_role_keys_map_to_current_roles() {
    let table = migrated(
        "[model_roles.smol]\nprovider='mlx'\nmodel='small-a'\n\
             [model_roles.stronger]\nprovider='x'\nmodel='coach-c'\n\
             [model_roles.feedback]\nprovider='x'\nmodel='coach-d'\n\
             [model_roles.plan]\nprovider='x'\nmodel='plan-e'\n\
             [model_roles.memory]\nprovider='x'\nmodel='mem-f'\n\
             [escalation]\nenabled=true\nrole='stronger'\n",
    );
    let roles = role_table(&table);
    assert_eq!(roles.len(), 4);
    assert_eq!(roles["small"]["model"].as_str(), Some("small-a"));
    assert_eq!(roles["coach"]["model"].as_str(), Some("coach-c"));
    assert_eq!(roles["planner"]["model"].as_str(), Some("plan-e"));
    assert_eq!(roles["memory"]["model"].as_str(), Some("mem-f"));
    assert!(!table["escalation"].as_table().unwrap().contains_key("role"));
}

#[test]
fn explicit_new_key_wins_and_legacy_priority_orders_the_rest() {
    let table = migrated(
        "[model_roles.smol]\nprovider='mlx'\nmodel='smol'\n\
             [model_roles.compaction]\nprovider='mlx'\nmodel='compaction'\n\
             [model_roles.small]\nprovider='mlx'\nmodel='explicit'\n\
             [model_roles.stronger]\nprovider='x'\nmodel='stronger'\n\
             [model_roles.feedback]\nprovider='x'\nmodel='feedback'\n",
    );
    let roles = role_table(&table);
    assert_eq!(roles["small"]["model"].as_str(), Some("explicit"));
    assert_eq!(roles["coach"]["model"].as_str(), Some("stronger"));
}

#[test]
fn first_legacy_key_in_priority_order_wins_for_a_target() {
    let table = migrated(
        "[model_roles.compaction]\nprovider='mlx'\nmodel='compaction'\n\
             [model_roles.smol]\nprovider='mlx'\nmodel='smol'\n",
    );
    assert_eq!(role_table(&table)["small"]["model"].as_str(), Some("smol"));
}

#[test]
fn default_fills_unassigned_roles_and_vision_is_dropped() {
    let table = migrated(
        "[model_roles.default]\nprovider='mlx'\nmodel='base'\n\
             [model_roles.vision]\nprovider='mlx'\nmodel='vision'\n\
             [model_roles.plan]\nprovider='mlx'\nmodel='plan'\n",
    );
    let roles = role_table(&table);
    assert!(!roles.contains_key("default"));
    assert!(!roles.contains_key("vision"));
    assert_eq!(roles["planner"]["model"].as_str(), Some("plan"));
    for role in ["task", "small", "coach", "tester", "memory"] {
        assert_eq!(roles[role]["model"].as_str(), Some("base"), "{role}");
    }
    assert!(!roles.contains_key("decision"));
}

#[test]
fn legacy_at_references_and_memory_role_fields_are_renamed() {
    let table = migrated(
        "[profile]\nmodels=['@smol', '@stronger', 'anthropic/x', '@subagent', '@task']\n\
             [memory.local]\nextraction_role='memory'\nconsolidation_role='stronger'\n",
    );
    let models = table["profile"]["models"].as_array().unwrap();
    assert_eq!(models[0].as_str(), Some("@small"));
    assert_eq!(models[1].as_str(), Some("@coach"));
    assert_eq!(models[2].as_str(), Some("anthropic/x"));
    assert_eq!(models[3].as_str(), Some("@task"));
    assert_eq!(models[4].as_str(), Some("@task"));
    assert_eq!(
        table["memory"]["local"]["extraction_role"].as_str(),
        Some("memory")
    );
    assert_eq!(
        table["memory"]["local"]["consolidation_role"].as_str(),
        Some("coach")
    );
    assert!(!migrate_model_role_table(&mut table.clone()));
}

#[test]
fn missing_memory_role_is_recoverable_setup_not_invalid_config() {
    let with_backend = |backend: &str| {
        format!("model='local'\nprovider='mlx'\n[memory]\nenabled=true\nbackend='{backend}'\n")
    };
    for backend in ["local", "sharpshooter"] {
        let config = resolve(&with_backend(backend)).unwrap();
        config.validate().unwrap();
        assert!(
            config
                .memory_setup_error()
                .unwrap()
                .contains("assign the memory role")
        );
        let configured = resolve(&format!(
            "{}[model_roles.memory]\nprovider='mlx'\nmodel='m'\n",
            with_backend(backend)
        ))
        .unwrap();
        configured.validate().unwrap();
        assert!(configured.memory_setup_error().is_none());
    }
    for backend in ["off", "provenance"] {
        let config = resolve(&with_backend(backend)).unwrap();
        config.validate().unwrap();
        assert!(config.memory_setup_error().is_none());
    }
    let config = resolve("model='local'\nprovider='mlx'\n[memory]\nbackend='local'\n[memory.local]\nconsolidation_role='coach'\n[model_roles.memory]\nprovider='mlx'\nmodel='m'\n").unwrap();
    assert!(
        config
            .memory_setup_error()
            .unwrap()
            .contains("assign the coach role")
    );
}

#[test]
fn validation_accepts_only_assignable_roles() {
    for role in [
        "task",
        "small",
        "coach",
        "tester",
        "planner",
        "memory",
        "embedding",
    ] {
        resolve(&format!(
            "model='local'\nprovider='mlx'\n[model_roles.{role}]\nprovider='mlx'\nmodel='m'\n"
        ))
        .unwrap()
        .validate()
        .unwrap();
    }
    for role in [
        "main", "default", "stronger", "feedback", "vision", "subagent",
    ] {
        let result = resolve(&format!(
            "model='local'\nprovider='mlx'\n[model_roles.{role}]\nprovider='mlx'\nmodel='m'\n"
        ))
        .and_then(|config| config.validate());
        assert!(result.is_err(), "accepted unknown model role {role}");
    }
}

#[test]
fn removed_settings_migrate_and_the_rest_still_loads() {
    let mut table: toml::Table = toml::from_str(
        "[memory]\nenabled=true\nbackend='mnemopi'\n[memory.mnemopi]\nendpoint='x'\n\
             [memory.local]\nextraction_role='embedding'\nconsolidation_role='coach'\n\
             [memory.sharpshooter]\nmodel_role='embedding'\n\
             [harness]\nmax_steps=3\nallow_skill_install_scripts=true\ntools=['read','memory_edit']\n\
             [browser]\nrelay=true\nrelay_url='http://localhost:1'\ntern=false\n\
             [reliability]\nauto_check='local'\nenabled=true\n\
             [model_roles.embedding]\nprovider='mlx'\nmodel='e'\n\
             [model_roles.memory]\nprovider='mlx'\nmodel='m'\n\
             [model_presets.work.roles.embedding]\nprovider='mlx'\nmodel='e'\n\
             [retry.fallback_chains]\nmemory=['@embedding','@coach']\n[ui]\nworker_shards=false\nsession_rail=true\n\
             [extensions]\nenabled=true\n[extensions.plugins.demo]\nprogram='/bin/demo'\n",
    )
    .unwrap();
    assert!(migrate_removed_settings(&mut table));
    assert!(!migrate_removed_settings(&mut table));
    assert_eq!(table["ui"]["pinned_workers"].as_str(), Some("off"));
    assert!(
        !table["ui"]
            .as_table()
            .unwrap()
            .contains_key("worker_shards")
    );
    assert_eq!(table["ui"]["session_rail"].as_bool(), Some(true));
    assert_eq!(table["browser"]["relay"].as_bool(), Some(true));
    assert_eq!(
        table["browser"]["relay_url"].as_str(),
        Some("http://localhost:1")
    );
    assert!(!table["browser"].as_table().unwrap().contains_key("tern"));
    assert!(!table["memory"].as_table().unwrap().contains_key("mnemopi"));
    assert!(!table.contains_key("extensions"));
    assert_eq!(table["memory"]["backend"].as_str(), Some("semantic"));
    assert!(
        !table["reliability"]
            .as_table()
            .unwrap()
            .contains_key("auto_check")
    );
    assert_eq!(table["reliability"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        table["memory"]["local"]["consolidation_role"].as_str(),
        Some("coach")
    );
    assert!(
        table["memory"]["local"]
            .as_table()
            .unwrap()
            .get("extraction_role")
            .is_none()
    );
    assert!(
        table["memory"]["sharpshooter"]
            .as_table()
            .unwrap()
            .get("model_role")
            .is_none()
    );
    assert_eq!(table["model_roles"]["memory"]["model"].as_str(), Some("m"));
    assert_eq!(
        table["model_roles"]["embedding"]["model"].as_str(),
        Some("e")
    );
    assert_eq!(
        table["model_presets"]["work"]["roles"]["embedding"]["model"].as_str(),
        Some("e")
    );
    assert_eq!(table["harness"]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        table["retry"]["fallback_chains"]["memory"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let config = Config::from_table(&layout, table).unwrap();
    config.validate().unwrap();
    assert_eq!(config.harness.max_steps, 3);
    assert_eq!(config.role_selection("embedding").unwrap().model, "e");
}

#[test]
fn task_and_small_coexist_and_current_roles_are_migration_safe() {
    let mut table: toml::Table = toml::from_str(
        "[model_roles.task]\nprovider='mlx'\nmodel='implementation'\n\
             [model_roles.small]\nprovider='mlx'\nmodel='maintenance'\n\
             [model_roles.memory]\nprovider='mlx'\nmodel='memory'\n",
    )
    .unwrap();
    let original = table.clone();
    assert!(!migrate_model_role_table(&mut table));
    assert_eq!(table, original);
    let roles = role_table(&table);
    assert_eq!(roles["task"]["model"].as_str(), Some("implementation"));
    assert_eq!(roles["small"]["model"].as_str(), Some("maintenance"));
    table
        .get_mut("model_roles")
        .unwrap()
        .as_table_mut()
        .unwrap()
        .insert(
            "subagent".into(),
            toml::Value::try_from(crate::llm::model_config::ModelSelection::new(
                "mlx", "legacy",
            ))
            .unwrap(),
        );
    assert!(migrate_model_role_table(&mut table));
    assert_eq!(
        table, original,
        "explicit task must win over legacy subagent"
    );
    assert!(!migrate_model_role_table(&mut table));
}

#[test]
fn task_survives_persistent_legacy_migration_and_serialization() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    fs::write(
        &path,
        "model='main-model'\nprovider='mlx'\n\
             [model_roles.subagent]\nprovider='mlx'\nmodel='implementation'\n\
             [model_roles.small]\nprovider='mlx'\nmodel='maintenance'\n",
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    config.validate().unwrap();
    assert_eq!(config.for_role("task").model, "implementation");
    assert_eq!(config.for_role("small").model, "maintenance");
    let persisted = fs::read_to_string(&path).unwrap();
    let mut table: toml::Table = toml::from_str(&persisted).unwrap();
    assert!(role_table(&table).contains_key("task"));
    assert!(!role_table(&table).contains_key("subagent"));
    assert!(!migrate_model_role_table(&mut table));
    let reloaded = Config::load(&layout, None).unwrap();
    assert_eq!(reloaded.model_roles, config.model_roles);
    assert_eq!(fs::read_to_string(&path).unwrap(), persisted);
    let serialized = toml::Value::try_from(&config).unwrap();
    let round_trip = Config::from_table(&layout, serialized.as_table().unwrap().clone()).unwrap();
    assert_eq!(round_trip.model_roles, config.model_roles);
}

#[test]
fn task_status_shows_assigned_model_or_main_without_changing_small_fallback() {
    use crate::llm::model_config::{ModelSelection, RoleSource};
    let mut config = Config {
        model: "main-model".into(),
        ..Default::default()
    };
    let task = config
        .role_statuses()
        .into_iter()
        .find(|row| row.role == "task")
        .unwrap();
    assert_eq!(task.source, RoleSource::FollowsMain);
    assert_eq!(task.effective, Some(config.selection().display_name()));
    assert!(task.assigned.is_none());
    assert_eq!(config.for_role("task").selection(), config.selection());
    let selection = ModelSelection::new("mlx", "implementation");
    config.model_roles.insert("task".into(), selection.clone());
    let task = config
        .role_statuses()
        .into_iter()
        .find(|row| row.role == "task")
        .unwrap();
    assert_eq!(task.source, RoleSource::Assigned);
    assert_eq!(task.effective, Some(selection.display_name()));
    assert_eq!(task.assigned, Some(selection));
    assert_eq!(config.for_role("small").selection(), config.selection());
}

#[test]
fn image_role_follows_main_until_assigned_and_can_be_cleared() {
    use crate::llm::model_config::{ModelSelection, RoleSource};
    let mut config = Config {
        model: "main-model".into(),
        ..Default::default()
    };
    let status = config
        .role_statuses()
        .into_iter()
        .find(|row| row.role == "image")
        .unwrap();
    assert_eq!(status.source, RoleSource::FollowsMain);
    assert_eq!(config.for_role("image").selection(), config.selection());
    let mut image = ModelSelection::new("mlx-serve", "downloaded/image-model");
    image.base_url = Some("http://127.0.0.1:1234/v1".into());
    image.context_window = Some(4096);
    image.protocol = Some(crate::llm::model_config::ApiProtocol::ChatCompletions);
    config.model_roles.insert("image".into(), image.clone());
    config.validate().unwrap();
    assert_eq!(config.for_role("image").selection(), image);
    assert_eq!(config.model, "main-model");
    let table = toml::Value::try_from(&config).unwrap();
    assert_eq!(
        table["model_roles"]["image"]["model"].as_str(),
        Some("downloaded/image-model")
    );
    config.model_roles.remove("image");
    assert_eq!(config.for_role("image").selection(), config.selection());
}

#[test]
fn turn_drafter_row_is_listed_only_while_the_turn_optimizer_is_on() {
    use crate::llm::model_config::{ROLES, RoleSource, TURN_DRAFTER};
    assert_eq!(ROLES.last().map(|(role, _)| *role), Some(TURN_DRAFTER));
    let mut config = Config {
        model: "main-model".into(),
        ..Default::default()
    };
    assert!(
        !config
            .role_statuses()
            .iter()
            .any(|row| row.role == TURN_DRAFTER)
    );
    config.experimental.turn_optimizer = true;
    let drafter = config
        .role_statuses()
        .into_iter()
        .find(|row| row.role == TURN_DRAFTER)
        .unwrap();
    assert_eq!(drafter.source, RoleSource::FollowsMain);
    assert_eq!(drafter.effective, Some(config.selection().display_name()));
    assert_eq!(
        config.for_role(TURN_DRAFTER).selection(),
        config.selection()
    );
}

#[test]
fn unassigned_roles_resolve_to_the_main_selection() {
    let config = resolve(
        "provider='chatgpt'\nmodel='account-model'\n[model_roles.small]\nprovider='chatgpt'\nmodel='account-small'\n",
    )
    .unwrap();
    assert_eq!(config.for_role("coach").selection(), config.selection());
    assert_eq!(config.for_role("planner").selection(), config.selection());
    assert_eq!(config.for_role("small").model, "account-small");
}

#[test]
fn unassigned_decision_is_not_filled() {
    let config = resolve(
        "provider='chatgpt'\nmodel='account-model'\n[model_roles.memory]\nprovider='chatgpt'\nmodel='account-memory'\n",
    )
    .unwrap();
    assert_eq!(config.for_role("decision").model, "account-model");
    assert!(config.role_selection("decision").is_none());
    assert_eq!(config.for_role("memory").model, "account-memory");
    assert_eq!(config.for_role("coach").model, "account-model");
}

#[test]
fn decision_role_must_be_a_laya_checkpoint() {
    let laya = resolve(
        "model='local'\nprovider='mlx'\n[model_roles.decision]\nprovider='laya'\nmodel='/models/laya'\n",
    )
    .unwrap();
    laya.validate().unwrap();
    assert_eq!(
        laya.laya_service().checkpoint.as_deref(),
        Some("/models/laya")
    );
    let chat = resolve(
        "model='local'\nprovider='mlx'\n[model_roles.decision]\nprovider='mlx'\nmodel='m'\n",
    );
    assert!(
        chat.is_err(),
        "invalid decision roles are rejected during resolution"
    );
}

#[test]
fn migration_moves_a_chat_decision_role_to_memory() {
    let table = migrated("[model_roles.decision]\nprovider='mlx'\nmodel='chat'\n");
    let roles = role_table(&table);
    assert!(!roles.contains_key("decision"));
    assert_eq!(roles["memory"]["model"].as_str(), Some("chat"));
}
