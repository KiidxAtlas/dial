use super::*;
use crate::agent::Agent;
use crate::agent::window::context;
use crate::app::command_registry::{
    BUILTIN_COMMANDS, command_entries, palette_entries, turn_count,
};
use crate::app::completions::command_argument_completions;
use crate::app::core::App;
use crate::app::discovery::{Catalog, ProbeLease};
use crate::app::display::{config_key, local_time, terminal_dark};
use crate::app::drive::sign_in_still_pending;
use crate::app::git::{StatusCache, parse_file_diffs, parse_status};
use crate::app::improve_view::improve_summary;
use crate::app::learning::{Learning, lora_state};
use crate::app::notices::{memory_age, memory_status_text};
use crate::app::overlay::compute::{compute_starts, overlay_compute_release};
use crate::app::overlay::{Which, overlay_destination, work_panel};
use crate::app::panel::{
    PanelView, panel_body_limit, panel_inner, panel_rows_window, panel_scroll,
};
use crate::app::selection::{
    PendingFavorite, clear_role_changes, model_settings_profile, next_favorite, reasoning_changes,
    reasoning_profile, thinking_label,
};
use crate::app::shell::Shell;
use crate::config::paths::Layout;
use crate::config::selection::changes as selection_changes;
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::{Approval, Config};
use crate::learn::auto;
use crate::llm::catalog;
use crate::llm::model_config::{ModelSelection, UiConfig};
use crate::storage::session::{Session, SessionMeta};
use crate::storage::skills;
use crate::storage::store::Store;
use crate::ui::composer::CommandKind;
use crate::ui::keys::{Action, Keymap};
use crate::ui::overlay::palette::PaletteOverlay;
use crate::ui::overlay::{DrawCtx, OverlayEvent};
use crate::ui::status::{Lora, StatusData};
use crate::ui::text;
use crate::ui::theme::Sym;
use crossterm::event::{Event, KeyCode};
use fs2::FileExt;
use ratatui::text::Line;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[test]
fn provider_updates_persist_explicit_auth_method() {
    use crate::llm::model_config::{AuthMethod, ProviderSpec};
    for (auth, expected) in [
        (AuthMethod::Auto, "auto"),
        (AuthMethod::ApiKey, "api-key"),
        (AuthMethod::OAuth, "oauth"),
    ] {
        let changes = crate::app::selection::provider_changes(
            "anthropic",
            &ProviderSpec {
                auth,
                ..Default::default()
            },
        );
        assert!(changes.iter().any(|change| matches!(change,
            Change::Set { path, value } if path == "providers.anthropic.auth" && value.as_str() == Some(expected)
        )));
    }
}

#[test]
fn overlay_navigation_opens_roles_directly_in_every_lifecycle() {
    assert!(matches!(
        overlay_destination("open:roles"),
        Some(Which::ModelRoles)
    ));
    assert!(matches!(
        overlay_destination("open:models"),
        Some(Which::Models)
    ));
    assert!(matches!(
        overlay_destination("open:context-defaults"),
        Some(Which::ContextDefaults)
    ));
    assert!(overlay_destination("open:unknown").is_none());
}
#[test]
fn coherent_runtime_coding_does_not_open_or_schedule_learning() {
    let (_home, _project, layout, mut saved) = config();
    saved.model = "scripted".into();
    saved.improve.enabled = true;
    saved.improve.background = true;
    saved.training.enabled = true;
    saved.assistance.teacher_enabled = true;
    saved.codebase.synthesize_architecture = true;
    saved.laya.enabled = true;
    let path = saved.state_dir.join("state.sqlite3");
    let app = App::new(
        layout,
        saved.clone(),
        toml::Table::new(),
        Default::default(),
        Vec::new(),
        Default::default(),
    )
    .unwrap();
    assert!(app.store.is_none());
    assert!(
        !path.exists(),
        "Coding must not create the learning database"
    );
    assert!(!app.config.improve.enabled && !app.config.improve.background);
    assert!(!app.config.training.enabled && !app.config.laya.enabled);
    assert!(!app.config.assistance.teacher_enabled && !app.config.codebase.synthesize_architecture);
    assert!(
        saved.improve.enabled && saved.training.enabled,
        "Saved learning preferences stay intact"
    );
}

#[test]
fn teaching_a_local_model_opens_its_learning_store_for_coding() {
    let (_home, _project, layout, mut saved) = config();
    saved.model = "scripted".into();
    saved.endpoint = None;
    saved.improve.teach = true;
    let path = saved.state_dir.join("state.sqlite3");
    let mut app = App::new(
        layout,
        saved.clone(),
        toml::Table::new(),
        Default::default(),
        Vec::new(),
        Default::default(),
    )
    .unwrap();
    assert!(app.store.is_some() && app.store_error.is_none());
    assert!(path.exists());
    assert!(app.config.improve.enabled && !app.config.improve.background);
    // A hosted model is never recorded, even with the switch on.
    saved.endpoint = Some(crate::config::types::EndpointConfig {
        base_url: "https://api.example.com/v1".into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        headers: None,
        account_id: None,
    });
    app.adopt(saved);
    assert!(app.store.is_none() && !app.config.improve.enabled);
}

#[test]
fn consistent_ux_work_aliases_have_one_navigation_contract() {
    for alias in ["workers", "agents", "jobs", "hub"] {
        assert!(matches!(work_panel(alias, ""), Some(Which::Subagents)));
    }
    for alias in ["processes", "bg"] {
        assert!(matches!(work_panel(alias, ""), Some(Which::Jobs)));
    }
    assert!(
        work_panel("workers", "task-1").is_none(),
        "job IDs still use their status handler"
    );
    assert!(
        work_panel("agents", "definitions").is_none(),
        "agent definitions remain accessible"
    );
}

#[test]
fn first_esc_during_sign_in_warns_and_the_next_gives_up() {
    let mut warned = false;
    assert!(
        sign_in_still_pending(&mut warned),
        "first Esc keeps setup open"
    );
    assert!(
        !sign_in_still_pending(&mut warned),
        "second Esc gives up on the sign-in"
    );
    assert!(
        !sign_in_still_pending(&mut warned),
        "later Escs stay on the quit path"
    );
}

#[test]
fn onboarding_navigation_and_connection_tests_are_not_cancellations() {
    use crate::app::drive::{OnboardingFlow, onboarding_flow};
    use crate::app::overlay::OverlayOutcome;

    for (action, expected) in [
        ("open:models", "models"),
        ("open:providers", "providers"),
        ("open:model-settings", "settings"),
    ] {
        let outcome = OverlayOutcome {
            next: Some(action.into()),
            ..Default::default()
        };
        let OnboardingFlow::Navigate(which) = onboarding_flow(&outcome) else {
            panic!("{action} must navigate, not exit setup");
        };
        assert!(matches!(
            (expected, which),
            ("models", Which::Models)
                | ("providers", Which::Providers)
                | ("settings", Which::ModelSettings)
        ));
    }
    let test = OverlayOutcome {
        next: Some("test:lm-studio".into()),
        ..Default::default()
    };
    assert!(matches!(
        onboarding_flow(&test),
        OnboardingFlow::TestConnection(id) if id == "lm-studio"
    ));
    assert!(matches!(
        onboarding_flow(&OverlayOutcome::default()),
        OnboardingFlow::Cancel
    ));
    let unsupported = OverlayOutcome {
        next: Some("command:help".into()),
        ..Default::default()
    };
    assert!(matches!(
        onboarding_flow(&unsupported),
        OnboardingFlow::Unsupported(_)
    ));
}

#[test]
fn onboarding_applies_navigation_even_after_settings_were_saved() {
    use crate::app::drive::{OnboardingFlow, onboarding_flow};
    use crate::app::overlay::OverlayOutcome;

    let (_home, _project, _layout, config) = config();
    let mut outcome = OverlayOutcome {
        latest: Some(config),
        ..Default::default()
    };
    assert!(matches!(onboarding_flow(&outcome), OnboardingFlow::Saved));
    outcome.next = Some("open:models".into());
    assert!(matches!(
        onboarding_flow(&outcome),
        OnboardingFlow::Navigate(Which::Models)
    ));
}

