use super::*;
use crate::app::command_registry::{
    BUILTIN_COMMANDS, COMMANDS, DuringTurn, Surface, canonical_command, command_entries,
    command_panel, command_spec, during_turn, during_turn_note, turn_help,
};
use crate::app::core::App;
use crate::app::overlay::auth::{AuthJob, PreparedModel};
use crate::app::overlay::env::settings_tables;
use crate::app::shell::Shell;
use crate::config::selection::changes as selection_changes;
use crate::config::settings_store::{self, Scope};
use crate::llm::model_config::ModelSelection;
use crate::ui::command_surface::{EVERYDAY, START_HERE, STARTERS};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// Phase-B intentional menu changes: these names remain executable compatibility deep links.
// The integrator applies this explicit exclusion to the frozen discovery contract.
const GIT_ACTIVITY_MENU_CHANGES: &[&str] = &[
    "workers",
    "processes",
    "compute",
    "schedule",
    "queue",
    "commit",
    "git",
    "activity",
];

#[test]
fn git_and_activity_centralize_discovery_without_removing_legacy_handlers() {
    let entries = command_entries(&[], &[]);
    for name in GIT_ACTIVITY_MENU_CHANGES {
        assert!(command_spec(name).is_some(), "/{name} remains registered");
        assert_eq!(
            entries.iter().any(|entry| entry.name == *name),
            matches!(*name, "git" | "activity"),
            "/{name} discovery visibility"
        );
    }
    assert_eq!(during_turn("activity", "queue"), DuringTurn::Now);
    assert_eq!(during_turn("commit", "--dry-run"), DuringTurn::AfterTurn);
}
/// A name a user can type that dispatches to a real command, not counting aliases.
fn registered(name: &str) -> bool {
    BUILTIN_COMMANDS
        .iter()
        .any(|(builtin, _, _)| *builtin == name)
        || commands::specs().any(|spec| spec.name == name)
}

#[test]
fn every_alias_resolves_to_a_registered_command_and_is_not_listed() {
    let listed = command_entries(&[], &[]);
    for (alias, target) in COMMANDS.iter().flat_map(|spec| {
        spec.aliases
            .iter()
            .map(move |alias| (alias.name, spec.name))
    }) {
        assert!(
            registered(target),
            "/{alias} points at unregistered /{target}"
        );
        assert!(
            !registered(alias),
            "/{alias} is listed as a command and an alias"
        );
        assert_eq!(canonical_command(alias), target);
        assert!(
            listed.iter().all(|entry| entry.name != alias),
            "/{alias} must stay out of menus, palette and help"
        );
    }
    assert_eq!(canonical_command("exit"), "exit");
}

#[test]
fn consolidated_commands_hide_compatibility_names_without_changing_turn_policy() {
    let listed = command_entries(&[], &[]);
    for name in [
        "clear",
        "resume",
        "palette",
        "commands",
        "tree",
        "branch",
        "rollback",
        "review-file",
    ] {
        assert!(listed.iter().all(|entry| entry.name != name), "/{name}");
    }
    for (alias, target) in [
        ("clear", "new"),
        ("resume", "sessions"),
        ("palette", "help"),
    ] {
        assert_eq!(canonical_command(alias), target);
        assert_eq!(during_turn(alias, ""), during_turn(target, ""));
    }
    assert_eq!(during_turn("rollback", "2"), DuringTurn::Stop);
    assert_eq!(during_turn("review-file", ""), DuringTurn::AfterTurn);
}

#[test]
fn starters_and_everyday_commands_are_registered() {
    for name in EVERYDAY.iter().chain(START_HERE).chain(STARTERS) {
        assert!(
            registered(name),
            "/{name} is offered to users but not registered"
        );
    }
    for name in ["compact", "theme", "setup"] {
        assert!(EVERYDAY.contains(&name), "/{name} must be everyday");
    }
}

#[test]
fn descriptions_are_short_and_plain_and_syntax_lives_in_usage() {
    for (name, usage, description) in BUILTIN_COMMANDS.iter() {
        assert!(
            description.chars().count() <= 64,
            "/{name} description is {} characters",
            description.chars().count()
        );
        assert!(
            !description.contains('[') && !description.contains('<'),
            "/{name} puts syntax in its description"
        );
        assert!(!usage.contains('\n'), "/{name} usage is one line");
    }
    for spec in commands::specs() {
        assert!(
            spec.description.chars().count() <= 100,
            "/{} description is {} characters",
            spec.name,
            spec.description.chars().count()
        );
        if !spec.subcommands.is_empty() {
            assert!(
                !spec.usage.is_empty(),
                "/{} takes subcommands but shows no usage",
                spec.name
            );
        }
    }
}

