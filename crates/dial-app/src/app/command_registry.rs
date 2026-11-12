//! The built-in command table, aliases, turn-stopping rules and palette entries.

use crate::agent::window::context;
use crate::app::overlay::{Which, work_panel};
use crate::storage::skills;
use crate::ui::composer::{CommandEntry, CommandKind};
use crate::ui::overlay::palette::PaletteEntry;

pub(in crate::app) mod hubs;

pub(super) use crate::ui::command_surface::{
    ArgumentPolicy, COMMANDS, CommandSpec, DuringTurn, Surface, command_spec,
};

pub(super) fn canonical_command(name: &str) -> &str {
    command_spec(name).map_or(name, |spec| spec.name)
}

/// Historical RPC rows, projected once from the shared metadata (not a second declaration).
pub(super) static BUILTIN_COMMANDS: std::sync::LazyLock<Vec<(&str, &str, &str)>> =
    std::sync::LazyLock::new(|| {
        COMMANDS
            .iter()
            .filter(|spec| spec.surface == Surface::Builtin)
            .map(|spec| (spec.name, spec.usage, spec.description))
            .collect()
    });

pub(super) fn turn_help(stopping: bool) -> Vec<String> {
    let mut names: Vec<_> = COMMANDS
        .iter()
        .filter(|spec| spec.visible || spec.surface == Surface::Undiscovered)
        .filter(|spec| (spec.during_turn == DuringTurn::Stop) == stopping)
        .filter_map(|spec| spec.help_order.map(|order| (order, spec.name.to_owned())))
        .collect();
    names.sort_by_key(|(order, _)| *order);
    let mut result: Vec<_> = names.into_iter().map(|(_, name)| name).collect();
    if !stopping {
        let mut forms: Vec<_> = COMMANDS
            .iter()
            .flat_map(|spec| {
                spec.live_forms.iter().map(move |form| {
                    (
                        form.order,
                        format!(
                            "{}{}{}",
                            spec.name,
                            if form.args.is_empty() { "" } else { " " },
                            form.args
                        ),
                    )
                })
            })
            .collect();
        forms.sort_by_key(|(order, _)| *order);
        result.extend(forms.into_iter().map(|(_, form)| form));
    }
    result
}

/// The turn count for `/undo [n]` and `/rollback [n]`: 1 when omitted, otherwise a whole number
/// of at least 1. `None` for anything else.
pub(super) fn turn_count(args: &str) -> Option<usize> {
    let args = args.trim();
    if args.is_empty() {
        return Some(1);
    }
    args.parse::<usize>().ok().filter(|turns| *turns > 0)
}

pub(super) fn during_turn(name: &str, args: &str) -> DuringTurn {
    let (name, args) = hubs::expand(canonical_command(name), args);
    let name = canonical_command(name);
    if name == "activity" && args == "resume" {
        return DuringTurn::AfterTurn;
    }
    if name == "search" && (args == "back" || args.starts_with("open ")) {
        return DuringTurn::Stop;
    }
    if command_spec(name).is_some_and(|spec| spec.argument_policy == ArgumentPolicy::Model)
        && args == "restart"
    {
        return DuringTurn::Unavailable;
    }
    let spec = command_spec(name);
    if spec.is_some_and(|spec| spec.during_turn == DuringTurn::Stop) {
        return DuringTurn::Stop;
    }
    if spec.is_some_and(|spec| spec.during_turn == DuringTurn::Now) {
        return DuringTurn::Now;
    }
    let sub = args.split_whitespace().next().unwrap_or("");
    let live = match spec.map_or(ArgumentPolicy::None, |spec| spec.argument_policy) {
        ArgumentPolicy::SideThread => sub != "promote",
        ArgumentPolicy::Security => !matches!(
            crate::security::command::parse(args),
            Ok(Some(invocation)) if matches!(invocation.op, "scan" | "validate")
        ),
        ArgumentPolicy::Setup => !matches!(sub, "" | "providers" | "choose" | "memory" | "presets"),
        ArgumentPolicy::Mcp => !sub.starts_with("smithery-") || sub == "smithery-logout",
        ArgumentPolicy::CodeHistory => sub != "clear",
        ArgumentPolicy::Computer => matches!(sub, "" | "status" | "on" | "off"),
        _ => spec.is_some_and(|spec| spec.live_forms.iter().any(|form| form.args == sub)),
    };
    if live {
        DuringTurn::Now
    } else {
        DuringTurn::AfterTurn
    }
}