#[test]
fn memory_status_names_the_backend_and_the_next_step_in_plain_words() {
    let value = json!({
        "backend": "hindsight",
        "active": false,
        "error": "the environment variable named in API key variable is not set",
    });
    let text = memory_status_text(&value);
    assert!(text.contains("Backend: Hindsight service"), "{text}");
    assert!(text.contains("Active: no"), "{text}");
    assert!(text.contains("Why: the environment variable"), "{text}");
    assert!(text.contains("Next: /settings → Memory"), "{text}");
}

#[test]
fn memory_status_lists_remembered_facts_before_backend_details() {
    let value = json!({
        "backend": "provenance",
        "active": true,
        "count": 2,
        "truncated": false,
        "recent": [
            {"content": "Tests run with cargo nextest", "scope": "project", "updated_at": 0},
            {"content": "Prefer rg over grep", "scope": "global", "updated_at": 0}
        ],
    });
    let text = memory_status_text(&value);
    assert!(text.contains("2 remembered facts, newest first:"), "{text}");
    assert!(text.contains("Tests run with cargo nextest"), "{text}");
    assert!(text.contains("Backend: Remembered facts"), "{text}");
    assert!(text.contains("Active: yes"), "{text}");
    assert!(!text.contains("pipeline"), "{text}");
}

#[test]
fn memory_status_with_no_items_says_so_and_how_items_arrive() {
    let value = json!({"backend": "provenance", "active": true, "count": 0, "truncated": false, "recent": []});
    let text = memory_status_text(&value);
    assert!(text.contains("No remembered facts yet."), "{text}");
    assert!(!text.contains("newest first"), "{text}");
}

#[test]
fn memory_age_uses_short_relative_units() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(memory_age(now), "just now");
    assert_eq!(memory_age(now - 300), "5m ago");
    assert_eq!(memory_age(now - 7200), "2h ago");
    assert_eq!(memory_age(now - 3 * 86400), "3d ago");
}

#[test]
fn action_palette_uses_registered_commands_and_real_turn_policies() {
    use crate::ui::theme::Theme;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    let custom = context::CommandDef {
        name: "review-project".into(),
        description: "Review project conventions".into(),
        template: "Review $ARGUMENTS".into(),
        source: "/fixture/review-project.md".into(),
    };
    let skill = skills::Skill {
        name: "web-review".into(),
        description: "Inspect a rendered page".into(),
        path: "/fixture/web-review/SKILL.md".into(),
        source: skills::Scope::Project,
        origin: "Dial",
    };
    let entries = palette_entries(&[custom], &[skill], &[]);
    let find = |name: &str| entries.iter().find(|e| e.command.name == name).unwrap();
    assert!(find("undo").during_turn.starts_with("Stops"));
    assert!(find("model").during_turn.starts_with("Opens now"));
    assert!(find("workers").during_turn.starts_with("Opens now"));
    for name in ["message", "cancel", "search", "help", "context"] {
        assert!(find(name).during_turn.starts_with("Runs now"), "{name}");
    }
    for name in ["rename", "review-project", "skill:web-review"] {
        assert!(find(name).during_turn.starts_with("Runs after"), "{name}");
    }
    assert_eq!(find("rename").command.args_hint.as_deref(), Some("<title>"));
    assert_eq!(
        find("model").command.args_hint.as_deref(),
        Some("[settings|defaults|local]")
    );
    // Hidden aliases resolve at dispatch but never appear as palette rows.
    assert!(entries.iter().all(|e| e.command.name != "jobs"));
    let theme = Theme::dark();
    let keys = Keymap::defaults();
    let status = StatusData::default();
    let preview = |_: &UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &preview,
    };
    let mut palette = PaletteOverlay::new(entries, true);
    palette.handle(&Event::Paste("model".into()), &keys);
    let mut terminal = Terminal::new(TestBackend::new(110, 26)).unwrap();
    terminal.draw(|frame| palette.draw(frame, &ctx)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(screen.contains("turn running"));
    assert!(screen.contains("Opens now"));
    assert!(screen.contains(&keys.label(Action::ModelSelect)));
    // Optional visual artifact from the real renderer and command registry, for offscreen QA.
    if let Ok(path) = std::env::var("DIAL_TEST_PALETTE_CAPTURE") {
        let cells: Vec<_> = buffer
            .content
            .iter()
            .map(|cell| {
                json!({
                    "symbol":cell.symbol(), "fg":format!("{:?}", cell.fg),
                    "bg":format!("{:?}", cell.bg), "modifiers":format!("{:?}",cell.modifier),
                })
            })
            .collect();
        std::fs::write(
            path,
            json!({"width":110,"height":26,"cells":cells}).to_string(),
        )
        .unwrap();
    }
    assert!(
        matches!(palette.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
        OverlayEvent::Action(action) if action == "command:/model")
    );
    palette.handle(
        &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &keys,
    );
    palette.handle(&Event::Paste("review-project".into()), &keys);
    assert!(
        matches!(palette.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
        OverlayEvent::Action(action) if action == "command:/review-project")
    );
}

#[test]
fn undo_and_rollback_take_an_optional_positive_turn_count() {
    assert_eq!(turn_count(""), Some(1), "bare /undo keeps undoing one turn");
    assert_eq!(turn_count(" 3 "), Some(3));
    for bad in ["0", "-1", "two", "1 2"] {
        assert_eq!(turn_count(bad), None, "{bad}");
    }
    let undo = BUILTIN_COMMANDS
        .iter()
        .find(|(name, _, _)| *name == "undo")
        .unwrap();
    assert_eq!(undo.1, "[n]");
    assert!(Shell::stops_turn("/undo 3"));
}

#[test]
fn only_commands_meant_to_end_the_turn_stop_it() {
    for line in [
        "/exit",
        "/new",
        "/clear",
        "/resume abc",
        "/fork",
        "/undo",
        "/rollback 2",
        "/compact",
        "/plan on",
    ] {
        assert!(Shell::stops_turn(line), "{line}");
    }
    for line in [
        "/model",
        "/settings",
        "/approval auto",
        "/theme nord",
        "/cost",
        "/jobs",
        "/my-custom",
        "/skill:review x",
        "/export",
    ] {
        assert!(
            !Shell::stops_turn(line),
            "{line} must not interrupt a running turn"
        );
    }
}

#[test]
fn context_model_picker_auto_masks_inherited_caps_and_preserves_family_defaults() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let set = |path: &str, value: toml::Value| Change::Set {
        path: path.into(),
        value,
    };
    let base = settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set("context_window", 272000.into()),
            set("cloud_context_window", 64000.into()),
            set("local_context_window", 8192.into()),
            set("model_roles.small.provider", "anthropic".into()),
            set("model_roles.small.model", "claude-opus-5-5".into()),
            set("model_roles.small.context_window", 200000.into()),
        ],
    )
    .unwrap();
    let global = std::fs::read(&layout.config_files()[0]).unwrap();
    let model = ModelSelection {
        provider: "anthropic".into(),
        model: "claude-opus-5-5".into(),
        base_url: Some(crate::llm::anthropic::BASE_URL.into()),
        context_window: Some(1000000),
        ..Default::default()
    };
    let selected = settings_store::save_config(
        &layout,
        Scope::Project,
        &selection_changes(&base, &model, "main").unwrap(),
    )
    .unwrap();
    assert_eq!(selected.context_window, 64000);
    assert_eq!(
        selected.context_limit,
        crate::config::types::ContextWindow::Auto
    );
    let selected = settings_store::save_config(
        &layout,
        Scope::Project,
        &selection_changes(&selected, &model, "small").unwrap(),
    )
    .unwrap();
    assert_eq!(selected.for_role("small").context_window, 64000);
    assert_eq!(std::fs::read(&layout.config_files()[0]).unwrap(), global);
    let reloaded = Config::load(&layout, None).unwrap();
    assert_eq!(reloaded.context_window, 64000);
    assert_eq!(reloaded.for_role("small").context_window, 64000);
}

