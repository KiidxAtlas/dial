//! Argument completions for the composer, derived from configuration and skills.

use crate::app::selection::reasoning_profile;
use crate::config::types::{Approval, Config};
use crate::storage::skills;

/// `/memory` argument choices for the selected backend: each backend accepts different
/// subcommands, and an unknown word is a search, so only real subcommands are offered.
fn memory_argument_completions(config: &Config) -> Vec<String> {
    use crate::config::memory_backend::MemoryBackendId;
    let backend = if config.memory.enabled {
        config.memory.backend
    } else {
        MemoryBackendId::Off
    };
    let (words, nested): (&[&str], &[(&str, &[&str])]) = match backend {
        MemoryBackendId::Off => (&["status", "help"], &[]),
        MemoryBackendId::Provenance => (
            &["status", "search", "clear", "help"],
            &[("clear", &["project", "global"])],
        ),
        MemoryBackendId::Semantic => (
            &[
                "status", "search", "save", "forget", "update", "history", "rebuild", "clear",
                "help",
            ],
            &[("clear", &["user"])],
        ),
        MemoryBackendId::Hindsight => (
            &[
                "status", "search", "save", "enqueue", "queue", "mm", "clear", "help",
            ],
            &[(
                "mm",
                &["list", "seed", "show", "history", "refresh", "delete"],
            )],
        ),
        MemoryBackendId::Local => (&["status", "save", "enqueue", "clear", "help"], &[]),
        MemoryBackendId::Sharpshooter => (
            &[
                "status", "search", "stats", "diagnose", "queue", "enqueue", "clear", "help",
            ],
            &[],
        ),
    };
    let mut out: Vec<String> = words.iter().map(|word| format!("/memory {word}")).collect();
    for (word, args) in nested {
        out.extend(args.iter().map(|arg| format!("/memory {word} {arg}")));
    }
    out
}

/// Concrete argument choices, not usage placeholders. Keep these next to the command registry;
/// the composer only matches/renders continuations and never knows command semantics.
pub(super) fn command_argument_completions(
    config: &Config,
    skills: &[skills::Skill],
) -> Vec<String> {
    let mut candidates = Vec::new();
    for (name, args) in [
        (
            "model",
            &[
                "settings", "defaults", "local", "memory", "next", "prev", "cancel", "restart",
            ][..],
        ),
        ("understanding", &["architecture", "build", "clear"][..]),
        ("code-history", &["show", "clear"]),
        ("checks", &["list", "add", "remove", "help"][..]),
        ("experience", &["ledger"]),
        ("laya", &["status", "check", "start", "on", "off"]),
        (
            "skills",
            &[
                "list",
                "sources",
                "import",
                "enable",
                "disable",
                "install",
                "update",
                "remove",
                "installed",
                "search",
            ],
        ),
        ("skills import", &["--dry-run", "--user", "--update"]),
        ("skills install", &["--user"]),
        ("approval", Approval::NAMES),
        ("plan", &["on", "off"]),
        ("permissions", &["allow", "deny", "remove"]),
        ("permissions allow", &["--persist"]),
        ("permissions deny", &["--persist"]),
        (
            "improve",
            &[
                "status", "jobs", "stop", "cycle", "train", "session", "enable", "disable",
                "versions", "tasks",
            ],
        ),
        ("theme", &["auto"]),
        ("export", &["--html"]),
        ("tree", &["forks"]),
        ("profile", &["off"]),
    ] {
        candidates.extend(args.iter().map(|arg| format!("/{name} {arg}")));
    }
    candidates.extend(memory_argument_completions(config));
    for skill in skills {
        for action in ["enable", "disable"] {
            candidates.push(format!("/skills {action} {}", skill.name));
            candidates.push(format!("/skills {action} {} --user", skill.name));
        }
    }
    candidates.extend(
        config
            .model_profiles
            .keys()
            .map(|name| format!("/profile {name}")),
    );
    if let Ok(Some(profile)) = reasoning_profile(config) {
        {
            let command = "effort";
            candidates.extend(
                profile
                    .capabilities
                    .reasoning_efforts
                    .iter()
                    .map(|effort| format!("/{command} {}", effort.as_str())),
            );
        }
    }
    candidates.extend(crate::app::commands::completions(config));
    candidates
}
