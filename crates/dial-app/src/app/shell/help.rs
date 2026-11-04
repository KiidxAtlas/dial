//! Help text and the queue of commands typed while a turn is busy.

#[cfg(test)]
use crate::app::command_registry::{DuringTurn, during_turn};
use crate::app::command_registry::{command_entries, turn_help};
use crate::app::shell::Shell;
use crate::ui::composer::{CommandEntry, CommandKind};
use crate::ui::keys::Action;

/// One owner for topic names, documentation destinations and the introduction shown in chat.
const HELP_TOPICS: &[(&str, &str, &str)] = &[
    (
        "composer",
        "docs/terminal-composer.md#composer-and-transcript",
        "Write, paste and edit drafts without losing your place; understand the launch screen, transcript and status line.",
    ),
    (
        "command-reference",
        "docs/terminal-commands.md#commands-and-help",
        "Find actions through the goal-first palette, slash completion and command help.",
    ),
    (
        "shortcuts",
        "docs/terminal-shortcuts.md#keyboard-shortcuts",
        "Find the active keys, edit bindings and handle terminal-specific keyboard differences.",
    ),
    (
        "sessions",
        "docs/terminal-sessions.md#sessions-and-history",
        "Resume, search, branch, share and move conversations while preserving their recorded history.",
    ),
    (
        "models",
        "docs/terminal-models.md#models-and-roles",
        "Keep model selection, roles, favorites, downloads and per-model tuning together in the Models panel.",
    ),
    (
        "settings",
        "docs/terminal-settings.md#settings",
        "Search configuration, choose storage scope and review setup presets before saving.",
    ),
    (
        "turns",
        "docs/terminal-turns.md#turns-steering-and-review",
        "Steer running work, queue follow-ups and review recorded outcomes without confusing history with fresh verification.",
    ),
    (
        "terminal",
        "docs/terminal-integration.md#terminal-integration",
        "Use terminal links, titles, progress signals and the optional live side pane.",
    ),
    (
        "stop-scopes",
        "docs/terminal-turns.md#turns-steering-and-review",
        "Stop scope — reading this page stops nothing.\n\nCurrent answer: use the Interrupt key shown in /hotkeys. It requests a stop at the next safe point, not rollback. The agent also requests cancellation of its live workers and side questions when it observes that interruption; do not use it to stop only one worker. An admitted file mutation may finish.\n\nIndividual worker: open /workers, inspect the worker, then use its cancellation control (or /cancel <id>). Cancellation is requested for that worker; the main answer and other workers continue. Already completed edits and external effects remain.\n\nSession-owned shell jobs: open /processes to inspect jobs and output before explicitly killing a selected job. Interrupting an answer or a bg wait does not kill these background processes. They remain managed by bg until explicitly killed, finished, or their owning tool/session ends. Stopping one job does not stop the answer or other jobs.\n\nPersistent jobs: Unix bg jobs started with persist=true survive session exit/restarts until completion, explicit persistent kill, or their lifetime expires. Use bg list/output with persist=true to inspect them, and bg kill with persist=true and the exact name to stop one; ordinary session-job controls are not the persistent-job owner. Scheduled/learning work has separate controls in /activity and /learn; stopping an answer does not disable future scheduling.\n\nStopping is cooperative, not proof that every effect ceased immediately. Inspect results before retrying; no stop action promises to undo files, network requests or other external effects.",
    ),
    (
        "undo-meanings",
        "docs/terminal-turns.md#turns-steering-and-review",
        "Undo meanings — reading this page changes nothing.\n\nReview files first: /review files opens the checkpoint review owner. Inspect recorded changes and choose any revert there; existing overlap warnings and confirmations still apply.\n\nFiles only: /undo files [n] (also /rollback [n]) restores recorded file checkpoints from the latest n checkpoint turns, default 1. It keeps conversation history. These commands execute a restore, not a preview.\n\nFiles and conversation: /undo [n] restores the checkpoints linked to the selected conversation turns, then rewinds history. It keeps the conversation when restoration reports incomplete work; files may already be partly restored.\n\nConversation only: /branch (alias /rewind) opens the message picker; confirm the selected rewind there. It changes the active conversation path, retaining the abandoned branch, but does not restore files or reverse completed tool effects.\n\nSettings inheritance: open /settings and inspect the selected scope and source. Removing an override removes that scope's saved value so the lower-precedence value is inherited (or the built-in default is used). This is not file rollback or conversation rewind, and is not necessarily a reset to the built-in value. Higher-precedence overrides may still win. Saving and adopting agent settings can happen at different boundaries.\n\nCheckpoint limits: only captured file writes are recoverable, within checkpoint workspace roots. Arbitrary shell writes and external effects are not guaranteed to be captured. Missing checkpoints, changed files, overlapping writers and partial restore warnings require inspection. None of these actions promises to undo network requests, processes, commits, remote services or other external side effects. Stop conflicting work and review the owner's warnings before restoring.",
    ),
    (
        "everyday",
        "docs/everyday.md",
        "Start, steer, review, undo and resume everyday work in Dial.",
    ),
    (
        "tools",
        "docs/tools.md",
        "Browse tools, safety boundaries and context sources.",
    ),
    (
        "setup",
        "docs/setup.md",
        "Configure optional capabilities and inspect missing dependencies.",
    ),
];