#[test]
fn switching_models_clears_an_inherited_profile_without_deleting_it() {
    use crate::llm::{chatgpt, profiles::ModelProfile};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let profile = ModelProfile {
        provider: "chatgpt".into(),
        model: "old-model".into(),
        ..Default::default()
    };
    let config = settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            Change::Set {
                path: "provider".into(),
                value: "chatgpt".into(),
            },
            Change::Set {
                path: "model".into(),
                value: "old-model".into(),
            },
            Change::Set {
                path: "model_profiles.old".into(),
                value: toml::Value::try_from(profile).unwrap(),
            },
            Change::Set {
                path: "model_profile".into(),
                value: "old".into(),
            },
        ],
    )
    .unwrap();
    let before = std::fs::read(layout.config_files()[0].clone()).unwrap();
    let selection = ModelSelection {
        provider: "chatgpt".into(),
        model: "new-model".into(),
        base_url: Some(chatgpt::BASE_URL.into()),
        ..Default::default()
    };
    let saved = settings_store::save_config(
        &layout,
        Scope::Project,
        &selection_changes(&config, &selection, "main").unwrap(),
    )
    .unwrap();
    assert_eq!(saved.model, "new-model");
    assert!(saved.model_profile.is_none());
    assert!(saved.model_profiles.contains_key("old"));
    assert_eq!(
        std::fs::read(layout.config_files()[0].clone()).unwrap(),
        before
    );
}
#[test]
fn model_settings_profiles_save_locally_and_match_exact_models() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let config = settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            Change::Set {
                path: "model".into(),
                value: "test/local-model".into(),
            },
            Change::Set {
                path: "provider".into(),
                value: "mlx".into(),
            },
        ],
    )
    .unwrap();
    let (name, mut profile) = model_settings_profile(&config);
    assert_ne!(profile.capabilities.tools, Some(false));
    assert_eq!(profile.model, config.model);
    profile.local_sampling = Some(crate::llm::local_sampling::LocalSampling {
        top_k: Some(40),
        min_p: Some(0.05),
        ..Default::default()
    });
    let saved = settings_store::save_config(
        &layout,
        Scope::Project,
        &[
            Change::Set {
                path: format!("model_profiles.{name}"),
                value: toml::Value::try_from(&profile).unwrap(),
            },
            Change::Set {
                path: "model_profile".into(),
                value: name.clone().into(),
            },
        ],
    )
    .unwrap();
    assert_eq!(
        saved
            .active_model_profile()
            .unwrap()
            .local_sampling
            .as_ref()
            .unwrap()
            .top_k,
        Some(40)
    );
    let reloaded = Config::load(&layout, None).unwrap();
    assert_eq!(model_settings_profile(&reloaded).0, name);
    assert_eq!(
        reloaded
            .active_model_profile()
            .unwrap()
            .local_sampling
            .as_ref()
            .unwrap()
            .min_p,
        Some(0.05)
    );
    let mut other = reloaded;
    other.model = "different-model".into();
    assert!(other.active_model_profile().is_none());
    assert_ne!(model_settings_profile(&other).0, name);
}
#[test]
fn lmstudio_settings_default_to_sampling_and_seed_capabilities() {
    let config = Config {
        provider: Some("lm-studio".into()),
        model: "qwen".into(),
        ..Config::default()
    };
    let (_, profile) = model_settings_profile(&config);
    assert_eq!(profile.provider, "lm-studio");
    assert!(profile.capabilities.temperature);
    assert!(profile.capabilities.top_p);
    assert!(profile.capabilities.seed);
}

#[test]
fn chatgpt_reasoning_controls_create_and_persist_an_exact_model_profile() {
    use crate::llm::{chatgpt, profiles::ReasoningEffort};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    chatgpt::test_account(&layout.user_dir, false);
    chatgpt::test_catalog(
        &layout.user_dir,
        &serde_json::json!({"models":[{"slug":"account-model","visibility":"list","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"max"},{"effort":"ultra"}]}]}),
    );
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            Change::Set {
                path: "provider".into(),
                value: "chatgpt".into(),
            },
            Change::Set {
                path: "model".into(),
                value: "account-model".into(),
            },
        ],
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert!(config.model_profile.is_none());
    let (changes, label) = reasoning_changes(&config, "ultra").unwrap();
    assert_eq!(label, "ultra");
    let saved = settings_store::save_config(&layout, Scope::Project, &changes).unwrap();
    assert_eq!(
        saved.active_model_profile().unwrap().reasoning_effort,
        Some(ReasoningEffort::Ultra)
    );
    assert_eq!(
        Config::load(&layout, None)
            .unwrap()
            .active_model_profile()
            .unwrap()
            .reasoning_effort,
        Some(ReasoningEffort::Ultra)
    );
    assert_eq!(
        thinking_label(&reasoning_profile(&saved).unwrap().unwrap()),
        "ultra"
    );
    assert!(
        reasoning_changes(&saved, "off")
            .err()
            .unwrap()
            .to_string()
            .contains("supported reasoning levels")
    );
    assert!(reasoning_changes(&saved, "minimal").is_err());
    let (changes, _) = reasoning_changes(&saved, "high").unwrap();
    let saved = settings_store::save_config(&layout, Scope::Project, &changes).unwrap();
    assert_eq!(
        saved.active_model_profile().unwrap().reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        thinking_label(&reasoning_profile(&saved).unwrap().unwrap()),
        "high"
    );
    let switched = saved.with_selection(&ModelSelection {
        provider: "chatgpt".into(),
        model: "other-model".into(),
        base_url: Some(chatgpt::BASE_URL.into()),
        ..Default::default()
    });
    assert!(reasoning_profile(&switched).unwrap().is_none());
}

#[test]
fn thinking_status_distinguishes_toggle_default_and_unsupported() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    let mut profile = ModelProfile {
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AnthropicAdaptive,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(thinking_label(&profile), "default");
    profile.thinking = Some(true);
    assert_eq!(thinking_label(&profile), "on");
    profile.reasoning_effort = Some(ReasoningEffort::High);
    assert_eq!(thinking_label(&profile), "high");
    profile.thinking = Some(false);
    assert_eq!(thinking_label(&profile), "off");
    profile.capabilities.reasoning = ReasoningApi::None;
    assert_eq!(thinking_label(&profile), "unsupported");
}

#[test]
fn existing_budgeted_thinking_toggle_keeps_its_budget_without_effort_levels() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi};
    let mut config = Config {
        provider: Some("anthropic".into()),
        model: "exact-model".into(),
        model_profile: Some("custom".into()),
        ..Default::default()
    };
    config.model_profiles.insert(
        "custom".into(),
        ModelProfile {
            provider: "anthropic".into(),
            model: "exact-model".into(),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::AnthropicBudget,
                ..Default::default()
            },
            thinking: Some(false),
            thinking_budget: Some(1024),
            ..Default::default()
        },
    );
    for arg in ["on", "off"] {
        let (changes, _) = reasoning_changes(&config, arg).unwrap();
        let Change::Set { value, .. } = &changes[0] else {
            panic!("profile must be saved")
        };
        let updated: ModelProfile = value.clone().try_into().unwrap();
        assert_eq!(updated.thinking, Some(arg == "on"));
        assert_eq!(updated.thinking_budget, Some(1024));
        updated.validate().unwrap();
    }
}

#[test]
fn pending_status_queries_do_not_block_or_spawn_more_workers() {
    let (release, wait) = std::sync::mpsc::channel();
    let mut cache = StatusCache::<String>::default();
    let start = Instant::now();
    cache.refresh(Duration::from_secs(5), move || {
        wait.recv().unwrap();
        "ready".into()
    });
    for _ in 0..100 {
        cache.refresh(Duration::ZERO, || {
            panic!("a second query must not start while one is pending")
        });
    }
    assert!(start.elapsed() < Duration::from_millis(100));
    assert!(cache.value.is_empty());
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cache.worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    cache.refresh(Duration::from_secs(5), || {
        panic!("fresh values must stay cached")
    });
    assert_eq!(cache.value, "ready");
    assert!(cache.worker.is_none());
}
use crate::ui::theme::Theme;

pub(super) fn config() -> (tempfile::TempDir, tempfile::TempDir, Layout, Config) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let config = Config::load(&layout, None).unwrap();
    (home, project, layout, config)
}

#[test]
fn disabling_compute_locks_releases_an_existing_session_lease() {
    let (_home, _project, layout, mut config) = config();
    config.model = "local-model".into();
    config.endpoint = None;
    config.provider = Some("mlx".into());
    let mut app = App::new(
        layout,
        config.clone(),
        toml::Table::new(),
        Default::default(),
        Vec::new(),
        Default::default(),
    )
    .unwrap();
    let path = app.layout.user_dir.join("compute.lock");
    let lock = auto::lock_file(&path).unwrap();
    lock.try_lock_exclusive().unwrap();
    let inherited = lock.try_clone().unwrap();
    *app.compute.borrow_mut() = Some(lock);
    crate::learn::compute::register(
        &app.layout.user_dir,
        "session",
        &app.layout.project_root,
        &app.config.state_dir,
        &app.config.model,
    )
    .unwrap();
    config.compute_locks = false;
    app.adopt(config);
    assert!(app.compute.borrow().is_none());
    assert!(!app.layout.user_dir.join("compute-owner.json").exists());
    let contender = auto::lock_file(&path).unwrap();
    contender.try_lock_exclusive().unwrap();
    drop(inherited);
}

