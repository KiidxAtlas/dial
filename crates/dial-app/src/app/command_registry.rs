//! The built-in command table, aliases, turn-stopping rules and palette entries.

use crate::agent::window::context;
use crate::app::overlay::{Which, work_panel};
use crate::storage::skills;
use crate::ui::composer::{CommandEntry, CommandKind};
use crate::ui::overlay::palette::PaletteEntry;

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Name, arguments syntax and one-line description of every built-in slash command, in the order
/// the lists show them. The syntax lives in the usage column, never in the description. Hidden
/// aliases are in [`COMMAND_ALIASES`]; a usage of `""` means the command takes no arguments.
pub(super) const BUILTIN_COMMANDS: &[(&str, &str, &str)] = &[
    (
        "workers",
        "[id]",
        "open the worker panel: results, cancel and message",
    ),
    (
        "processes",
        "",
        "open background processes and attach to a job's terminal",
    ),
    ("palette", "", "search commands and actions"),
    ("compute", "", "compute jobs and local runtimes"),
    (
        "changes",
        "",
        "list changed files and line counts for this workspace",
    ),
    (
        "help",
        "[all|text]",
        "search actions and commands; /help text lists them",
    ),
    (
        "queue",
        "[edit|remove] <number>",
        "review queued follow-ups; edit or remove one",
    ),
    (
        "review",
        "[uncommitted|base <ref>|ref <ref>|commit <ref>|pr://N|PR URL]",
        "read-only AI review of changes",
    ),
    (
        "review-file",
        "[turn] [path]",
        "keep or revert a checkpoint's changes; AI review is /review",
    ),
    (
        "model",
        "[settings|defaults|local]",
        "choose the main model and assign roles",
    ),
    ("providers", "", "set up providers, keys and endpoints"),
    ("settings", "", "appearance, tools and coding preferences"),
    (
        "reload",
        "",
        "reload settings, commands, skills and themes from disk",
    ),
    ("theme", "[name|auto]", "list or switch the color theme"),
    ("hotkeys", "", "view and edit keyboard shortcuts"),
    ("new", "", "start a fresh session"),
    ("sessions", "", "search and resume sessions"),
    (
        "resume",
        "[id]",
        "search sessions or resume a unique id prefix",
    ),
    ("fork", "", "copy this conversation into a new session"),
    ("rename", "<title>", "name this session"),
    (
        "export",
        "[path] [--html]",
        "export the conversation as Markdown or HTML",
    ),
    (
        "archive",
        "",
        "browse recent projects, sessions and prompts",
    ),
    (
        "autoresearch",
        "[status|init|run|log|notes|resume]",
        "manage branch-scoped benchmark experiments",
    ),
    (
        "tree",
        "[forks]",
        "browse this session's branches; forks browses session forks",
    ),
    ("copy", "", "copy the last assistant answer"),
    ("plan", "[on|off]", "toggle read-only planning mode"),
    ("profile", "[name|off]", "choose a capability profile"),
    (
        "permissions",
        "[allow|deny] [--persist] <tool> [prefix] | remove <number>",
        "list, add or remove allow and deny rules",
    ),
    ("cost", "", "show observed runtime cost and usage coverage"),
    (
        "agents",
        "[definitions]",
        "open the worker panel; list agent definitions",
    ),
    ("message", "<id> <text>", "steer a live subagent"),
    ("cancel", "<id>", "cancel a live subagent"),
    (
        "compact",
        "[soft|remote|snapcompact|handoff|shake] [focus]",
        "compact the conversation context",
    ),
    (
        "undo",
        "",
        "drop the last turn and restore its file changes",
    ),
    (
        "rollback",
        "[n]",
        "restore files changed in the last n turns",
    ),
    (
        "clear",
        "",
        "clear the conversation and start a fresh session",
    ),
    (
        "todo",
        "[list|show <number>]",
        "browse the work plan, blockers and recorded evidence",
    ),
    (
        "skills",
        "[list|install|update|remove|enable|disable|import|sources]",
        "browse, install and manage skills",
    ),
    (
        "memory",
        "[clear [project|global]]",
        "what Dial remembers for this project",
    ),
    (
        "reliability",
        "",
        "live tool success, corrections and verification",
    ),
    (
        "policy",
        "",
        "live task phase, effective settings and request metrics",
    ),
    (
        "steering",
        "",
        "activation vector, evaluation and runtime use",
    ),
    (
        "experience",
        "[ledger]",
        "learned recipes, failures and investigation",
    ),
    (
        "understanding",
        "[architecture|build|clear|query]",
        "how the project fits together",
    ),
    (
        "code-history",
        "[words] [path:<p>] [branch:<b>] | show <id> | clear",
        "search diffs Dial's turns made, across sessions",
    ),
    (
        "laya",
        "[status|check|start|on|off]",
        "local decision advice",
    ),
    ("commands", "", "list custom commands"),
    ("search", "<query>", "search past sessions"),
    ("context", "", "show token usage"),
    (
        "approval",
        "[ask|write|auto|readonly]",
        "choose when tool calls need approval",
    ),
    (
        "outcome",
        "<0-100> [note]",
        "record your verdict on the last answer",
    ),
    (
        "improve",
        "[status|jobs|stop|cycle|train|session|enable|disable]",
        "self-improvement status and job controls",
    ),
    (
        "accounts",
        "pin",
        "pin which ChatGPT account this session uses",
    ),
    ("exit", "", "quit"),
];

