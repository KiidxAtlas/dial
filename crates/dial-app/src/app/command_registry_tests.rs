use super::*;
use crate::app::command_registry::{
    BUILTIN_COMMANDS, COMMAND_ALIASES, DuringTurn, RUNS_NOW, RUNS_NOW_ARGS, STOPS_TURN,
    canonical_command, command_entries, command_panel, during_turn, during_turn_note,
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
    for (alias, target) in COMMAND_ALIASES {
        assert!(
            registered(target),
            "/{alias} points at unregistered /{target}"
        );
        assert!(
            !registered(alias),
            "/{alias} is listed as a command and an alias"
        );
        assert_eq!(canonical_command(alias), *target);
        assert!(
            listed.iter().all(|entry| entry.name != *alias),
            "/{alias} must stay out of menus, palette and help"
        );
    }
    assert_eq!(canonical_command("exit"), "exit");
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
    for (name, usage, description) in BUILTIN_COMMANDS {
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
                !spec.args_hint.is_empty(),
                "/{} takes subcommands but shows no usage",
                spec.name
            );
        }
    }
}

#[test]
fn turn_lists_drive_stops_turn_and_the_palette_notes_alike() {
    for name in STOPS_TURN {
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
    for name in RUNS_NOW {
        assert!(!Shell::stops_turn(&format!("/{name}")), "/{name} runs now");
        let note = during_turn_note(name);
        assert!(
            note.starts_with("Runs now")
                || note.starts_with("Opens now")
                || note.starts_with("Refreshes"),
            "/{name}: {note}"
        );
    }
    for line in RUNS_NOW_ARGS {
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