/// Agent-free panels have one destination table for both idle and busy dispatch.
pub(super) fn command_panel(name: &str, args: &str) -> Option<Which> {
    if let Some(spec) = hubs::picker(name, args) {
        return Some(Which::Choice(Box::new(spec)));
    }
    let (name, args) = hubs::expand(name, args);
    let name = canonical_command(name);
    let args = args.trim();
    match (name, args) {
        ("model", "") => Some(Which::Models),
        ("model", "settings" | "setting") | ("model-settings", "") => Some(Which::ModelSettings),
        ("image", "settings" | "setting") => Some(Which::ImageSettings),
        ("model", "defaults") => Some(Which::ContextDefaults),
        ("model", "backups") => Some(Which::BackupModels),
        ("model", "local") => Some(Which::ModelDownloads),
        ("model", "memory") => Some(Which::ModelMemory),
        ("roles", "") => Some(Which::ModelRoles),
        ("providers", "") => Some(Which::Providers),
        ("hotkeys", "") => Some(Which::Shortcuts),
        ("help", "") => Some(Which::Palette),
        ("controls", "") => Some(Which::Controls),
        ("settings", "") => Some(Which::Settings),
        ("settings", path)
            if crate::config::schema::registry()
                .iter()
                .any(|def| def.path == path) =>
        {
            Some(Which::SettingsAt(path.to_owned()))
        }
        ("palette", "") => Some(Which::Palette),
        ("extensions", "") => Some(Which::Extensions),
        ("extensions", "mcp") => Some(Which::ExtensionsTab(
            crate::ui::overlay::extensions::model::Tab::Mcp,
        )),
        ("extensions", "skills") => Some(Which::ExtensionsTab(
            crate::ui::overlay::extensions::model::Tab::Skills,
        )),
        ("extensions", "tools") => Some(Which::ExtensionsTab(
            crate::ui::overlay::extensions::model::Tab::Tools,
        )),
        ("extensions", "hooks") | ("hooks", "") => Some(Which::ExtensionsTab(
            crate::ui::overlay::extensions::model::Tab::Hooks,
        )),
        ("stats", "") => Some(Which::Stats(String::new())),
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
    let mut specs: Vec<_> = COMMANDS
        .iter()
        .filter(|spec| spec.visible && spec.surface == Surface::Builtin)
        .collect();
    specs.sort_by_key(|spec| spec.category.advanced());
    specs.extend(
        COMMANDS
            .iter()
            .filter(|spec| spec.visible && spec.surface == Surface::Group),
    );
    let mut entries: Vec<CommandEntry> = specs
        .into_iter()
        .map(|spec| CommandEntry {
            name: spec.name.into(),
            description: spec.description.into(),
            kind: CommandKind::Builtin,
            args_hint: (!spec.usage.is_empty()).then(|| spec.usage.to_owned()),
        })
        .collect();
    let reserved = |name: &str| {
        command_spec(name).is_some_and(|spec| {
            spec.surface == Surface::Builtin
                || spec.name != name
                || matches!(spec.name, "roles" | "model-settings" | "status")
        })
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
    let spec = command_spec(name);
    if during_turn(name, "") == DuringTurn::Stop {
        "Stops the current turn before running."
    } else if let Some(note) = spec.and_then(|spec| spec.note) {
        note
    } else if spec.is_some_and(|spec| spec.during_turn == DuringTurn::Now) {
        "Runs now without stopping the current turn."
    } else if spec.is_some_and(|spec| !spec.live_forms.is_empty()) {
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
    entries.extend(
        crate::config::schema::registry()
            .iter()
            .map(|def| PaletteEntry {
                command: CommandEntry {
                    name: format!("settings {}", def.path),
                    description: format!(
                        "Setting · {} / {} · {}. {} {}",
                        def.tab,
                        def.section,
                        def.label,
                        def.description,
                        def.search_terms()
                    ),
                    kind: CommandKind::Builtin,
                    args_hint: None,
                },
                during_turn: "Opens its settings editor without stopping the current turn.",
            }),
    );
    // Getting-started commands lead the palette; the rest keep their registry order (stable sort).
    entries.sort_by_key(|entry| {
        crate::ui::command_surface::STARTERS
            .iter()
            .position(|starter| *starter == entry.command.name)
            .unwrap_or(usize::MAX)
    });
    entries
}

/// One bounded current-project snapshot, loaded only on explicit palette opt-in.
/// Resuming still goes through `/resume` and the ordinary session writer/approval rules.
pub(super) fn palette_sessions(
    layout: &crate::config::paths::Layout,
    current: &str,
) -> anyhow::Result<Vec<crate::ui::overlay::palette::UnifiedEntry>> {
    use crate::ui::overlay::palette::{UnifiedEntry, UnifiedKind};
    let sessions = crate::storage::session::Session::list_for_picker(layout, 200)?;
    Ok(sessions
        .into_iter()
        .filter(|session| session.id != current && session.is_conversation())
        .filter(|session| session.path.parent() == Some(layout.sessions_dir().as_path()))
        .map(|session| UnifiedEntry {
            kind: UnifiedKind::Session,
            label: crate::ui::text::one_line(if session.title.trim().is_empty() {
                &session.id
            } else {
                &session.title
            }),
            description: crate::ui::text::one_line(&format!(
                "{} · {} · {} · {}",
                session.id, session.model, session.first_message, session.last_message
            )),
            command: format!("resume {}", session.id),
            session: Some(session),
        })
        .collect())
}

/// The built-in slash commands as `(name, usage, description)` rows, for RPC clients.
pub(crate) fn builtin_commands() -> &'static [(&'static str, &'static str, &'static str)] {
    &BUILTIN_COMMANDS
}

/// The prompt a `/skill:<name> [task]` line sends: the same text the terminal sends.
pub(crate) fn skill_prompt(skill: &str, task: &str) -> String {
    let task = task.trim();
    let task = if task.is_empty() {
        String::new()
    } else {
        format!(" Task: {task}")
    };
    format!("Use the \"{skill}\" skill (load it with the skill tool first).{task}")
}

/// Expands a prompt that starts with a custom command (`/name args`) or a skill
/// (`/skill:name task`) into the text the terminal would send, for RPC `send` and ACP prompts.
/// `None` leaves the text as is: no leading slash, or text that only happens to start with a
/// slash (a pasted path). A built-in command name is an error rather than text for the model,
/// because it would otherwise look as if the command had run; so is an unknown skill.
pub(crate) fn expand_prompt_command(
    layout: &crate::config::paths::Layout,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let Some(line) = text.trim_start().strip_prefix('/') else {
        return Ok(None);
    };
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    if let Some(skill) = name.strip_prefix("skill:") {
        anyhow::ensure!(
            skills::discover(layout)
                .iter()
                .any(|found| found.name == skill),
            "unknown skill {skill:?}; get_available_commands lists them"
        );
        return Ok(Some(skill_prompt(skill, args)));
    }
    if name.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        command_spec(name).is_none(),
        "/{name} is a Dial command and does not run from a prompt; use the client's matching control or RPC method (get_available_commands lists them), or reword the message"
    );
    Ok(context::discover_commands(layout)
        .iter()
        .find(|def| def.name == name)
        .map(|def| context::expand_command(def, args)))
}

/// Custom and skill commands as RPC rows (`name`, `usage`, `description`, `kind`), in the order
/// the terminal's command menu lists them. `send` expands them (see [`expand_prompt_command`]).
pub(crate) fn prompt_commands(
    layout: &crate::config::paths::Layout,
) -> Vec<(String, String, String, &'static str)> {
    command_entries(
        &context::discover_commands(layout),
        &skills::discover(layout),
    )
    .into_iter()
    .filter_map(|entry| {
        let kind = match entry.kind {
            CommandKind::Custom => "custom",
            CommandKind::Skill => "skill",
            _ => return None,
        };
        Some((
            entry.name,
            entry.args_hint.unwrap_or_default(),
            entry.description,
            kind,
        ))
    })
    .collect()
}