/// Project the canonical topic metadata into the optional palette mode.
pub(super) fn palette_topics() -> Vec<crate::ui::overlay::palette::UnifiedEntry> {
    use crate::ui::overlay::palette::{UnifiedEntry, UnifiedKind};
    HELP_TOPICS
        .iter()
        .map(|(name, path, introduction)| UnifiedEntry {
            kind: UnifiedKind::Help,
            label: (*name).into(),
            description: format!(
                "{} · {path}",
                introduction.lines().next().unwrap_or_default()
            ),
            command: format!("help {name}"),
            session: None,
        })
        .collect()
}

#[cfg(test)]
mod tests;

/// Topic help is plain text: no editor, model request or runtime dependency on a source checkout.
pub(super) fn topic_help(topic: &str) -> Option<String> {
    HELP_TOPICS.iter().find_map(|(name, path, introduction)| {
        topic
            .eq_ignore_ascii_case(name)
            .then(|| format!("{path}\n\n{introduction}"))
    })
}

/// Exact command help wins over topic introductions; aliases resolve through the registry.
pub(super) fn command_help(name: &str) -> Option<String> {
    let spec = crate::ui::command_surface::command_spec(name.trim().trim_start_matches('/'))?;
    let mut text = format!("/{} {}\n\n{}", spec.name, spec.usage, spec.description);
    if let Some(cli) = spec.cli {
        text.push_str(&format!("\n\nalso: dial {cli}"));
    }
    if let Some(note) = spec.note {
        text.push_str(&format!("\n\n{note}"));
    }
    Some(text)
}

impl Shell {
    pub(super) const SEARCH_USAGE: &str = "Search past sessions in this project.\nUsage: /search <words>\nExample: /search parser error";

    pub(super) const OUTCOME_USAGE: &str = "Rate the last completed turn so improvement runs learn from it.\nUsage: /outcome <0-100> [note]\nExample: /outcome 90 fixed the bug on the first try";