#[cfg(unix)]
#[test]
fn coding_model_catalog_ignores_learning_records_and_never_starts_python() {
    use std::os::unix::fs::PermissionsExt;
    let (_home, project, layout, mut config) = config();
    config.endpoint = None;
    config.provider = Some("mlx".into());
    let python = project.path().join("unexpected-python");
    std::fs::write(&python, "#!/bin/sh\nprintf ran > \"$0.called\"\nexit 1\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    config.python = python;
    let mut paths = Vec::new();
    for (name, supported) in [("main-model", true), ("role-model", false)] {
        let path = project.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        let selection = ModelSelection {
            model: path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let selected = config.with_selection(&selection);
        let store = Store::open(
            &selected.learning_dir(&layout),
            &selected.model,
            &selected.use_case,
            &selected.harness,
        )
        .unwrap();
        store
            .set_meta(
                "serving_capabilities",
                &json!({"inference":true, "lora":supported, "lora_probe_version":2}).to_string(),
            )
            .unwrap();
        paths.push(selection);
    }
    config.model = paths[0].model.clone();
    config.model_roles.insert("tester".into(), paths[1].clone());
    let unverified = project.path().join("unverified-role-model");
    std::fs::create_dir_all(&unverified).unwrap();
    config.model_roles.insert(
        "planner".into(),
        ModelSelection {
            model: unverified.to_string_lossy().into_owned(),
            ..Default::default()
        },
    );
    let app = App {
        launch: Default::default(),
        add_dirs: Vec::new(),
        mode_flags: Default::default(),
        models: RefCell::new(None),
        layout,
        config,
        overlay: toml::Table::new(),
        store: None,
        store_error: None,
        interrupt: Arc::new(AtomicBool::new(false)),
        compute: RefCell::new(None),
    };
    let mut catalog = Catalog::new();
    catalog.refresh(&app);
    assert_eq!(
        catalog.request, 0,
        "config refresh must preserve lazy discovery"
    );
    assert!(catalog.worker.is_none());
    assert!(catalog.get().is_none());
    catalog.start(&app);
    let deadline = Instant::now() + Duration::from_secs(5);
    while catalog.worker.is_some() {
        catalog.collect();
        assert!(Instant::now() < deadline, "catalog scan did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    for selection in &paths {
        let discovery = catalog.get().unwrap();
        let entry = discovery
            .models
            .iter()
            .find(|entry| entry.selection.model == selection.model)
            .unwrap();
        assert!(
            !entry.metadata.lora_verified,
            "Coding discovery must not open the learning store"
        );
    }
    let discovery = catalog.get().unwrap();
    let entry = discovery
        .models
        .iter()
        .find(|entry| entry.selection.model == unverified.to_string_lossy())
        .unwrap();
    assert!(!entry.metadata.lora_verified);
    // Repeated idle/catalog visits reuse the scan and never create model workers.
    for _ in 0..10 {
        catalog.start(&app);
        catalog.collect();
    }
    assert!(!project.path().join("unexpected-python.called").exists());
    assert!(!app.layout.user_dir.join("compute-processes").exists());
}

#[test]
fn lora_acceptance_is_not_runtime_use_and_disabling_training_does_not_hide_it() {
    let (_h, _p, _layout, mut config) = config();
    config.endpoint = None;
    config.model = "test-model".into();
    config.use_case = "test".into();
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    store
        .set_meta(
            "serving_capabilities",
            "{\"lora\":true,\"lora_probe_version\":2}",
        )
        .unwrap();
    let mut active = store.active().unwrap();
    assert_eq!(
        lora_state(&config, Some(&store), Some(&active)),
        Lora::Ready
    );
    active.adapter = Some("/adapter".into());
    config.training.enabled = false;
    assert_eq!(
        lora_state(&config, Some(&store), Some(&active)),
        Lora::Active(active.id)
    );
    use crate::llm::provider::{AdapterPhase, AdapterRuntime};
    let accepted = Lora::Active(active.id);
    assert_eq!(accepted.with_runtime(&AdapterRuntime::default()), accepted);
    assert_eq!(
        accepted.with_runtime(&AdapterRuntime {
            adapter: Some("/adapter".into()),
            phase: AdapterPhase::Used
        }),
        Lora::Used
    );
    assert_eq!(
        Lora::Training.with_runtime(&AdapterRuntime {
            adapter: Some("/adapter".into()),
            phase: AdapterPhase::Used
        }),
        Lora::Training
    );
    assert_eq!(
        Lora::Ready.with_runtime(&AdapterRuntime {
            adapter: None,
            phase: AdapterPhase::Used
        }),
        Lora::Ready
    );
}

#[test]
fn idle_handoff_releases_compute_without_closing_the_session() {
    let (_h, _p, layout, mut config) = config();
    let model = layout.user_dir.join("test-model");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("config.json"), "{}").unwrap();
    std::fs::write(model.join("model.safetensors"), "weights").unwrap();
    config.model = model.to_string_lossy().into_owned();
    config.use_case = "test".into();
    let lock_path = layout.user_dir.join("compute.lock");
    let lock = auto::lock_file(&lock_path).unwrap();
    lock.try_lock_exclusive().unwrap();
    let app = App {
        launch: Default::default(),
        add_dirs: Vec::new(),
        mode_flags: Default::default(),
        models: RefCell::new(None),
        layout,
        config,
        overlay: toml::Table::new(),
        store: None,
        store_error: None,
        interrupt: Arc::new(AtomicBool::new(false)),
        compute: RefCell::new(Some(lock)),
    };
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: "test-model".into(),
        },
    )
    .unwrap();
    let mut agent = app.make_agent(session).unwrap();
    let id = agent.session().id().to_owned();
    let worker = auto::lock_file(&lock_path).unwrap();
    assert_eq!(
        worker.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let inherited = app.compute.borrow().as_ref().unwrap().try_clone().unwrap();
    assert!(app.release_idle_compute(&mut agent));
    worker.try_lock_exclusive().unwrap();
    drop(inherited);
    assert!(app.compute.borrow().is_none());
    assert_eq!(agent.session().id(), id);
    assert!(
        agent.adapter_monitor().is_some(),
        "backend remains available for lazy reload"
    );
}

#[cfg(unix)]
#[test]
fn idle_unload_stops_a_resident_server_without_a_background_request() {
    if crate::learn::processes::live_inspection_unavailable() {
        return;
    }
    let (_h, _p, layout, mut config) = config();
    std::fs::create_dir_all(&layout.user_dir).unwrap();
    let python = crate::llm::server::fixtures::fake_python(&layout.user_dir);
    config.python = python.clone();
    config.endpoint = None;
    config.provider = Some("mlx".into());
    config.model = layout
        .user_dir
        .join("fake-model-pid")
        .to_string_lossy()
        .into_owned();
    config.improve.enabled = false;
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Local {
            model_dir: config.model.clone(),
        },
        python,
        5,
    )
    .with_steering(&config);
    let reply = backend
        .chat(
            None,
            &crate::llm::Chat {
                messages: vec![json!({"role":"user","content":"hello"})],
                tools: vec![],
                temperature: 0.0,
                max_tokens: 16,
                seed: None,
            },
            5,
            None,
        )
        .unwrap();
    assert_eq!(reply.text, "pong");
    let marker: Value = serde_json::from_slice(&std::fs::read(&config.model).unwrap()).unwrap();
    let pid = marker["pid"].as_i64().unwrap() as i32;
    let lock = auto::lock_file(&layout.user_dir.join("compute.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let app = App {
        launch: Default::default(),
        add_dirs: Vec::new(),
        mode_flags: Default::default(),
        models: RefCell::new(None),
        layout,
        config,
        overlay: toml::Table::new(),
        store: None,
        store_error: None,
        interrupt: Arc::new(AtomicBool::new(false)),
        compute: RefCell::new(Some(lock)),
    };
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    let id = session.id().to_owned();
    let mut agent = Agent::interactive(
        &app.config,
        backend,
        app.layout.clone(),
        session,
        None,
        None,
        app.interrupt.clone(),
    )
    .unwrap();
    assert!(!crate::learn::compute::background_pending(&app.layout.user_dir).unwrap());
    assert!(
        crate::learn::processes::snapshot(&app.layout.user_dir)
            .unwrap()
            .iter()
            .any(|row| row["pid"] == pid)
    );
    assert!(app.release_idle_compute(&mut agent));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert!(app.compute.borrow().is_none());
    assert_eq!(agent.session().id(), id);
    assert!(
        agent.adapter_monitor().is_some(),
        "backend remains available for the next turn"
    );
}
#[cfg(unix)]
#[test]
fn compute_action_release_unloads_a_resident_server_only_when_started() {
    if crate::learn::processes::live_inspection_unavailable() {
        return;
    }
    let (_h, _p, layout, mut config) = config();
    std::fs::create_dir_all(&layout.user_dir).unwrap();
    let python = crate::llm::server::fixtures::fake_python(&layout.user_dir);
    config.python = python.clone();
    config.endpoint = None;
    config.provider = Some("mlx".into());
    config.model = layout
        .user_dir
        .join("fake-model-residency")
        .to_string_lossy()
        .into_owned();
    config.improve.enabled = false;
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Local {
            model_dir: config.model.clone(),
        },
        python,
        5,
    )
    .with_steering(&config);
    backend
        .chat(
            None,
            &crate::llm::Chat {
                messages: vec![json!({"role":"user","content":"hello"})],
                tools: vec![],
                temperature: 0.0,
                max_tokens: 16,
                seed: None,
            },
            5,
            None,
        )
        .unwrap();
    let marker: Value = serde_json::from_slice(&std::fs::read(&config.model).unwrap()).unwrap();
    let pid = marker["pid"].as_i64().unwrap() as i32;
    let lock = auto::lock_file(&layout.user_dir.join("compute.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let app = App {
        launch: Default::default(),
        add_dirs: Vec::new(),
        mode_flags: Default::default(),
        models: RefCell::new(None),
        layout,
        config,
        overlay: toml::Table::new(),
        store: None,
        store_error: None,
        interrupt: Arc::new(AtomicBool::new(false)),
        compute: RefCell::new(Some(lock)),
    };
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    let id = session.id().to_owned();
    let mut agent = Agent::interactive(
        &app.config,
        backend,
        app.layout.clone(),
        session,
        None,
        None,
        app.interrupt.clone(),
    )
    .unwrap();
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    // The welcome flow has no agent; panels that only browse never invoke the release.
    assert!(overlay_compute_release(&app, None).is_none());
    assert!(alive(pid), "server stays resident while panels are browsed");
    assert!(
        app.compute.borrow().is_some(),
        "compute lease is kept while browsing"
    );
    // Non-starting actions (pause, stop, inspection) keep residency.
    assert!(!compute_starts(r#"{"action":"stop_process","pid":1}"#));
    assert!(!compute_starts(r#"{"action":"pause"}"#));
    assert!(!compute_starts("not json"));
    assert!(alive(pid));
    // A start action releases the server and the lease through the closure the panel gets.
    assert!(compute_starts(r#"{"action":"start_current"}"#));
    let mut release = overlay_compute_release(&app, Some(&mut agent)).unwrap();
    release();
    drop(release);
    for _ in 0..50 {
        if !alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "start action unloads the resident server");
    assert!(app.compute.borrow().is_none());
    assert_eq!(agent.session().id(), id);
}

#[test]
fn completed_probe_releases_compute_even_when_a_handle_was_inherited() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("compute.lock");
    let lock = auto::lock_file(&path).unwrap();
    lock.try_lock_exclusive().unwrap();
    let lease = ProbeLease(lock);
    let inherited = lease.0.try_clone().unwrap();
    drop(lease);
    let next = auto::lock_file(&path).unwrap();
    next.try_lock_exclusive().unwrap();
    drop(inherited);
}

#[test]
fn learning_status_reads_the_selected_model_store_not_the_project_store() {
    let (_h, _p, layout, mut config) = config();
    config.model = "test-model".into();
    config.use_case = "test".into();
    config.endpoint = None;
    let project_store = Store::open(
        &layout.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    project_store
        .set_meta(
            "serving_capabilities",
            "{\"lora\":true,\"lora_probe_version\":2}",
        )
        .unwrap();
    config.state_dir = layout.state_dir.join("models/selected");
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    store
        .set_meta(
            "serving_capabilities",
            "{\"lora\":false,\"lora_probe_version\":2,\"reason\":\"fused QKV unsupported\"}",
        )
        .unwrap();
    store
        .set_meta("auto_status", "{\"state\":\"running\"}")
        .unwrap();
    store
        .set_meta("cycle_progress", "{\"stage\":\"self-play 5/24\"}")
        .unwrap();
    let app = App {
        launch: Default::default(),
        add_dirs: Vec::new(),
        mode_flags: Default::default(),
        models: RefCell::new(None),
        layout,
        config,
        overlay: toml::Table::new(),
        store: Some(store),
        store_error: None,
        interrupt: Arc::new(AtomicBool::new(false)),
        compute: RefCell::new(None),
    };
    let mut learning = Learning::default();
    learning.read(&app);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !learning.cache.worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    learning.read(&app);
    assert_eq!(learning.cache.value.2, Lora::Unsupported);
    assert!(learning.cache.value.1.contains("self-play 5/24"));
    assert!(improve_summary(&app).contains("fused QKV unsupported"));
}

#[test]
fn favorite_quick_switch_wraps_deduplicates_and_preserves_endpoint_identity() {
    let (_h, _p, _l, mut config) = config();
    let current = config.selection();
    let next = ModelSelection {
        model: "second".into(),
        ..current.clone()
    };
    let unavailable = ModelSelection {
        model: "offline".into(),
        ..current.clone()
    };
    let other_endpoint = ModelSelection {
        base_url: Some("https://other.example/v1".into()),
        ..next.clone()
    };
    config.model_favorites = vec![
        current.clone(),
        unavailable,
        next.clone(),
        next.clone(),
        other_endpoint,
    ];
    let available = vec![current.clone(), next.clone()];
    let favorites = config.model_favorites.clone();
    assert_eq!(
        next_favorite(&config, &favorites, &available),
        Some(next.clone())
    );
    config = config.with_selection(&next);
    assert_eq!(
        next_favorite(&config, &favorites, &available),
        Some(current)
    );
    assert_eq!(next_favorite(&config, &favorites, &[]), None);
    assert_eq!(next_favorite(&config, &[], &available), None);
}

#[test]
fn effective_config_changes_require_authoritative_safe_boundary_adoption() {
    let (_h, _p, _l, base) = config();
    let mut ui = base.clone();
    ui.ui.theme = "light".into();
    ui.ui.status_separator = crate::llm::model_config::StatusSeparator::Slash;
    ui.approval = Approval::Auto;
    assert_ne!(
        config_key(&base),
        config_key(&ui),
        "UI and approval changes must update the authoritative immutable config"
    );
    for mutate in [
        (|c: &mut Config| c.model = "other".into()) as fn(&mut Config),
        |c| c.sandbox.allow_network = !c.sandbox.allow_network,
        |c| c.context_window += 1,
        |c| c.improve.enabled = !c.improve.enabled,
    ] {
        let mut changed = base.clone();
        mutate(&mut changed);
        assert_ne!(config_key(&base), config_key(&changed));
    }
}

#[test]
fn selecting_a_local_model_clears_the_remote_endpoint_and_a_new_remote_adds_its_provider() {
    let (_h, _p, _l, config) = config();
    let local = ModelSelection {
        provider: "mlx".into(),
        model: "mlx-community/Qwen".into(),
        ..ModelSelection::default()
    };
    let changes = selection_changes(&config, &local, "main").unwrap();
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, Change::Remove { path } if path == "endpoint")),
        "a local model must not keep an inherited endpoint"
    );
    assert!(changes.iter().any(|c| matches!(c, Change::Set { path, value } if path == "provider" && value.as_str() == Some("mlx"))));
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, Change::Set { path, value } if path == "context_window" && value.as_str() == Some("auto"))),
        "a model without a known window must not keep the previous model's"
    );
    let remote = ModelSelection {
        provider: "lm-studio".into(),
        model: "qwen".into(),
        base_url: Some("http://127.0.0.1:1234/v1".into()),
        context_window: Some(8192),
        ..ModelSelection::default()
    };
    let changes = selection_changes(&config, &remote, "main").unwrap();
    let sets: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Set { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    for expected in [
        "model",
        "endpoint.base_url",
        "providers.lm-studio.base_url",
        "context_window",
    ] {
        assert!(sets.contains(&expected), "{expected} missing from {sets:?}");
    }
    let helper = selection_changes(&config, &remote, "small").unwrap();
    assert!(
        helper.iter().all(|c| match c {
            Change::Set { path, .. } | Change::Insert { path, .. } | Change::Remove { path } =>
                path.starts_with("model_roles.small."),
        }),
        "a helper role must not touch the main model"
    );
    assert!(selection_changes(&config, &remote, "bogus").is_err());
    assert!(
        selection_changes(&config, &ModelSelection::default(), "main").is_err(),
        "an empty model id is refused"
    );
}

#[test]
fn model_assignment_takes_effect_in_both_scopes_without_touching_other_roles() {
    for scope in [Scope::Global, Scope::Project] {
        for role in [
            "main", "small", "coach", "tester", "planner", "memory", "decision",
        ] {
            let (_home, _project, layout, base) = config();
            let old =
                ModelSelection::new(if role == "decision" { "laya" } else { "mlx" }, "old-model");
            settings_store::save_config(
                &layout,
                Scope::Global,
                &selection_changes(&base, &old, role).unwrap(),
            )
            .unwrap();
            settings_store::save_config(
                &layout,
                Scope::Project,
                &selection_changes(&base, &old, role).unwrap(),
            )
            .unwrap();
            let other_role = if role == "tester" {
                "planner"
            } else {
                "tester"
            };
            let other = ModelSelection::new("mlx", "keep-other-model");
            settings_store::save_config(
                &layout,
                Scope::Project,
                &selection_changes(&base, &other, other_role).unwrap(),
            )
            .unwrap();
            settings_store::save_config(
                &layout,
                Scope::Project,
                &[Change::Set {
                    path: "codebase.architecture_model".into(),
                    value: "keep-architecture".into(),
                }],
            )
            .unwrap();
            let global_before = std::fs::read(&layout.config_files()[0]).unwrap();
            let fresh = Config::load(&layout, None).unwrap();
            let new = ModelSelection::new(
                if role == "decision" { "laya" } else { "openai" },
                "new-model",
            );
            settings_store::save_model_config(
                &layout,
                scope,
                &selection_changes(&fresh, &new, role).unwrap(),
                role,
            )
            .unwrap();
            let reloaded = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
            let status = reloaded
                .role_statuses()
                .into_iter()
                .find(|row| row.role == role)
                .unwrap();
            let assigned = status.assigned.unwrap();
            assert_eq!(assigned.model, new.model, "{scope:?}: {role}");
            assert_eq!(assigned.provider, new.provider, "{scope:?}: {role}");
            assert_eq!(
                reloaded.role_selection(other_role).unwrap().model,
                other.model
            );
            assert_eq!(reloaded.codebase.architecture_model, "keep-architecture");
            if scope == Scope::Project {
                assert_eq!(
                    std::fs::read(&layout.config_files()[0]).unwrap(),
                    global_before
                );
            } else {
                let project = settings_store::load_layer(&layout, Scope::Project).unwrap();
                if role != "main" {
                    assert!(project["model_roles"].get(role).is_none());
                }
            }
        }
    }
}

#[test]
fn model_assignment_globally_replaces_project_follow_main_marker() {
    let (_home, _project, layout, base) = config();
    settings_store::save_config(
        &layout,
        Scope::Project,
        &clear_role_changes("small", Scope::Project).unwrap(),
    )
    .unwrap();
    let new = ModelSelection::new("openai", "new-small-model");
    settings_store::save_model_config(
        &layout,
        Scope::Global,
        &selection_changes(&base, &new, "small").unwrap(),
        "small",
    )
    .unwrap();
    let fresh = Config::load(&layout, None).unwrap();
    assert_eq!(fresh.role_selection("small").unwrap().model, new.model);
    assert_eq!(fresh.for_role("small").selection().model, new.model);
}

#[test]
fn clearing_a_role_in_the_project_layer_overrides_a_global_assignment() {
    assert!(
        matches!(&clear_role_changes("coach", Scope::Global).unwrap()[..], [Change::Remove { path }] if path == "model_roles.coach")
    );
    let project = clear_role_changes("coach", Scope::Project).unwrap();
    assert!(project.iter().any(|c| matches!(c, Change::Set { path, value } if path == "model_roles.coach.provider" && value.as_str() == Some("inherit"))));
    assert!(clear_role_changes("main", Scope::Global).is_err());
}

#[test]
fn clearing_small_routes_to_main_without_changing_architecture_settings() {
    for scope in [Scope::Global, Scope::Project] {
        let (_home, _project, layout, _) = config();
        settings_store::save(
            &layout,
            Scope::Global,
            &[
                Change::Set {
                    path: "model".into(),
                    value: "main-model".into(),
                },
                Change::Set {
                    path: "provider".into(),
                    value: "mlx".into(),
                },
                Change::Set {
                    path: "model_roles.small.provider".into(),
                    value: "mlx".into(),
                },
                Change::Set {
                    path: "model_roles.small.model".into(),
                    value: "small-model".into(),
                },
                Change::Set {
                    path: "codebase.architecture_model".into(),
                    value: "architecture-only".into(),
                },
            ],
        )
        .unwrap();
        settings_store::save(&layout, scope, &clear_role_changes("small", scope).unwrap()).unwrap();
        let fresh = Config::load(&layout, None).unwrap();
        let status = fresh
            .role_statuses()
            .into_iter()
            .find(|row| row.role == "small")
            .unwrap();
        assert!(status.assigned.is_none());
        assert_eq!(
            status.source,
            crate::llm::model_config::RoleSource::FollowsMain
        );
        assert_eq!(
            status.effective.as_deref(),
            Some(fresh.for_role("small").selection().display_name().as_str())
        );
        assert_eq!(fresh.codebase.architecture_model, "architecture-only");
    }
}

#[test]
fn a_global_role_with_a_project_inherit_marker_resolves_to_the_main_model() {
    let (_h, _p, layout, _) = config();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            Change::Set {
                path: "model".into(),
                value: "main-model".into(),
            },
            Change::Set {
                path: "model_roles.coach.provider".into(),
                value: "mlx".into(),
            },
            Change::Set {
                path: "model_roles.coach.model".into(),
                value: "helper".into(),
            },
        ],
    )
    .unwrap();
    assert_eq!(
        Config::load(&layout, None).unwrap().model_roles["coach"].model,
        "helper"
    );
    settings_store::save_config(
        &layout,
        Scope::Project,
        &clear_role_changes("coach", Scope::Project).unwrap(),
    )
    .unwrap();
    let after = Config::load(&layout, None).unwrap();
    assert!(!after.model_roles.contains_key("coach"));
    assert_eq!(after.for_role("coach").model, "main-model");
}

