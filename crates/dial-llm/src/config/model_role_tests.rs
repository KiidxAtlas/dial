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
    assert_eq!(roles.len(), 3);
    assert_eq!(roles["small"]["model"].as_str(), Some("small-a"));
    assert_eq!(roles["coach"]["model"].as_str(), Some("coach-c"));
    assert!(!roles.contains_key("planner"));
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
    assert_eq!(roles["coach"]["model"].as_str(), Some("plan"));
    for role in ["task", "small", "tester", "memory"] {
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
        "decision",
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
        "main",
        "default",
        "stronger",
        "feedback",
        "vision",
        "subagent",
        "planner",
        "judge",
        "grounding",
        "image_embedding",
    ] {
        let result = resolve(&format!(
            "model='local'\nprovider='mlx'\n[model_roles.{role}]\nprovider='mlx'\nmodel='m'\n"
        ))
        .and_then(|config| config.validate());
        assert!(result.is_err(), "accepted unknown model role {role}");
    }
}

#[test]
fn role_thinking_level_applies_to_the_role_and_is_validated() {
    let config = resolve(
        "model='claude-opus-5-5'\nprovider='anthropic'\n\
         [model_roles.task]\nprovider='anthropic'\nmodel='claude-haiku-5-5'\nthinking='medium'\n",
    )
    .unwrap();
    config.validate().unwrap();
    let task = config.for_role("task");
    let profile = task.active_model_profile().expect("role thinking profile");
    assert_eq!(profile.model, "claude-haiku-5-5");
    assert_eq!(
        profile.reasoning_effort,
        Some(crate::llm::profiles::ReasoningEffort::Medium)
    );
    assert!(
        config.active_model_profile().is_none(),
        "the main model keeps its own thinking"
    );
    for level in ["auto", "loud"] {
        let result = resolve(&format!(
            "model='local'\nprovider='mlx'\n[model_roles.task]\nprovider='mlx'\nmodel='m'\nthinking='{level}'\n"
        ))
        .and_then(|config| config.validate());
        assert!(result.is_err(), "accepted role thinking {level}");
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
    assert_eq!(config.for_role("tester").selection(), config.selection());
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
fn decision_role_accepts_chat_or_laya_with_separate_checkpoint() {
    let laya = resolve(
        "model='local'\nprovider='mlx'\n[model_roles.decision]\nprovider='laya'\nmodel='decision'\n[laya]\ncheckpoint='/models/laya'\n",
    ).unwrap();
    laya.validate().unwrap();
    assert_eq!(
        laya.laya_service().checkpoint.as_deref(),
        Some("/models/laya")
    );
    let chat = resolve(
        "model='local'\nprovider='mlx'\n[model_roles.decision]\nprovider='mlx'\nmodel='m'\n",
    )
    .unwrap();
    assert_eq!(chat.for_role("decision").model, "m");
    assert!(chat.laya_service().checkpoint.is_none());
    let row = chat
        .role_statuses()
        .into_iter()
        .find(|row| row.role == "decision")
        .unwrap();
    assert_eq!(row.source, crate::llm::model_config::RoleSource::Assigned);
}

#[test]
fn current_chat_decision_role_is_migration_safe() {
    let mut table = migrated("[model_roles.decision]\nprovider='mlx'\nmodel='chat'\n");
    assert_eq!(
        role_table(&table)["decision"]["model"].as_str(),
        Some("chat")
    );
    assert!(!migrate_model_role_table(&mut table));
}

#[test]
fn consolidated_roles_migrate_without_losing_feature_routes() {
    let mut table = migrated(
        "[model_roles.planner]\nprovider='mlx'\nmodel='planning'\n\
         [model_roles.judge]\nprovider='openai'\nmodel='typed-choice'\n\
         [model_roles.decision]\nprovider='laya'\nmodel='/models/checkpoint'\n\
         [model_roles.grounding]\nprovider='endpoint'\nmodel='ui-model'\nbase_url='http://127.0.0.1:9000/v1'\n\
         [model_roles.image_embedding]\nprovider='mlx'\nmodel='vectors'\n\
         [agents.custom]\nmodel=['@planner:high','@judge']\n\
         [retry.fallback_chains]\njudge=['@judge']\nplanner=['@planner']\n",
    );
    let roles = role_table(&table);
    assert_eq!(roles["coach"]["model"].as_str(), Some("planning"));
    assert_eq!(roles["decision"]["model"].as_str(), Some("typed-choice"));
    for removed in ["planner", "judge", "grounding", "image_embedding"] {
        assert!(!roles.contains_key(removed), "{removed}");
    }
    assert_eq!(
        table["laya"]["checkpoint"].as_str(),
        Some("/models/checkpoint")
    );
    assert_eq!(
        table["vision"]["grounding"]["model"].as_str(),
        Some("ui-model")
    );
    assert_eq!(
        table["vision"]["grounding"]["base_url"].as_str(),
        Some("http://127.0.0.1:9000/v1")
    );
    assert_eq!(
        table["vision"]["image_embedding"]["model"].as_str(),
        Some("vectors")
    );
    assert_eq!(
        table["agents"]["custom"]["model"][0].as_str(),
        Some("@coach:high")
    );
    assert_eq!(
        table["agents"]["custom"]["model"][1].as_str(),
        Some("@decision")
    );
    assert_eq!(
        table["retry"]["fallback_chains"]["decision"][0].as_str(),
        Some("@decision")
    );
    assert_eq!(
        table["retry"]["fallback_chains"]["coach"][0].as_str(),
        Some("@coach")
    );
    assert!(
        !migrate_model_role_table(&mut table),
        "migration must be idempotent"
    );
}

#[test]
fn consolidated_role_migration_preserves_explicit_new_settings() {
    let mut table = migrated(
        "[model_roles.coach]\nprovider='mlx'\nmodel='coach'\n\
         [model_roles.planner]\nprovider='mlx'\nmodel='old-plan'\n\
         [model_roles.decision]\nprovider='openai'\nmodel='new-decision'\n\
         [model_roles.judge]\nprovider='openai'\nmodel='old-judge'\n\
         [model_roles.grounding]\nprovider='mlx'\nmodel='old-grounding'\n\
         [vision.grounding]\nprovider='mlx'\nmodel='new-grounding'\n",
    );
    assert_eq!(
        table["model_roles"]["coach"]["model"].as_str(),
        Some("coach")
    );
    assert_eq!(
        table["model_roles"]["decision"]["model"].as_str(),
        Some("new-decision")
    );
    assert_eq!(
        table["vision"]["grounding"]["model"].as_str(),
        Some("new-grounding")
    );
    assert!(!migrate_model_role_table(&mut table));
}

#[test]
fn saved_presets_migrate_removed_roles_and_keep_feature_selections() {
    let mut table = migrated(
        "[model_presets.work.roles.planner]\nprovider='mlx'\nmodel='plan'\n\
         [model_presets.work.roles.judge]\nprovider='openai'\nmodel='judge-chat'\n\
         [model_presets.work.roles.decision]\nprovider='laya'\nmodel='/checkpoint'\n\
         [model_presets.work.roles.grounding]\nprovider='mlx'\nmodel='locate'\n\
         [model_presets.work.roles.image_embedding]\nprovider='mlx'\nmodel='vectors'\n",
    );
    let config: Config = table.clone().try_into().unwrap();
    let preset = &config.model_presets["work"];
    assert_eq!(preset.roles["coach"].model, "plan");
    assert_eq!(preset.roles["decision"].model, "judge-chat");
    assert_eq!(preset.roles.len(), 2);
    assert_eq!(preset.laya_checkpoint.as_deref(), Some("/checkpoint"));
    let vision = preset.vision.as_ref().unwrap();
    assert_eq!(vision.grounding.as_ref().unwrap().model, "locate");
    assert_eq!(vision.image_embedding.as_ref().unwrap().model, "vectors");
    assert!(!migrate_model_role_table(&mut table));
}

#[test]
fn migrated_local_vision_override_does_not_inherit_the_global_endpoint() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    for feature in ["grounding", "image_embedding"] {
        let global = migrated(&format!(
            "provider='mlx'\nmodel='main'\n[model_roles.{feature}]\n\
             provider='endpoint'\nmodel='remote'\nbase_url='http://127.0.0.1:9000/v1'\napi_key_env='OLD_KEY'\n",
        ));
        let project = migrated(&format!(
            "[model_roles.{feature}]\nprovider='mlx'\nmodel='local'\n",
        ));
        let config = Config::from_layers(&layout, &global, &project, &toml::Table::new()).unwrap();
        let resolved = config.for_vision(feature);
        assert_eq!(resolved.model, "local");
        assert!(
            resolved.endpoint.is_none(),
            "{feature} retained the global endpoint"
        );
        assert!(
            config
                .vision_selection(feature)
                .unwrap()
                .api_key_env
                .is_none()
        );
    }
}
