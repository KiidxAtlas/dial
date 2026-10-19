//! Help text and the queue of commands typed while a turn is busy.

use crate::app::command_registry::{
    RUNS_NOW, RUNS_NOW_ARGS, STOPS_TURN, canonical_command, command_entries,
};
use crate::app::shell::Shell;
use crate::ui::composer::{CommandEntry, CommandKind};
use crate::ui::keys::Action;

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
            lines.push(row(&self.keys.labels(action), action.describe()));
        }
        lines.push(row("/hotkeys", "view and rebind every shortcut"));
        lines.push(String::new());
        lines.push("While a turn is running".to_owned());
        lines.push(row(
            &self.keys.label(Action::Submit),
            "choose: add direction now or queue for later",
        ));
        lines.push(row(
            &self.keys.labels(Action::FollowUp),
            "queue a follow-up for when the turn ends",
        ));
        lines.push(row(
            &self.keys.label(Action::SelectUp),
            "edit the last queued message (Esc cancels the edit)",
        ));
        lines.push(row(
            "Stops the turn",
            &STOPS_TURN
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            "Runs at once",
            &RUNS_NOW
                .iter()
                .chain(RUNS_NOW_ARGS)
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
        lines.push(row(
            &format!("/model or {}", self.keys.label(Action::ModelSelect)),
            "opens now; the choice applies after the turn",
        ));
        lines.push(row("Anything else", "runs when the turn finishes"));
        // Commands, grouped the same way as the action palette. Advanced sections stay folded
        // unless asked for, so the first screen of help is about everyday work.
        let entries = command_entries(&self.commands, &self.skills);
        let command_row = |entry: &CommandEntry| -> String {
            let name = format!("/{}", entry.name);
            match &entry.args_hint {
                Some(hint) if name.len() + 1 + hint.len() <= LEFT => {
                    row(&format!("{name} {hint}"), &entry.description)
                }
                Some(hint) => format!(
                    "{}\n{}",
                    row(&name, &entry.description),
                    row("", &format!("usage: {name} {hint}"))
                ),
                None => row(&name, &entry.description),
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
        lines.push(
            "/reload re-reads settings, commands, skills and themes from disk (files are also checked every two seconds; invalid edits keep the last good settings).".into(),
        );
        lines.join("\n")
    }

    /// Commands whose purpose is to end or replace what the running turn is doing, so typing one during a turn
    /// stops it. Every other command leaves the turn alone: it runs at once or right after the turn.
    pub(in crate::app) fn stops_turn(line: &str) -> bool {
        let name = line
            .trim()
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        STOPS_TURN.contains(&canonical_command(name))
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