#[test]
fn clearing_chatgpt_roles_in_project_discards_inherited_account_bindings() {
    for role in ["small", "coach", "tester", "planner", "memory"] {
        let (_home, _project, layout, config) = config();
        let selection = ModelSelection {
            account_id: Some("test-account".into()),
            ..ModelSelection::new("chatgpt", "gpt-5")
        };
        settings_store::save_config(
            &layout,
            Scope::Global,
            &selection_changes(&config, &selection, role).unwrap(),
        )
        .unwrap();
        let cleared = settings_store::save_config(
            &layout,
            Scope::Project,
            &clear_role_changes(role, Scope::Project).unwrap(),
        )
        .unwrap();
        assert!(cleared.role_selection(role).is_none(), "{role}");
        assert!(
            settings_store::load_config(&layout, &toml::Table::new())
                .unwrap()
                .role_selection(role)
                .is_none(),
            "{role}"
        );
    }
}

#[test]
fn clearing_decision_in_project_does_not_migrate_the_marker_to_memory() {
    let (_home, _project, layout, config) = config();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &selection_changes(
            &config,
            &ModelSelection::new("laya", "test/checkpoint"),
            "decision",
        )
        .unwrap(),
    )
    .unwrap();
    let cleared = settings_store::save_config(
        &layout,
        Scope::Project,
        &clear_role_changes("decision", Scope::Project).unwrap(),
    )
    .unwrap();
    assert!(cleared.role_selection("decision").is_none());
    assert!(cleared.role_selection("memory").is_none());
    assert!(
        settings_store::load_config(&layout, &toml::Table::new())
            .unwrap()
            .role_selection("decision")
            .is_none()
    );
}