/// Hidden aliases: resolved to the target before dispatch, and listed in no menu, palette or help.
pub(super) const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("quit", "exit"),
    ("q", "exit"),
    ("?", "help"),
    ("models", "model"),
    ("provider", "providers"),
    ("reasoning", "effort"),
    ("thinking", "effort"),
    ("jobs", "workers"),
    ("hub", "workers"),
    ("bg", "processes"),
    ("doctor", "setup"),
    ("plugin", "plugins"),
];

/// The dispatch name for `name`, following one hidden alias.
pub(super) fn canonical_command(name: &str) -> &str {
    COMMAND_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, target)| target)
}

/// Commands that end or replace the running turn, so typing one stops the turn first. `stops_turn`,
/// `/help` and the palette all read this list.
pub(super) const STOPS_TURN: &[&str] = &[
    "exit", "new", "clear", "resume", "sessions", "tree", "fork", "undo", "rollback", "compact",
    "plan",
];

/// Commands that run at once while a turn runs, without stopping it (see `live_command`,
/// `commands::run_live` and `commands::run_settings`). Settings they save land at once; the agent
/// adopts them when the turn finishes. `/help` and the palette read this list too.
pub(super) const RUNS_NOW: &[&str] = &[
    "model",
    "roles",
    "model-settings",
    "providers",
    "settings",
    "compute",
    "processes",
    "agents",
    "palette",
    "extensions",
    "reload",
    "help",
    "hotkeys",
    "todo",
    "search",
    "skills",
    "commands",
    "context",
    "reliability",
    "policy",
    "workers",
    "message",
    "cancel",
    "queue",
    "archive",
    "changes",
    "status",
    "leave",
    "usage",
    "marketplace",
    "schedule",
    "gc",
    "images",
    "update",
    "relay",
    "hooks",
    "watch",
    "login",
    "logout",
    "stats",
    "changelog",
    "fast",
    "slow",
    "extended-context",
    "modelpreset",
    "ssh",
    "checks",
    "plugins",
    "reload-plugins",
    "effort",
    "theme",
    "profile",
    "outcome",
];

/// Subcommands that run at once while a turn runs; the command itself is not in [`RUNS_NOW`].
pub(super) const RUNS_NOW_ARGS: &[&str] = &[
    "improve status",
    "improve jobs",
    "improve stop",
    "computer status",
    "computer on",
    "computer off",
    "security plan",
    "security status",
    "security cancel",
    "security scans",
    "security show",
    "security import",
    "security export",
    "security compare",
    "security disposition",
    "security cloud",
    "collab status",
    "collab list",
    "collab stop",
    "collab abort",
    "mcp list",
    "mcp help",
    "mcp add",
    "mcp remove",
    "mcp enable",
    "mcp disable",
    "mcp reload",
    "mcp test",
    "mcp login",
    "mcp logout",
    "mcp resources",
    "mcp prompts",
    "mcp notifications",
    "mcp smithery-logout",
    "setup list",
    "setup <id>",
    "setup embeddings",
    "laya on",
    "laya off",
    "code-history search",
    "code-history show",
    "understanding clear",
    "improve settings",
    "improve models",
];

/// One scheduling decision for dispatch, help and palette annotations. Aliases are resolved before
/// considering arguments; model restart is deliberately refused, not queued or used to interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DuringTurn {
    Now,
    Stop,
    AfterTurn,
    Unavailable,
}

pub(super) fn during_turn(name: &str, args: &str) -> DuringTurn {
    let name = canonical_command(name);
    let args = args.trim();
    if name == "model" && args == "restart" {
        return DuringTurn::Unavailable;
    }
    if STOPS_TURN.contains(&name) {
        return DuringTurn::Stop;
    }
    if RUNS_NOW.contains(&name) {
        return DuringTurn::Now;
    }
    let sub = args.split_whitespace().next().unwrap_or("");
    let live = match name {
        "security" => !matches!(
            crate::security::command::parse(args),
            Ok(Some(invocation)) if matches!(invocation.op, "scan" | "validate")
        ),
        "setup" => !matches!(sub, "" | "providers" | "choose" | "memory" | "presets"),
        "mcp" => !sub.starts_with("smithery-") || sub == "smithery-logout",
        "code-history" => sub != "clear",
        "computer" => matches!(sub, "" | "status" | "on" | "off"),
        _ => RUNS_NOW_ARGS.iter().any(|form| {
            let (command, argument) = form.split_once(' ').unwrap_or((form, ""));
            command == name && argument == sub
        }),
    };
    if live {
        DuringTurn::Now
    } else {
        DuringTurn::AfterTurn
    }
}