#[test]
fn turn_lists_drive_stops_turn_and_the_palette_notes_alike() {
    for name in COMMANDS
        .iter()
        .filter(|spec| spec.during_turn == DuringTurn::Stop)
        .map(|spec| spec.name)
    {
        assert!(
            registered(name),
            "/{name} stops the turn but is not registered"
        );
        assert!(Shell::stops_turn(&format!("/{name}")), "/{name}");
        assert_eq!(
            during_turn_note(name),
            "Stops the current turn before running."
        );
    }
    for name in COMMANDS
        .iter()
        .filter(|spec| spec.during_turn == DuringTurn::Now)
        .map(|spec| spec.name)
    {
        assert!(!Shell::stops_turn(&format!("/{name}")), "/{name} runs now");
        let note = during_turn_note(name);
        assert!(
            note.starts_with("Runs now")
                || note.starts_with("Opens now")
                || note.starts_with("Refreshes"),
            "/{name}: {note}"
        );
    }
    for line in COMMANDS.iter().flat_map(|spec| {
        spec.live_forms
            .iter()
            .map(move |form| format!("{} {}", spec.name, form.args))
    }) {
        assert!(!Shell::stops_turn(&format!("/{line}")), "/{line} runs now");
    }
    // Aliases follow their target, so /quit stops a turn exactly like /exit.
    assert!(Shell::stops_turn("/quit") && Shell::stops_turn("/q") && Shell::stops_turn("/clear"));
    assert!(!Shell::stops_turn("/review"));
}

#[test]
fn retry_is_registered_without_arguments_and_runs_now_even_for_invalid_args() {
    let entries = command_entries(&[], &[]);
    let retry = entries.iter().find(|entry| entry.name == "retry").unwrap();
    assert!(retry.args_hint.is_none());
    for args in ["", "now", "120"] {
        assert_eq!(during_turn("retry", args), DuringTurn::Now);
        assert!(!Shell::stops_turn(&format!("/retry {args}")));
    }
}

#[test]
fn model_aliases_and_subcommands_share_live_policy_and_panel_destinations() {
    use crate::app::overlay::Which;
    for name in ["model", "models"] {
        for args in [
            "", "settings", "setting", "defaults", "local", "memory", "next", "cancel",
        ] {
            assert_eq!(during_turn(name, args), DuringTurn::Now, "/{name} {args}");
            assert!(!Shell::stops_turn(&format!("/{name} {args}")));
        }
        assert!(matches!(command_panel(name, ""), Some(Which::Models)));
        assert!(matches!(
            command_panel("image", "settings"),
            Some(Which::ImageSettings)
        ));
        assert!(command_panel("image", "generate a fox").is_none());
        assert_eq!(during_turn("image", "settings"), DuringTurn::Now);
        assert!(matches!(
            command_panel(name, "settings"),
            Some(Which::ModelSettings)
        ));
        assert!(matches!(
            command_panel(name, "setting"),
            Some(Which::ModelSettings)
        ));
        assert!(matches!(
            command_panel(name, "defaults"),
            Some(Which::ContextDefaults)
        ));
        assert!(matches!(
            command_panel(name, "local"),
            Some(Which::ModelDownloads)
        ));
        assert!(matches!(
            command_panel(name, "memory"),
            Some(Which::ModelMemory)
        ));
        assert!(command_panel(name, "restart").is_none());
        assert_eq!(during_turn(name, "restart"), DuringTurn::Unavailable);
        assert_eq!(during_turn_note(name), during_turn_note("model"));
    }
    assert_eq!(during_turn("review", ""), DuringTurn::AfterTurn);
    assert_eq!(during_turn("quit", ""), DuringTurn::Stop);
}

#[test]
fn navigation_panels_have_the_same_destinations_while_busy() {
    use crate::app::overlay::Which;
    for name in ["hotkeys", "help"] {
        assert_eq!(during_turn(name, ""), DuringTurn::Now);
        assert!(command_panel(name, "unexpected").is_none());
    }
    assert!(matches!(
        command_panel("hotkeys", ""),
        Some(Which::Shortcuts)
    ));
    assert!(matches!(command_panel("help", ""), Some(Which::Palette)));
    assert!(matches!(command_panel("?", ""), Some(Which::Palette)));
    assert!(command_panel("help", "text").is_none());
    assert!(command_panel("help", "all").is_none());
}