#[test]
fn command_menu_lists_builtins_then_custom_then_skills_and_builtins_win() {
    let custom = vec![
        context::CommandDef {
            name: "audit-diff".into(),
            description: "review the diff".into(),
            template: String::new(),
            source: Default::default(),
        },
        context::CommandDef {
            name: "help".into(),
            description: "shadow".into(),
            template: String::new(),
            source: Default::default(),
        },
        context::CommandDef {
            name: "models".into(),
            description: "old alias".into(),
            template: String::new(),
            source: Default::default(),
        },
        context::CommandDef {
            name: "review".into(),
            description: "shadow review".into(),
            template: String::new(),
            source: Default::default(),
        },
    ];
    let skill = skills::Skill {
        name: "lint".into(),
        description: "run linters".into(),
        path: Default::default(),
        source: skills::Scope::Project,
        origin: "Dial",
    };
    let entries = command_entries(&custom, &[skill], &[]);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names.iter().filter(|n| **n == "help").count(),
        1,
        "a custom command cannot shadow a built-in"
    );
    assert!(
        !names.contains(&"models"),
        "retired aliases are not offered"
    );
    let at = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert!(at("exit") < at("audit-diff") && at("audit-diff") < at("skill:lint"));
    assert!(
        matches!(entries[at("audit-diff")].kind, CommandKind::Custom)
            && matches!(entries[at("skill:lint")].kind, CommandKind::Skill)
    );
    assert_eq!(names.iter().filter(|name| **name == "review").count(), 1);
    assert!(matches!(entries[at("review")].kind, CommandKind::Builtin));
    assert_eq!(entries[at("resume")].args_hint.as_deref(), Some("[id]"));
}