    /// `/help`: how to start, then keys, then turn behaviour, then every command. Rows are
    /// `name  description` pairs so the transcript lays them out as aligned columns.
    pub(super) fn help_text(&self, all: bool) -> String {
        const LEFT: usize = 24;
        let row = |left: &str, right: &str| format!("  {left:<LEFT$}  {right}");
        let palette = self.keys.label(Action::CommandPalette);
        let mut lines = vec![
            "Getting started".to_owned(),
            row(
                "Type and press Enter",
                "ask for anything: explain, fix, build, review",
            ),
            row("/", "open the command menu; keep typing to filter"),
            row(&palette, "search slash commands"),
            row("@path", "attach a file to the message"),
            row(
                "!command",
                "run a shell command yourself; output joins the conversation",
            ),
            row("!!command", "run a shell command without adding its output"),
            String::new(),
            "Keys".to_owned(),
        ];
        for action in [
            Action::CommandPalette,
            Action::ModelSelect,
            Action::ProviderSetup,
            Action::Settings,
            Action::HistorySearch,
            Action::Newline,
            Action::ToolsExpand,
            Action::ApprovalCycle,
            Action::DisplayReset,
            Action::EditExternal,
            Action::Interrupt,
            Action::Exit,
        ] {
            lines.push(row(&self.keys.label(action), action.describe()));
        }
        lines.push(row("/hotkeys", "view and rebind every shortcut"));
        lines.push(String::new());
        lines.push("While a turn is running".to_owned());
        lines.push(row(
            &self.keys.label(Action::Submit),
            "choose: add direction now or queue for later",
        ));
        lines.push(row(
            &self.keys.label(Action::FollowUp),
            "queue a follow-up for when the turn ends",
        ));
        lines.push(row(
            &self.keys.label(Action::SelectUp),
            "edit the last queued message (Esc cancels the edit)",
        ));
        lines.push(row(
            "Stops the turn",
            &turn_help(true)
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            "Runs at once",
            &turn_help(false)
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            &format!("/model or {}", self.keys.label(Action::ModelSelect)),
            "all model panels open now; saved choices apply after the turn",
        ));
        lines.push(row(
            "/model restart",
            "unavailable while busy; never stops inference",
        ));
        lines.push(row("Anything else", "runs when the turn finishes"));
        // Commands, grouped the same way as the action palette. Advanced sections stay folded
        // unless asked for, so the first screen of help is about everyday work.
        let entries = command_entries(&self.commands, &self.skills);
        let command_row = |entry: &CommandEntry| -> String {
            let name = format!("/{}", entry.name);
            let description = crate::ui::command_surface::command_spec(&entry.name)
                .and_then(|spec| spec.cli)
                .map_or_else(
                    || entry.description.clone(),
                    |cli| format!("{} · also: dial {cli}", entry.description),
                );
            match &entry.args_hint {
                Some(hint) if name.len() + 1 + hint.len() <= LEFT => {
                    row(&format!("{name} {hint}"), &description)
                }
                Some(hint) => format!(
                    "{}\n{}",
                    row(&name, &description),
                    row("", &format!("usage: {name} {hint}"))
                ),
                None => row(&name, &description),
            }
        };
        let mut folded = Vec::new();
        for category in crate::ui::command_surface::Category::ALL {
            let members: Vec<&CommandEntry> = entries
                .iter()
                .filter(|entry| {
                    entry.kind == CommandKind::Builtin
                        && crate::ui::command_surface::category(&entry.name) == category
                })
                .collect();
            if members.is_empty() {
                continue;
            }
            if category.advanced() && !all {
                folded.push(format!("{} ({})", category.title(), members.len()));
                continue;
            }
            lines.push(String::new());
            lines.push(category.title().to_owned());
            lines.extend(members.into_iter().map(command_row));
        }
        let custom: Vec<&CommandEntry> = entries
            .iter()
            .filter(|entry| entry.kind != CommandKind::Builtin)
            .collect();
        if !custom.is_empty() {
            lines.push(String::new());
            lines.push("Your commands and skills".to_owned());
            lines.extend(custom.into_iter().map(command_row));
        }
        if !folded.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "More commands in {}. /help all lists them; Ctrl+P searches everything.",
                folded.join(" and ")
            ));
        }
        lines.push(String::new());
        lines.push("/help <topic>: composer, command-reference, shortcuts, sessions, models, settings, turns, terminal, everyday, tools, setup.".into());
        lines.push("/sessions browses all sessions; Ctrl+T opens this session's branches and labels. /sessions branch rewinds; /sessions label bookmarks a message; /sessions fork copies the conversation.".into());
        lines.push("/review offers AI review or keep/revert files; /review files opens checkpoint review directly. /undo files restores files without removing conversation turns.".into());
        lines.push("/todo inspects Plan and Progress tabs. /btw browses side threads. /help commands lists custom commands.".into());
        lines.push(
            "/reload re-reads settings, commands, skills and themes from disk (files are also checked every two seconds; invalid edits keep the last good settings).".into(),
        );
        lines.join("\n")
    }

    /// Commands whose purpose is to end or replace what the running turn is doing, so typing one during a turn
    /// stops it. Every other command leaves the turn alone: it runs at once or right after the turn.
    #[cfg(test)]
    pub(in crate::app) fn stops_turn(line: &str) -> bool {
        let name = line
            .trim()
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        during_turn(name, "") == DuringTurn::Stop
    }

    pub(super) fn queue_command(&mut self, line: &str) {
        let items = self.composer.queued_messages();
        if items.is_empty() {
            self.note("Queue", "No queued messages. While busy, submit a draft to choose between adding direction now and queuing for later; the follow-up shortcut queues directly.");
            return;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        let requested = words
            .get(2)
            .and_then(|number| number.parse::<usize>().ok())
            .and_then(|number| number.checked_sub(1));
        let selected = if words.len() == 1 {
            let labels: Vec<_> = items
                .iter()
                .enumerate()
                .map(|(index, (message, follow_up))| {
                    format!(
                        "{} {}: {}",
                        index + 1,
                        if *follow_up { "follow-up" } else { "steering" },
                        crate::ui::text::ellipsize(&crate::ui::text::one_line(message), 72, true)
                    )
                })
                .collect();
            let options: Vec<_> = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    (
                        label.as_str(),
                        char::from_digit((index + 1) as u32, 36).unwrap_or('?'),
                    )
                })
                .collect();
            self.ask(
                "Queued messages",
                "Choose a message to edit or remove. Esc keeps the queue unchanged.",
                &options,
            )
        } else {
            requested.filter(|index| *index < items.len())
        };
        let Some(index) = selected else {
            if words.len() > 1 {
                self.note(
                    "Queue",
                    "Use /queue, /queue edit <number> or /queue remove <number>. Queue unchanged.",
                );
            }
            return;
        };
        let action = if words.get(1) == Some(&"edit") {
            Some(0)
        } else if words.get(1) == Some(&"remove") {
            Some(1)
        } else if words.len() == 1 {
            self.ask(
                "Queued message",
                &items[index].0,
                &[("Edit", 'e'), ("Remove", 'r'), ("Keep", 'k')],
            )
        } else {
            None
        };
        match action {
            Some(0) if self.composer.edit_queued(index) => self.note("Queue", "Message moved to the composer for editing. Your other queued messages are unchanged."),
            Some(0) => self.note("Queue", "Draft kept. Clear or send it before editing a queued message."),
            Some(1) => { self.composer.remove_queued(index); self.note("Queue", "Removed only the selected queued message."); }
            _ => {}
        }
    }
}