/// Agent-free panels have one destination table for both idle and busy dispatch.
pub(super) fn command_panel(name: &str, args: &str) -> Option<Which> {
    let name = canonical_command(name);
    let args = args.trim();
    match (name, args) {
        ("model", "") => Some(Which::Models),
        ("model", "settings" | "setting") | ("model-settings", "") => Some(Which::ModelSettings),
        ("model", "defaults") => Some(Which::ContextDefaults),
        ("model", "local") => Some(Which::ModelDownloads),
        ("model", "memory") => Some(Which::ModelMemory),
        ("roles", "") => Some(Which::ModelRoles),
        ("providers", "") => Some(Which::Providers),
        ("settings", "") => Some(Which::Settings),
        ("palette", "") => Some(Which::Palette),
        ("extensions", "") => Some(Which::Extensions),
        ("improve", "settings") => Some(Which::LearningSettings),
        ("improve", "models") => Some(Which::LearningModels),
        _ => work_panel(name, args),
    }
}

/// Built-ins always win at dispatch, so a custom command with a built-in's name is never offered.
pub(super) fn command_entries(
    custom: &[context::CommandDef],
    skills: &[skills::Skill],
) -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, usage, description)| CommandEntry {
            name: (*name).into(),
            description: (*description).into(),
            kind: CommandKind::Builtin,
            args_hint: (!usage.is_empty()).then(|| (*usage).to_owned()),
        })
        .collect();
    // Advanced sections (self-improvement, reliability, decision services, maintenance) sit after
    // the coding commands so the palette and completion lead with the work. The section is derived
    // from the category, so a label can never disagree with where the command is listed.
    entries.sort_by_key(|entry| crate::ui::command_surface::category(&entry.name).advanced());
    for spec in crate::app::commands::specs() {
        if entries.iter().any(|e| e.name == spec.name) {
            continue;
        }
        entries.push(CommandEntry {
            name: spec.name.into(),
            description: spec.description.into(),
            kind: CommandKind::Builtin,
            args_hint: (!spec.args_hint.is_empty()).then(|| spec.args_hint.to_owned()),
        });
    }
    let reserved = |name: &str| {
        BUILTIN_COMMANDS
            .iter()
            .any(|(builtin, _, _)| *builtin == name)
            || COMMAND_ALIASES.iter().any(|(alias, _)| *alias == name)
            || matches!(name, "roles" | "model-settings" | "status")
    };
    for def in custom.iter().filter(|def| !reserved(&def.name)) {
        if entries.iter().any(|e| e.name == def.name) {
            continue;
        }
        entries.push(CommandEntry {
            name: def.name.clone(),
            description: if def.description.trim().is_empty() {
                "custom command".into()
            } else {
                def.description.clone()
            },
            kind: CommandKind::Custom,
            args_hint: None,
        });
    }
    for skill in skills {
        entries.push(CommandEntry {
            name: format!("skill:{}", skill.name),
            description: if skill.description.trim().is_empty() {
                "run this skill".into()
            } else {
                skill.description.clone()
            },
            kind: CommandKind::Skill,
            args_hint: Some("[task]".into()),
        });
    }
    entries
}

/// What typing `name` during a running turn does. Derived from the same lists dispatch uses.
pub(super) fn during_turn_note(name: &str) -> &'static str {
    let name = canonical_command(name);
    if during_turn(name, "") == DuringTurn::Stop {
        "Stops the current turn before running."
    } else if name == "reload" {
        "Refreshes disk resources now; validated settings apply after this turn."
    } else if name == "model" {
        "Opens now; changes apply after this turn. Restart is unavailable while busy."
    } else if matches!(
        name,
        "roles" | "model-settings" | "providers" | "settings" | "compute"
    ) {
        "Opens now; changes apply after this turn."
    } else if matches!(name, "workers" | "agents") {
        "Opens now; m messages or resumes a worker, x x cancels a running one."
    } else if name == "processes" {
        "Opens now; keystrokes go to the attached job."
    } else if RUNS_NOW.contains(&name) {
        "Runs now without stopping the current turn."
    } else if RUNS_NOW_ARGS
        .iter()
        .any(|line| line.split_whitespace().next() == Some(name))
    {
        "Some forms run now; the rest run after the current turn finishes."
    } else {
        "Runs after the current turn finishes."
    }
}

pub(super) fn palette_entries(
    custom: &[context::CommandDef],
    skills: &[skills::Skill],
) -> Vec<PaletteEntry> {
    let mut entries = command_entries(custom, skills)
        .into_iter()
        .map(|command| PaletteEntry {
            during_turn: during_turn_note(&command.name),
            command,
        })
        .collect::<Vec<_>>();
    // Getting-started commands lead the palette; the rest keep their registry order (stable sort).
    entries.sort_by_key(|entry| {
        crate::ui::command_surface::STARTERS
            .iter()
            .position(|starter| *starter == entry.command.name)
            .unwrap_or(usize::MAX)
    });
    entries
}
