use super::*;
use crate::app::command_registry::{command_entries, command_panel, command_spec, during_turn};
use crate::app::overlay::Which;

#[test]
fn hubs_route_to_original_handlers_and_keep_their_turn_boundaries() {
    for (hub, args, target, rest) in [
        ("mode", "plan on", "plan", "on"),
        ("mode", "approval readonly", "approval", "readonly"),
        ("mode", "vibe status", "vibe", "status"),
        ("mode", "skillful off", "skillful", "off"),
        (
            "dirs",
            "add path with spaces",
            "add-dir",
            "path with spaces",
        ),
        ("dirs", "remove ../other", "remove-dir", "../other"),
        ("collab", "join secret-link", "join", "secret-link"),
        ("collab", "leave", "leave", ""),
        ("learn", "panel", "improve", ""),
        ("learn", "stop 42", "improve", "stop 42"),
        ("learn", "train", "improve", "train"),
        ("learn", "rollback 3", "improve", "rollback 3"),
        (
            "share",
            "export report name.md --html",
            "export",
            "report name.md --html",
        ),
        ("share", "copy", "copy", ""),
        ("share", "dump anon", "dump", "anon"),
    ] {
        assert_eq!(expand(hub, args), (target, rest));
        assert_eq!(
            during_turn(hub, args),
            during_turn(target, rest),
            "/{hub} {args}"
        );
    }
}

#[test]
fn bare_hubs_are_pickers_and_legacy_execution_records_stay_available() {
    let entries = command_entries(&[], &[]);
    let completions = crate::app::commands::completions(&crate::config::types::Config::default());
    assert!(completions.iter().any(|value| value == "/collab join"));
    for old in ["/vibe ", "/skillful ", "/dump ", "/join"] {
        assert!(
            completions.iter().all(|value| !value.starts_with(old)),
            "{old}"
        );
    }
    for hub in ["mode", "dirs", "collab", "learn", "share"] {
        let spec = command_spec(hub).unwrap();
        assert!(spec.visible && !spec.subcommands.is_empty(), "/{hub}");
        assert!(entries.iter().any(|entry| entry.name == hub));
        let Some(Which::Choice(choice)) = command_panel(hub, "") else {
            panic!("/{hub} needs a picker")
        };
        assert_eq!(choice.command, hub);
        assert!(choice.items.len() >= 3);
        for item in choice.items {
            let sub = item.value.split_whitespace().next().unwrap();
            assert!(spec.subcommands.contains(&sub), "/{hub} {}", item.value);
        }
    }
    for name in [
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
    ] {
        let spec = command_spec(name).unwrap();
        assert_eq!(spec.name, name);
        assert!(!spec.visible);
        assert!(!entries.iter().any(|entry| entry.name == name));
        for args in [
            "",
            "on",
            "off",
            "status",
            "path with spaces",
            "gist whole-journal",
        ] {
            assert_eq!(expand(name, args), (name, args));
        }
    }
    for name in [
        "plan",
        "approval",
        "peers",
        "memory",
        "autoresearch",
        "trace",
    ] {
        assert!(entries.iter().any(|entry| entry.name == name), "/{name}");
    }
}

#[test]
fn argument_actions_return_to_composer_and_learning_panels_keep_destinations() {
    for (hub, args, expected) in [
        ("dirs", "add", "/dirs add "),
        ("dirs", "remove", "/dirs remove "),
        ("collab", "join", "/collab join "),
    ] {
        assert_eq!(draft(hub, args), Some(expected));
        assert!(draft(hub, &format!("{args} value")).is_none());
    }
    assert!(matches!(
        command_panel("learn", "settings"),
        Some(Which::LearningSettings)
    ));
    assert!(matches!(
        command_panel("learn", "models"),
        Some(Which::LearningModels)
    ));
    // Existing upload modifiers retain their distinct path rather than becoming transcript exports.
    for args in ["gist", "whole-journal", "gist whole-journal", "upload"] {
        assert_eq!(expand("share", args), ("share", args));
        assert!(command_panel("share", args).is_none());
    }
}