#[test]
fn command_argument_choices_include_subcommands_flags_and_installed_skill_names() {
    let skill = skills::Skill {
        name: "review".into(),
        description: "Review".into(),
        path: Default::default(),
        source: skills::Scope::User,
        origin: "Dial",
    };
    let choices = command_argument_completions(&Config::default(), &[skill]);
    for expected in [
        "/model settings",
        "/model defaults",
        "/understanding architecture",
        "/understanding build",
        "/understanding clear",
        "/memory clear",
        "/memory clear project",
        "/memory clear global",
        "/skills sources",
        "/skills list",
        "/skills import --dry-run",
        "/skills enable review",
        "/skills disable review --user",
        "/approval readonly",
        "/plan off",
        "/improve status",
        "/improve jobs",
        "/improve stop",
        "/improve cycle",
        "/improve train",
        "/improve session",
        "/improve enable",
        "/improve disable",
        "/improve versions",
        "/improve tasks",
        "/model local",
        "/model memory",
        "/experience ledger",
        "/theme auto",
    ] {
        assert!(
            choices.iter().any(|choice| choice == expected),
            "missing {expected}"
        );
    }
    assert!(
        choices.iter().all(|choice| !choice.contains('<')),
        "never insert usage placeholders"
    );
    let known_head = |choice: &str| {
        BUILTIN_COMMANDS.iter().any(|(name, _, _)| {
            choice == format!("/{name}") || choice.starts_with(&format!("/{name} "))
        }) || commands::specs().any(|spec| {
            choice == format!("/{}", spec.name) || choice.starts_with(&format!("/{} ", spec.name))
        })
    };
    let unknown: Vec<&String> = choices
        .iter()
        .filter(|choice| !known_head(choice))
        .collect();
    assert!(
        unknown.is_empty(),
        "completions without a known command: {unknown:?}"
    );
}

#[test]
fn numstat_parsing_keeps_binary_counts_and_renames_and_unusual_paths() {
    let files = parse_file_diffs(
        "12\t3\tsrc/with\ttab.rs\0-\t-\timage.png\x002\t1\t\0old name.rs\0new name.rs\0",
    );
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path, "image.png");
    assert_eq!(files[0].added, None);
    assert_eq!(files[1].path, "new name.rs");
    assert_eq!((files[1].added, files[1].removed), (Some(2), Some(1)));
    assert_eq!(files[2].path, "src/with\ttab.rs");
}

#[test]
fn git_status_counts_staged_modified_and_untracked() {
    assert_eq!(
        parse_status("M  a\n M b\nMM c\n?? d\n?? e\nA  f\n"),
        (3, 2, 2)
    );
    assert_eq!(parse_status(""), (0, 0, 0));
}

#[test]
fn terminal_background_is_read_from_colorfgbg() {
    assert_eq!(terminal_dark(Some("15;0")), Some(true));
    assert_eq!(terminal_dark(Some("0;15")), Some(false));
    assert_eq!(terminal_dark(Some("0;default;15")), Some(false));
    assert_eq!(terminal_dark(Some("garbage")), None);
    assert_eq!(terminal_dark(None), None);
}

#[test]
fn local_time_formats_like_strftime() {
    let clock = local_time("%H:%M");
    assert_eq!(clock.len(), 5);
    assert_eq!(clock.as_bytes()[2], b':');
    assert_eq!(local_time("%Y-%m-%d %H:%M:%S").len(), 19);
}

#[test]
fn policy_reason_renders_in_normal_and_narrow_approval_panels() {
    use crate::agent::safety::approval::{PolicyExplanation, PolicyReason};
    let explanation = PolicyExplanation {
        reason: PolicyReason::PromptRequired,
        rule_scope: None,
    };
    let summary = format!("cargo test\n{explanation}");
    let options = [
        ("Allow once", 'y'),
        ("Always allow this session", 'a'),
        ("Always allow this project", 'p'),
        ("Deny", 'n'),
    ];
    for width in [40, 100] {
        let rows = panel_rows_window(
            &Theme::dark(),
            width,
            64,
            PanelView {
                title: "Allow bash?",
                body: &summary,
                options: &options,
                selected: 0,
                offset: 0,
            },
        );
        assert!(
            rows.iter()
                .all(|row| text::line_width(row) <= width as usize)
        );
        let text = rows
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Policy:"));
        assert!(text.contains("operator approval required"));
        assert!(text.contains("Always allow this project"));
        assert!(text.contains("Deny"));
    }
}

/// Opt-in review artifact of the real panel rows; no provider, TTY or authority.
#[test]
#[ignore = "writes an HTML fixture for explicit browser screenshot review"]
fn policy_approval_visual_fixture() {
    use crate::agent::safety::approval::{PolicyExplanation, PolicyReason};
    use ratatui::style::{Color, Modifier};
    use std::fmt::Write as _;
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let color = |c: Option<Color>| match c {
        Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "inherit".into(),
    };
    let summary = format!(
        "cargo test\n{}",
        PolicyExplanation {
            reason: PolicyReason::PromptRequired,
            rule_scope: None,
        }
    );
    let options = [
        ("Allow once", 'y'),
        ("Always allow this session", 'a'),
        ("Always allow this project", 'p'),
        ("Deny", 'n'),
    ];
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>Dial policy approval review</title><style>body{background:#111318;color:#e0e3ea;padding:20px}pre{font:14px/1.4 monospace}</style><h1>Actual approval panel rows</h1>",
    );
    for width in [40, 100] {
        writeln!(html, "<h2>{width} columns</h2><pre>").unwrap();
        for line in panel_rows_window(
            &Theme::dark(),
            width,
            64,
            PanelView {
                title: "Allow bash?",
                body: &summary,
                options: &options,
                selected: 0,
                offset: 0,
            },
        ) {
            for span in line.spans {
                let style = line.style.patch(span.style);
                write!(
                    html,
                    "<span style=\"color:{};font-weight:{}\">{}</span>",
                    color(style.fg),
                    if style.add_modifier.contains(Modifier::BOLD) {
                        "bold"
                    } else {
                        "normal"
                    },
                    escape(&span.content)
                )
                .unwrap();
            }
            html.push('\n');
        }
        html.push_str("</pre>");
    }
    let directory = std::env::temp_dir().join("dial-policy-review");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("approval.html");
    std::fs::write(&path, html).unwrap();
    println!("Policy review fixture: {}", path.display());
}