#[test]
fn settings_controls_and_reviewed_recovery_share_registry_routes() {
    use crate::app::overlay::Which;
    assert!(matches!(
        command_panel("controls", ""),
        Some(Which::Controls)
    ));
    assert_eq!(during_turn("controls", ""), DuringTurn::Now);
    assert_eq!(during_turn("activity", "resume"), DuringTurn::AfterTurn);
    assert!(command_panel("activity", "resume").is_none());
    assert!(
        matches!(command_panel("settings", "ui.notify"), Some(Which::SettingsAt(path)) if path == "ui.notify")
    );
    assert!(command_panel("settings", "not.a.setting").is_none());
    let actions = crate::app::command_registry::palette_entries(&[], &[]);
    assert!(
        actions
            .iter()
            .any(|entry| entry.command.name == "settings ui.notify")
    );
}

#[test]
fn model_preparation_only_publishes_successful_uncancelled_results() {
    let (_home, _project, layout, config) = super::tests::config();
    let app = App::new(
        layout.clone(),
        config.clone(),
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    let selection = ModelSelection::new("mlx", "org/prepared");
    for (cancelled, success) in [(true, true), (false, false), (false, true)] {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut job = Some(AuthJob {
            label: "Model",
            receiver,
            lines: None,
            cancel: Arc::new(AtomicBool::new(cancelled)),
            model: Some(PreparedModel {
                changes: selection_changes(&config, &selection, "main").unwrap(),
                role: Some("main".into()),
                scope: Scope::Global,
                voice: None,
                review: None,
            }),
        });
        assert!(AuthJob::collect(&mut job, &app).is_none());
        assert_ne!(app.reload().unwrap().model, selection.model);
        sender
            .send(if success {
                Ok("prepared".into())
            } else {
                Err(anyhow::anyhow!("download failed"))
            })
            .unwrap();
        let (_, result) = AuthJob::collect(&mut job, &app).unwrap();
        assert_eq!(result.is_ok(), success && !cancelled);
        assert!(job.is_none());
        assert_eq!(
            app.reload().unwrap().model == selection.model,
            success && !cancelled
        );
    }
}

#[test]
fn settings_panel_rejects_unreadable_layers_instead_of_empty_defaults() {
    let (_home, _project, layout, config) = super::tests::config();
    let app = App::new(
        layout.clone(),
        config.clone(),
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    crate::platform::atomic::private_dir(&layout.state_dir).unwrap();
    settings_store::write_private(&layout.config_files()[1], b"invalid = [").unwrap();
    let error = settings_tables(&app, &config).unwrap_err();
    assert!(format!("{error:#}").contains("reading project settings"));
}

/// The frozen contract predates consolidation. Explicit menu-only changes are excluded below;
/// legacy execution identities, arguments and scheduling remain covered in full.
#[test]
fn one_table_preserves_the_legacy_command_contract() {
    use serde_json::{Value, json};
    let baseline: Value =
        serde_json::from_str(include_str!("command_registry/legacy_contract.json")).unwrap();
    let added = ["set", "inbox", "doctor", "controls", "attachments", "draft"];
    // Phase B hubs intentionally replace these discovery rows, not their execution behavior.
    const HUB_MENU_CHANGES: &[&str] = &[
        "mode",
        "learn",
        "dirs",
        "collab",
        "share",
        "export",
        "copy",
        "dump",
        "improve",
        "join",
        "leave",
        "add-dir",
        "remove-dir",
        "vibe",
        "skillful",
    ];
    // Extensions gains tab arguments; /hooks becomes a hidden shortcut to its tab.
    const EXTENSIONS_STATS_MENU_CHANGES: &[&str] = &["extensions", "hooks"];
    // Approved next-UX changes have exact replacement contracts below.
    const UX_MENU_CHANGES: &[&str] = &["model", "search", "context"];
    // The worker vocabulary rewords these descriptions; asserted exactly below.
    const WORKER_WORDING_CHANGES: &[&str] = &["message", "cancel", "cleanse"];
    let excluded = |name: &str| {
        HUB_MENU_CHANGES.contains(&name)
            || EXTENSIONS_STATS_MENU_CHANGES.contains(&name)
            || GIT_ACTIVITY_MENU_CHANGES.contains(&name)
            || UX_MENU_CHANGES.contains(&name)
            || WORKER_WORDING_CHANGES.contains(&name)
    };
    let unchanged_hub_row = |row: &&Value| !excluded(row[0].as_str().unwrap());
    let entries: Vec<_> = command_entries(&[], &[])
        .into_iter()
        .filter(|entry| !added.contains(&entry.name.as_str()))
        .filter(|entry| !excluded(entry.name.as_str()))
        .map(|entry| {
            json!([
                entry.name,
                entry.args_hint.unwrap_or_default(),
                entry.description
            ])
        })
        .collect();
    let expected_entries: Vec<_> = baseline["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(unchanged_hub_row)
        .cloned()
        .collect();
    assert_eq!(json!(entries), json!(expected_entries));
    // Worker vocabulary: only the descriptions change ("worker" noun, "stop" verb).
    for (name, description) in [
        ("message", "message a running worker"),
        ("cancel", "stop a running worker"),
        (
            "cleanse",
            "Run project checkers and repair their diagnostics with workers",
        ),
    ] {
        let entry = command_entries(&[], &[])
            .into_iter()
            .find(|entry| entry.name == name)
            .unwrap();
        assert_eq!(entry.description, description, "/{name}");
    }
    let rpc = json!(super::command_registry::builtin_commands());
    let unchanged_rpc = |rows: &Value| {
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|row| {
                !UX_MENU_CHANGES.contains(&row[0].as_str().unwrap())
                    && !WORKER_WORDING_CHANGES.contains(&row[0].as_str().unwrap())
                    && !matches!(row[0].as_str(), Some("attachments" | "draft"))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(unchanged_rpc(&rpc), unchanged_rpc(&baseline["rpc"]));
    assert_eq!(json!(turn_help(true)), baseline["stop"]);
    // Hidden compatibility spellings stay executable, but their top-level help rows are hidden.
    let expected_now: Vec<_> = baseline["now"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|name| {
            let name = name.as_str().unwrap();
            !matches!(name, "leave" | "hooks") && !GIT_ACTIVITY_MENU_CHANGES.contains(&name)
        })
        .cloned()
        .collect();
    assert_eq!(json!(turn_help(false)), json!(expected_now));
    let args = baseline["args"].as_array().unwrap();
    for row in baseline["policies"].as_array().unwrap() {
        let name = row[0].as_str().unwrap();
        // /doctor is intentionally promoted from an alias to a diagnostic command in phase A.
        if name == "doctor" {
            continue;
        }
        assert_eq!(canonical_command(name), row[1].as_str().unwrap(), "{name}");
        assert_eq!(
            format!("{:?}", crate::ui::command_surface::category(name)),
            row[2].as_str().unwrap(),
            "{name}"
        );
        // The worker vocabulary rewords the worker panel's note; its policy is unchanged.
        let note = match row[3].as_str().unwrap() {
            "Opens now; m messages or resumes a worker, x x cancels a running one." => {
                "Opens now; m messages or continues a worker, x x stops a running one."
            }
            other => other,
        };
        assert_eq!(during_turn_note(name), note, "{name}");
        for (arg, expected) in args.iter().zip(row[4].as_array().unwrap()) {
            let arg = arg.as_str().unwrap();
            assert_eq!(
                format!("{:?}", during_turn(name, arg)),
                expected.as_str().unwrap(),
                "/{name} {arg}"
            );
        }
    }
    for name in baseline["hidden"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        assert!(command_spec(name).is_some_and(|spec| spec.name != name || !spec.visible));
    }
}

#[test]
fn command_records_and_aliases_are_unique_and_phase_a_additions_have_expected_policy() {
    let mut names = std::collections::HashSet::new();
    let mut aliases = std::collections::HashSet::new();
    for spec in COMMANDS {
        assert!(names.insert(spec.name), "duplicate /{}", spec.name);
        for alias in spec.aliases {
            assert!(
                aliases.insert(alias.name),
                "duplicate alias /{}",
                alias.name
            );
        }
    }
    assert!(names.is_disjoint(&aliases));
    for (name, expected) in [
        ("set", DuringTurn::Now),
        ("inbox", DuringTurn::AfterTurn),
        ("doctor", DuringTurn::AfterTurn),
    ] {
        let spec = command_spec(name).unwrap();
        assert!(spec.visible && spec.surface == Surface::Group);
        assert_eq!(spec.name, name);
        assert_eq!(during_turn(name, ""), expected);
    }
}

#[test]
fn extension_tabs_and_statistics_have_shared_idle_and_busy_destinations() {
    use crate::app::overlay::Which;
    use crate::ui::overlay::extensions::model::Tab;
    for (args, expected) in [
        ("mcp", Tab::Mcp),
        ("skills", Tab::Skills),
        ("tools", Tab::Tools),
        ("hooks", Tab::Hooks),
    ] {
        assert!(
            matches!(command_panel("extensions", args), Some(Which::ExtensionsTab(tab)) if tab == expected)
        );
        assert_eq!(during_turn("extensions", args), DuringTurn::Now);
    }
    assert!(matches!(
        command_panel("hooks", ""),
        Some(Which::ExtensionsTab(Tab::Hooks))
    ));
    assert!(matches!(command_panel("stats", ""), Some(Which::Stats(_))));
    assert!(command_panel("stats", "--port 9000").is_none());
    assert!(command_panel("stats", "7d").is_none());
    assert!(!command_spec("hooks").unwrap().visible);
    for name in ["cost", "usage", "stats", "skills", "tools"] {
        assert!(
            command_spec(name).unwrap().visible,
            "/{name} has distinct data or actions"
        );
    }
}

#[test]
fn unified_palette_sessions_are_local_conversations_and_use_normal_resume_dispatch() {
    use crate::app::command_registry::palette_sessions;
    use crate::storage::session::{Session, SessionMeta};
    let (_home, _project, layout, _config) = crate::app::tests::config();
    let make = |layout: &crate::config::paths::Layout, request: Option<&str>| {
        let mut session = Session::create(
            layout,
            SessionMeta {
                model: "test-model".into(),
            },
        )
        .unwrap();
        if let Some(request) = request {
            session
                .append(serde_json::json!({ "role": "user", "content": request }))
                .unwrap();
        }
        session
    };
    let current = make(&layout, Some("Current conversation"));
    let empty = make(&layout, None);
    let saved = make(&layout, Some("Callback retry"));
    let elsewhere = tempfile::tempdir().unwrap();
    let foreign_layout =
        crate::config::paths::Layout::with_user_dir(elsewhere.path(), layout.user_dir.clone())
            .unwrap();
    let foreign = make(&foreign_layout, Some("Not in this project"));
    let projected = palette_sessions(&layout, current.id()).unwrap();
    assert_eq!(projected.len(), 1);
    let result = &projected[0];
    assert_eq!(result.command, format!("resume {}", saved.id()));
    assert_eq!(
        result.kind,
        crate::ui::overlay::palette::UnifiedKind::Session
    );
    assert_eq!(result.session.as_ref().unwrap().id, saved.id());
    assert!(result.description.contains("Callback retry"));
    for excluded in [current.id(), empty.id(), foreign.id()] {
        assert!(
            projected
                .iter()
                .all(|entry| entry.session.as_ref().unwrap().id != excluded)
        );
    }
    assert_eq!(canonical_command("resume"), "sessions");
    assert_eq!(
        during_turn("resume", saved.id()),
        during_turn("sessions", saved.id())
    );
}

#[test]
fn next_ux_commands_have_explicit_replacement_contracts_and_safe_busy_routes() {
    for (name, usage, description) in [
        (
            "model",
            "[settings|defaults|local|backups]",
            "choose the main model and assign roles",
        ),
        (
            "search",
            "<query>|results|open <id>|back",
            "search, inspect and open past conversations",
        ),
        (
            "context",
            "",
            "review last and estimated next request budgets",
        ),
        (
            "attachments",
            "[inspect|remove <number>]",
            "inspect or remove draft attachments",
        ),
        (
            "draft",
            "[restore|replace|discard|resume|status]",
            "restore or discard saved unsent input",
        ),
    ] {
        let spec = command_spec(name).unwrap();
        assert_eq!(spec.usage, usage, "{name}");
        assert_eq!(spec.description, description, "{name}");
        assert!(super::command_registry::builtin_commands().contains(&(name, usage, description)));
    }
    assert!(matches!(
        command_panel("model", "backups"),
        Some(crate::app::overlay::Which::BackupModels)
    ));
    assert_eq!(during_turn("search", "open fixture"), DuringTurn::Stop);
    assert_eq!(during_turn("search", "back"), DuringTurn::Stop);
    assert_eq!(during_turn("search", "results"), DuringTurn::Now);
    assert_eq!(during_turn("attachments", ""), DuringTurn::Now);
}