#[test]
fn the_approval_panel_fits_any_width_and_shows_the_choice() {
    let theme = Theme::dark();
    let options = [
        ("Allow once", 'y'),
        ("Always allow this session", 'a'),
        ("Deny", 'n'),
    ];
    for width in [0u16, 1, 3, 4, 12, 40, 100] {
        let rows = panel_rows_window(
            &theme,
            width,
            64,
            PanelView {
                title: "Allow bash?",
                body: "rm -rf build && cargo build 日本語",
                options: &options,
                selected: 1,
                offset: 0,
            },
        );
        assert!(
            rows.iter().all(|r| text::line_width(r) <= width as usize),
            "width {width}"
        );
    }
    let rows = panel_rows_window(
        &theme,
        60,
        64,
        PanelView {
            title: "Allow bash?",
            body: "cargo test",
            options: &options,
            selected: 1,
            offset: 0,
        },
    );
    assert_eq!(
        rows.len(),
        1 + 1 + 3 + 1 + 1,
        "title, body, options, hint, bottom"
    );
    assert!(rows.iter().all(|r| text::line_width(r) == 60));
    let selected: Vec<String> = rows
        .iter()
        .map(|r| r.to_string())
        .filter(|s| s.contains(theme.sym(Sym::Pointer)))
        .collect();

    assert_eq!(selected.len(), 1);
    assert!(
        selected[0].contains("Always allow this session"),
        "{selected:?}"
    );
}
#[test]
fn modal_details_scroll_and_selection_stays_visible_at_small_sizes() {
    let theme = Theme::mono();
    let labels: Vec<_> = (0..32).map(|index| format!("choice-{index}")).collect();
    let options: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
    let body = (0..50)
        .map(|index| format!("detail-{index}\n"))
        .collect::<String>();
    for width in [6, 12, 40] {
        let rows = panel_rows_window(
            &theme,
            width,
            12,
            PanelView {
                title: "Details",
                body: &body,
                options: &options,
                selected: 31,
                offset: 20,
            },
        );
        assert!(
            rows.iter()
                .all(|row| text::line_width(row) <= width as usize)
        );
        assert!(rows.len() <= 12);
        if width == 40 {
            assert!(rows.iter().any(|row| row.to_string().contains("choice-31")));
        }
    }
    let first = panel_rows_window(
        &theme,
        40,
        20,
        PanelView {
            title: "Details",
            body: &body,
            options: &options,
            selected: 0,
            offset: 0,
        },
    );
    let later = panel_rows_window(
        &theme,
        40,
        20,
        PanelView {
            title: "Details",
            body: &body,
            options: &options,
            selected: 0,
            offset: 20,
        },
    );
    assert_ne!(
        first.iter().map(ToString::to_string).collect::<Vec<_>>(),
        later.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

#[test]
fn paging_long_approval_details_reaches_every_line_at_any_height() {
    let theme = Theme::mono();
    let options = [
        ("Allow once", 'y'),
        ("Always allow this session", 'a'),
        ("Always allow in this project", 'p'),
        ("Deny", 'n'),
    ];
    // Longer than the old 1,500-byte approval cap, with the consequential part at the end.
    let body = format!(
        "{}\nSENTINEL-END rm -rf /important",
        (0..120)
            .map(|index| format!("line-{index:03} echo safe"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(body.len() > 1500);
    for (cols, terminal_height) in [(40u16, 11usize), (40, 12), (60, 14), (100, 40)] {
        let height = terminal_height.saturating_sub(6);
        let lines = text::wrap_words(&text::sanitize(&body), panel_inner(cols)).len();
        let visible = panel_body_limit(options.len(), height);
        let mut seen = std::collections::BTreeSet::new();
        let mut offset = 0;
        loop {
            for row in panel_rows_window(
                &theme,
                cols,
                height,
                PanelView {
                    title: "Allow bash?",
                    body: &body,
                    options: &options,
                    selected: 0,
                    offset,
                },
            ) {
                let row = row.to_string();
                if let Some(at) = row.find("line-") {
                    seen.insert(row[at..at + 8].to_owned());
                }
                if row.contains("SENTINEL-END") {
                    seen.insert("end".to_owned());
                }
            }
            let next = panel_scroll(offset, lines, visible, true);
            if next == offset {
                break;
            }
            offset = next;
        }
        assert_eq!(seen.len(), 121, "cols {cols} height {terminal_height}");
        assert!(seen.contains("end"));
        assert_eq!(
            panel_scroll(offset, lines, visible, false),
            offset - (visible - 1).max(1)
        );
    }
}

#[test]
fn tiny_approval_panels_pause_confirmation_and_stay_within_the_viewport() {
    use crate::app::panel::panel_has_room;
    let options = [
        ("Allow once", 'y'),
        ("Allow for this project", 'p'),
        ("Deny", 'n'),
    ];
    for width in [0, 1, 12, 24, 35, 36, 80] {
        for height in 0..=8 {
            let rows = panel_rows_window(
                &Theme::dark(),
                width,
                height,
                PanelView {
                    title: "Allow bash?",
                    body: "cargo test",
                    options: &options,
                    selected: 1,
                    offset: 0,
                },
            );
            assert!(rows.len() <= height, "{width}x{height}");
            assert!(
                rows.iter()
                    .all(|row| text::line_width(row) <= width as usize)
            );
            if width < 36 || height < 5 {
                assert!(!panel_has_room(width, height));
                assert!(
                    !rows
                        .iter()
                        .any(|row| row.to_string().contains("Allow once"))
                );
            } else {
                assert!(panel_has_room(width, height));
                assert!(
                    rows.iter()
                        .any(|row| row.to_string().contains("Allow for this project"))
                );
            }
        }
    }
}

#[test]
fn effective_config_serializes_for_the_settings_overlay() {
    let (_h, _p, _l, config) = config();
    let table = toml::Table::try_from(&config).expect("Config must serialize to TOML");
    assert_eq!(
        crate::config::schema::get(&table, "ui.theme").and_then(toml::Value::as_str),
        Some("auto")
    );
    assert!(crate::config::schema::get(&table, "improve.interval_secs").is_some());
    assert!(crate::config::schema::get(&table, "sandbox.mode").is_some());
}

#[test]
fn initial_discovery_publishes_and_a_stale_refresh_cannot_replace_the_newest() {
    fn result(id: &str) -> catalog::Discovery {
        catalog::Discovery {
            models: vec![],
            providers: vec![catalog::ProviderStatus {
                id: id.into(),
                label: id.into(),
                base_url: None,
                state: "reachable".into(),
                detail: String::new(),
                models: 0,
            }],
        }
    }
    let mut catalog = Catalog::new();
    catalog.launch(|| result("first"));
    catalog.worker.take().unwrap().join().unwrap();
    assert_eq!(catalog.get().unwrap().providers[0].id, "first");
    let (release, wait) = std::sync::mpsc::channel();
    catalog.launch(move || {
        wait.recv().unwrap();
        result("stale")
    });
    let stale = catalog.worker.take().unwrap();
    catalog.launch(|| result("newest"));
    catalog.worker.take().unwrap().join().unwrap();
    release.send(()).unwrap();
    stale.join().unwrap();
    assert_eq!(catalog.get().unwrap().providers[0].id, "newest");
}
#[test]
fn pending_favorite_rejects_stale_scope_model_preferences_and_scan() {
    let (_home, _project, _layout, mut config) = config();
    let favorite = config.selection();
    config.model_favorites = vec![favorite.clone()];
    let pending = PendingFavorite {
        main: favorite,
        favorites: config.model_favorites.clone(),
        scope: Scope::Global,
        catalog_request: 7,
    };
    assert!(pending.current(&config, Scope::Global, 7));
    assert!(!pending.current(&config, Scope::Project, 7));
    assert!(!pending.current(&config, Scope::Global, 8));
    config.model = "other".into();
    assert!(!pending.current(&config, Scope::Global, 7));
    config.model = pending.main.model.clone();
    config.model_favorites.clear();
    assert!(!pending.current(&config, Scope::Global, 7));
}

#[test]
fn panicking_status_worker_keeps_the_stale_value_and_reports_once() {
    let mut cache = StatusCache::<String> {
        value: "stale".into(),
        ..Default::default()
    };
    cache.refresh(Duration::ZERO, || panic!("probe failed"));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cache.worker.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    cache.refresh(Duration::from_secs(5), || "unused".into());
    assert_eq!(cache.value, "stale");
    assert!(cache.take_failure().is_some());
    assert!(cache.take_failure().is_none());
}
