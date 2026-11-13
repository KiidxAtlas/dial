//! The `/` command menu: ranking, the hint line and the rows and detail under the box.
use super::super::{
    Rows,
    keys::Action,
    status::InputMode,
    text,
    theme::{Sym, Theme, Tok},
};
use super::queue::queued_route;
use super::render::{
    Hint, MAX_NAME_COLUMN, finish_row, highlighted, list_rows, render_list, window,
};
use super::{
    CommandEntry, CommandKind, Composer, ComposerCtx, Match, Menu, QueuedEdit, is_slash_command,
};
use crate::llm::model_config::ComposerShape;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{style::Modifier, text::Span};

/// The selected command's description word-wrapped to exactly `lines` rows; only the last row is
/// shortened with an ellipsis.
pub(super) fn detail_description(
    description: &str,
    room: usize,
    ascii: bool,
    lines: usize,
) -> Vec<String> {
    let lines = lines.max(1);
    let mut wrapped = text::wrap_words(description, room.max(1));
    if wrapped.len() > lines {
        let rest = wrapped.split_off(lines - 1).join(" ");
        wrapped.push(text::ellipsize(&rest, room, ascii));
    }
    wrapped.resize(lines, String::new());
    wrapped
}

/// Candidate count above which file ranking moves to a worker thread. Measured in release builds:
/// about 15 ms for 50k paths and under 1 ms for 2k, so anything at or below this rank inline.
pub(super) const SYNC_RANK_LIMIT: usize = 2_000;

/// Commands listed first when `/` is typed with nothing after it. Defined with the other starter
/// lists in `command_surface`, so the menu and the palette cannot disagree.
pub(super) const START_HERE: &[&str] = crate::ui::command_surface::START_HERE;

/// The unfiltered slash menu's groups, in order; the most used come first.
pub(super) const MENU_GROUPS: [crate::ui::command_surface::Category; 10] = {
    use crate::ui::command_surface::Category as C;
    [
        C::Model,
        C::Settings,
        C::Session,
        C::Context,
        C::Review,
        C::Work,
        C::Modes,
        C::Integrations,
        C::Maintenance,
        C::Learning,
    ]
};

/// Built-ins by category, then skills, custom commands, and loaded extension commands.
pub(super) fn group_rank(command: &CommandEntry) -> usize {
    match command.kind {
        CommandKind::Builtin => {
            let category = crate::ui::command_surface::category(&command.name);
            MENU_GROUPS
                .iter()
                .position(|c| *c == category)
                .unwrap_or(MENU_GROUPS.len())
        }
        CommandKind::Skill => MENU_GROUPS.len() + 1,
        CommandKind::Custom => MENU_GROUPS.len() + 2,
        CommandKind::File => MENU_GROUPS.len() + 3,
    }
}

/// The group label for a command in the unfiltered menu.
pub(super) fn group_of(command: &CommandEntry) -> &'static str {
    match command.kind {
        CommandKind::Builtin => crate::ui::command_surface::category(&command.name).tab(),
        CommandKind::Skill => "Skills",
        CommandKind::Custom => "Custom",
        CommandKind::File => "",
    }
}

/// Name matches lead description-only matches; within either field, prefixes lead word starts,
/// then subsequences. Fuzzy score breaks ties and supplies highlight positions.
pub(super) fn rank(commands: &[CommandEntry], query: &str) -> Vec<Match> {
    rank_until(commands, query, &|| false).unwrap_or_default()
}

/// How many candidates a ranking scores between checks of its cancellation flag.
pub(super) const RANK_CANCEL_STRIDE: usize = 256;

/// `rank`, but gives up (returning `None`) once `cancelled` reports true, checked before the first
/// candidate and then every `RANK_CANCEL_STRIDE` candidates, so a superseded query stops early.
pub(super) fn rank_until(
    commands: &[CommandEntry],
    query: &str,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<Match>> {
    let stopped = std::cell::Cell::new(false);
    let lower = query.to_lowercase();
    let tier = |value: &str| {
        if lower.is_empty() {
            return 0u8;
        }
        let value = value.to_lowercase();
        if value.starts_with(&lower) {
            2
        } else if value
            .char_indices()
            .any(|(i, c)| !c.is_alphanumeric() && value[i + c.len_utf8()..].starts_with(&lower))
        {
            1
        } else {
            0
        }
    };
    let mut scored: Vec<((bool, bool, u8, i32), Match)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            if stopped.get() {
                return None;
            }
            if index % RANK_CANCEL_STRIDE == 0 && cancelled() {
                stopped.set(true);
                return None;
            }
            // A scattered subsequence (`settings` in `skill:file-structure-cleanup`) is noise for
            // commands: their names must match tightly or at a word start, their descriptions as
            // a word piece. File paths keep plain fuzzy matching (`mrs` finds `src/main.rs`).
            let name = text::fuzzy(query, &command.name).filter(|(_, hits)| {
                command.kind == CommandKind::File
                    || tier(&command.name) > 0
                    || hits.first().zip(hits.last()).is_none_or(|(first, last)| {
                        let span = last - first + 1;
                        let n = query.chars().count();
                        n < 3 || span <= n + n / 2 + 1
                    })
            });
            // A file's description is the same for every file, so matching it would offer every
            // file for any query that happens to fit "project file".
            let desc = if command.kind == CommandKind::File
                || !command.description.to_lowercase().contains(&lower)
            {
                None
            } else {
                text::fuzzy(query, &command.description)
            };
            if name.is_none() && desc.is_none() {
                return None;
            }
            let exact = !query.is_empty() && command.name.eq_ignore_ascii_case(query);
            let score = match (&name, &desc) {
                (Some((s, _)), _) => (exact, true, tier(&command.name), *s),
                (None, Some((s, _))) => (false, false, tier(&command.description), *s),
                (None, None) => unreachable!(),
            };
            Some((
                score,
                Match {
                    index,
                    name_hits: name.map(|(_, p)| p).unwrap_or_default(),
                    desc_hits: desc.map(|(_, p)| p).unwrap_or_default(),
                },
            ))
        })
        .collect();
    if stopped.get() {
        return None;
    }
    if lower.is_empty() {
        // Nothing typed yet: everyday commands first, grouped (models, setup, sessions, …), each
        // group led by what people reach for first; then everything else, grouped the same way.
        let priority = |name: &str| {
            START_HERE
                .iter()
                .position(|p| p.eq_ignore_ascii_case(name))
                .unwrap_or(START_HERE.len())
        };
        scored.sort_by_key(|(_, m)| {
            let command = &commands[m.index];
            (
                !(command.kind == CommandKind::Builtin
                    && crate::ui::command_surface::everyday(&command.name)),
                group_rank(command),
                priority(&command.name),
            )
        });
    } else {
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    }
    Some(scored.into_iter().map(|(_, m)| m).collect())
}

/// Drops command names that could not be typed as a command (empty, padded with whitespace,
/// slashes, controls) and duplicates, and reduces descriptions and argument hints to one line.
pub(super) fn clean_commands(commands: Vec<CommandEntry>) -> Vec<CommandEntry> {
    let mut seen = std::collections::HashSet::new();
    commands
        .into_iter()
        .filter_map(|c| {
            let bad = c.name.is_empty()
                || c.name.trim() != c.name
                || c.name.chars().any(|ch| ch.is_control() || ch == '/');
            if bad || !seen.insert(c.name.clone()) {
                return None;
            }
            Some(CommandEntry {
                description: text::one_line(&c.description),
                args_hint: c
                    .args_hint
                    .map(|a| text::one_line(&a))
                    .filter(|a| !a.trim().is_empty()),
                ..c
            })
        })
        .collect()
}

impl Composer {
    /// Commands come from the controller so the menu matches real dispatch. Names that could not be
    /// typed as a command (empty, padded with whitespace, slashes, controls) and duplicates are dropped.
    pub fn set_commands(&mut self, commands: Vec<CommandEntry>) {
        self.candidates_generation += 1;
        let selection = self.menu_selection();
        self.commands = clean_commands(commands);
        self.menu = None;
        self.refresh();
        self.restore_selection(selection);
    }

    /// The controller publishes cheap cached state; rendering never probes the workspace.
    pub fn set_command_context(
        &mut self,
        context: crate::ui::command_surface::AvailabilityContext,
    ) {
        self.command_context = context;
    }

    pub(super) fn menu_selection(&self) -> Option<(String, usize, usize)> {
        let menu = self.menu.as_ref()?;
        let hit = menu.matches.get(menu.selected)?;
        Some((
            self.menu_entries(menu).get(hit.index)?.name.clone(),
            menu.selected,
            menu.scroll,
        ))
    }

    pub(super) fn restore_selection(&mut self, selection: Option<(String, usize, usize)>) {
        let Some((name, previous, scroll)) = selection else {
            return;
        };
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let selected = menu
            .matches
            .iter()
            .position(|hit| self.menu_entries(menu)[hit.index].name == name)
            .unwrap_or(previous.min(menu.matches.len().saturating_sub(1)));
        if let Some(menu) = &mut self.menu {
            menu.selected = selected;
            menu.scroll = scroll.min(selected);
        }
    }

    pub(super) fn menu_entries<'a>(&'a self, menu: &'a Menu) -> &'a [CommandEntry] {
        if let Some(arguments) = &menu.arguments {
            arguments
        } else if menu.file_start.is_some() {
            &self.files
        } else {
            &self.commands
        }
    }

    pub(super) fn step_menu(&mut self, delta: isize, wrap: bool, rows: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let total = menu.matches.len() as isize;
        if total == 0 {
            return;
        }
        menu.navigated = true;
        let next = menu.selected as isize + delta;
        menu.selected = if wrap {
            next.rem_euclid(total)
        } else {
            next.clamp(0, total - 1)
        } as usize;
        menu.scroll = window(menu.selected, menu.scroll, rows, menu.matches.len());
    }

    /// Priority-ordered, indivisible hints; disabled or shadowed bindings are not advertised.
    pub(super) fn hint_items(&self, ctx: &ComposerCtx) -> Vec<Hint> {
        if !ctx.ui.hints {
            return Vec::new();
        }
        let keys = ctx.keys;
        let enhanced = self.keyboard_enhanced();
        let label = |action: Action| {
            keys.chords(action)
                .iter()
                .find(|chord| {
                    let key = KeyEvent::new(chord.code, chord.mods);
                    // A chord the terminal cannot report is never advertised (Shift+Enter
                    // without the kitty keyboard protocol arrives as plain Enter).
                    (enhanced || !chord.needs_enhancement())
                    // State-specific actions must not advertise a shadowed shortcut.
                        && !(self.search.is_none()
                            && ((self.editing_queue.is_some()
                                && key.code == KeyCode::Esc
                                && action != Action::Dismiss)
                                || (self.busy
                                    && action != Action::FollowUp
                                    && keys.matches(Action::FollowUp, &key))
                                || (action == Action::Interrupt
                                    && keys.matches(Action::Dismiss, &key)
                                    && (self.menu_open() || self.ghost().is_some()))))
                })
                .map(|chord| chord.display())
                .unwrap_or_default()
        };
        let hint = |action: Action, what: &str| {
            let key = label(action);
            (!key.is_empty()).then(|| Hint {
                key: Some(key),
                what: what.to_owned(),
                warn: false,
            })
        };
        let select = || {
            let labels: Vec<_> = [Action::SelectUp, Action::SelectDown]
                .into_iter()
                .map(label)
                .filter(|s| !s.is_empty())
                .collect();
            (!labels.is_empty()).then(|| Hint {
                key: Some(labels.join("/")),
                what: "select".to_owned(),
                warn: false,
            })
        };
        let keyed = |key: &str, what: &str| {
            Some(Hint {
                key: Some(key.to_owned()),
                what: what.to_owned(),
                warn: false,
            })
        };
        let plain = |what: String| {
            Some(Hint {
                key: None,
                what,
                warn: false,
            })
        };
        let mut pieces = Vec::new();
        if self.busy {
            // Esc is advertised only when it closes something no other hint names: an inline
            // suggestion. Menus, the chooser, search and queue edits carry their own Esc hint.
            if self.search.is_none()
                && self.choice.is_none()
                && self.editing_queue.is_none()
                && !self.menu_open()
                && self.ghost().is_some()
            {
                pieces.push(hint(Action::Dismiss, "close"));
            }
            // Mirrors `handle_key`: a shared Ctrl+C clears a draft before it stops the turn.
            let clears = self.search.is_none()
                && !self.editor.text.is_empty()
                && keys.chords(Action::Interrupt).first().is_some_and(|chord| {
                    keys.matches(Action::ClearDraft, &KeyEvent::new(chord.code, chord.mods))
                });
            // An armed stop leads in the warning color: the next press ends the turn.
            let stop = hint(
                Action::Interrupt,
                if clears {
                    "clear draft"
                } else if self.stop_armed {
                    "again stops"
                } else {
                    "stop"
                },
            )
            .map(|stop| Hint {
                warn: self.stop_armed && !clears,
                ..stop
            });
            pieces.push(stop);
            // The draft a busy Ctrl+C just cleared is one undo away.
            if self.stop_armed && !clears {
                pieces.push(hint(Action::EditorUndo, "restore draft"));
            }
        }
        if self.choice.is_some() {
            pieces.extend([
                hint(Action::Submit, "confirm"),
                hint(Action::Dismiss, "cancel"),
                select(),
            ]);
        } else if self.search.is_some() {
            pieces.extend([
                hint(Action::Submit, "accept"),
                select(),
                hint(Action::Dismiss, "cancel"),
            ]);
        } else {
            if let Some(QueuedEdit { follow_up, .. }) = self.editing_queue {
                pieces.push(plain(format!(
                    "editing queued message ({})",
                    queued_route(follow_up)
                )));
                pieces.push(hint(
                    Action::Submit,
                    if self.busy {
                        "save queued item"
                    } else {
                        "send"
                    },
                ));
                pieces.push(keyed("Esc", "cancel edit"));
            } else if self.exit_armed {
                pieces.push(hint(Action::ClearDraft, "again to exit"));
            } else if self.eof_armed {
                pieces.push(hint(Action::Exit, "again to exit"));
            } else if self.menu_open() {
                pieces.extend([
                    hint(
                        Action::Submit,
                        if self
                            .menu
                            .as_ref()
                            .is_some_and(|menu| menu.file_start.is_some())
                        {
                            "complete"
                        } else {
                            "run command"
                        },
                    ),
                    hint(Action::Complete, "complete"),
                    select(),
                    hint(Action::Dismiss, "dismiss"),
                ]);
            } else if self.busy {
                let command = is_slash_command(&self.editor.text);
                let blank = self.editor.text.trim().is_empty();
                // Mirrors `submit`: an empty Enter asks to stop (only with queued work); a blank draft
                // is ignored; otherwise Enter asks whether to send now or after the turn.
                if command {
                    pieces.push(hint(Action::Submit, "run command"));
                } else if self.editor.text.is_empty() {
                    if !self.queued.is_empty() {
                        pieces.push(hint(Action::Submit, "stop or keep"));
                    }
                    if self.queued.is_empty() {
                        let submit = keys.label(Action::Submit);
                        if !submit.is_empty() {
                            pieces.push(plain(format!(
                                "with a draft: {submit} {}",
                                self.busy_send_hint()
                            )));
                        }
                    }
                } else if !blank {
                    pieces.push(hint(Action::Submit, &self.busy_send_hint()));
                }
                // Alt-Enter only queues a non-blank draft.
                if !command && !blank {
                    pieces.push(hint(Action::FollowUp, "send after turn"));
                }
            } else if !self.editor.text.is_empty() {
                pieces.extend([
                    hint(
                        Action::Submit,
                        if self.mode() == InputMode::Shell {
                            "run"
                        } else {
                            "send"
                        },
                    ),
                    hint(Action::Newline, "newline"),
                ]);
                // The dim inline ghost is the cue for a suggestion; advertising it here made the
                // bar flicker as the suggestion appeared and vanished with each keystroke.
                if self.mode() == InputMode::Shell {
                    pieces.push(plain("shell mode".to_owned()));
                }
            } else {
                pieces.extend([
                    hint(Action::Submit, "send"),
                    keyed("/", "commands"),
                    hint(Action::CommandPalette, "actions"),
                    hint(Action::ModelSelect, "model"),
                    hint(Action::Settings, "settings"),
                    hint(Action::HistorySearch, "history"),
                ]);
            }
        }
        pieces.into_iter().flatten().collect()
    }

    /// The hint texts (`Enter send`, `/ commands`), for assertions.
    #[cfg(test)]
    pub(super) fn hint(&self, ctx: &ComposerCtx) -> Vec<String> {
        self.hint_items(ctx).iter().map(Hint::text).collect()
    }

    pub(super) fn menu_rows(&self, menu: &Menu, w: usize, ctx: &ComposerCtx) -> Rows {
        let theme = ctx.theme;
        // On the band layout the list sits two cells inside an inset panel.
        let panel = ctx.ui.composer_shape == ComposerShape::Band && w >= 24;
        let outer = w;
        let w = if panel { w - 4 } else { w };
        let total = menu.matches.len();
        if total == 0 {
            let message = if menu.file_start.is_some() {
                "  No matching files"
            } else if menu.arguments.is_some() {
                "  No matching subcommands or options"
            } else {
                "  No matching commands"
            };
            return vec![finish_row(
                vec![Span::styled(message, theme.fg(Tok::Dim))],
                w,
            )];
        }
        let rows = list_rows(ctx.ui).min(total);
        let start = window(menu.selected, menu.scroll, rows, total);
        let visible = &menu.matches[start..start + rows];
        let entries = self.menu_entries(menu);
        let name_col = visible
            .iter()
            .map(|m| text::width(&entries[m.index].name))
            .max()
            .unwrap_or(0)
            .min(if menu.file_start.is_some() || menu.arguments.is_some() {
                w.saturating_sub(6)
            } else {
                // A long skill name gives way before the descriptions do.
                MAX_NAME_COLUMN.min((w / 3).max(8))
            });
        let pointer = theme.sym(Sym::Pointer);
        let blank_pointer = " ".repeat(text::width(pointer));
        let accent = theme.fg(Tok::Accent);
        let hit = accent.add_modifier(Modifier::BOLD);
        let groups = menu.file_start.is_none() && menu.arguments.is_none() && menu.query.is_empty();
        let mut rows = render_list(
            theme,
            w,
            total,
            rows,
            start,
            menu.selected,
            |i, budget, selected| {
                let found = &menu.matches[i];
                let command = &entries[found.index];
                let icon = theme.sym(match command.kind {
                    CommandKind::Builtin => Sym::IconCommand,
                    CommandKind::Custom => Sym::IconSettings,
                    CommandKind::Skill => Sym::IconSkill,
                    CommandKind::File => Sym::IconFile,
                });
                let mut spans = vec![
                    Span::styled(
                        if selected {
                            pointer
                        } else {
                            blank_pointer.as_str()
                        }
                        .to_owned(),
                        accent,
                    ),
                    Span::raw(" "),
                ];
                if !icon.is_empty() {
                    spans.push(Span::styled(format!("{icon} "), accent));
                }
                let prefix: usize = spans.iter().map(|s| text::width(&s.content)).sum();
                let name = text::ellipsize(&command.name, name_col, theme.ascii());
                let name_cells = text::width(&name);
                let unavailable = (command.kind == CommandKind::Builtin)
                    .then(|| {
                        crate::ui::command_surface::unavailable_reason(
                            &command.name,
                            "",
                            self.command_context,
                        )
                    })
                    .flatten();
                // While busy, a command that stops the turn says so in place of its description.
                let busy_effect = (command.kind == CommandKind::Builtin && unavailable.is_none())
                    .then(|| {
                        crate::ui::command_surface::busy_effect(&command.name, self.command_context)
                    })
                    .flatten()
                    .map(|effect| format!("{effect} · {}", command.description));
                spans.extend(highlighted(
                    &name,
                    &found.name_hits,
                    theme.fg(if unavailable.is_some() {
                        Tok::Dim
                    } else {
                        Tok::Text
                    }),
                    if unavailable.is_some() {
                        theme.fg(Tok::Dim)
                    } else {
                        hit
                    },
                ));
                spans.push(Span::raw(
                    " ".repeat(name_col.saturating_sub(name_cells) + 2),
                ));
                let used = prefix + name_col + 2;
                // Unfiltered, the list is grouped; each group's first row names it at the right.
                let group = if groups
                    && (i == start
                        || group_of(&entries[menu.matches[i - 1].index]) != group_of(command))
                {
                    group_of(command)
                } else {
                    ""
                };
                let full_description = unavailable
                    .or(busy_effect.as_deref())
                    .unwrap_or(&command.description);
                // A group tag only goes where it costs no description text: a narrow list keeps
                // the words and leaves the grouping to the order.
                let group = if text::width(full_description) + text::width(group) + 2
                    > budget.saturating_sub(used)
                {
                    ""
                } else {
                    group
                };
                let group_w = if group.is_empty() {
                    0
                } else {
                    text::width(group) + 2
                };
                let room = budget.saturating_sub(used + group_w);
                if room > 0 {
                    let description = text::ellipsize(full_description, room, theme.ascii());
                    let description_w = text::width(&description);
                    spans.extend(highlighted(
                        &description,
                        &found.desc_hits,
                        theme.fg(Tok::Muted),
                        hit,
                    ));
                    if !group.is_empty() {
                        spans.push(Span::raw(" ".repeat(room - description_w + 2)));
                        spans.push(Span::styled(group.to_uppercase(), theme.fg(Tok::Dim)));
                    }
                }
                spans
            },
        );
        if menu.file_start.is_none()
            && menu.arguments.is_none()
            && let Some(found) = menu.matches.get(menu.selected)
        {
            let command = &entries[found.index];
            let mut detail = command.clone();
            if command.kind == CommandKind::Builtin
                && let Some(reason) = crate::ui::command_surface::unavailable_reason(
                    &command.name,
                    "",
                    self.command_context,
                )
            {
                detail.description = reason.to_owned();
            } else if command.kind == CommandKind::Builtin
                && let Some(effect) =
                    crate::ui::command_surface::busy_effect(&command.name, self.command_context)
            {
                detail.description = format!("{effect} · {}", command.description);
            }
            // Two description rows when any listed command needs them, so the menu's height
            // stays put while the selection moves.
            let room = w.saturating_sub(2);
            let lines = if menu
                .matches
                .iter()
                .any(|m| text::width(&entries[m.index].description) > room)
            {
                2
            } else {
                1
            };
            rows.extend(Self::command_detail_rows(&detail, w, theme, lines));
        }
        if panel {
            // On the band layout the menu is an inset panel: its kind and position as a title.
            let title = if menu.file_start.is_some() {
                "Files"
            } else if menu.arguments.is_some() {
                "Options"
            } else {
                "Commands"
            };
            // The list draws its own `n/m` counter when it scrolls; the title only names the kind.
            let title = title.to_owned();
            let complete = ctx.keys.label(Action::Complete);
            let hint = if complete.is_empty() {
                String::new()
            } else if theme.ascii() {
                format!("{complete} complete")
            } else {
                format!("↑↓ select · {complete} complete")
            };
            let hint = hint.as_str();
            rows = super::super::chrome::framed_lines(
                theme,
                super::super::chrome::Section::Dial,
                &title,
                hint,
                rows,
                outer,
            );
        }
        rows
    }

    /// Usage and the full description of the selected command, drawn under the menu. Both rows are
    /// ellipsized to the width, so the detail never wraps into the composer.
    /// Usage, then the description on `lines` rows (see [`detail_description`]).
    pub(super) fn command_detail_rows(
        command: &CommandEntry,
        w: usize,
        theme: &Theme,
        lines: usize,
    ) -> Rows {
        let usage = match command.args_hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("/{} {hint}", command.name),
            _ => format!("/{}", command.name),
        };
        let room = w.saturating_sub(2);
        let mut rows = vec![finish_row(
            vec![Span::styled(
                format!(" {}", text::ellipsize(&usage, room, theme.ascii())),
                theme.fg(Tok::Accent),
            )],
            w,
        )];
        rows.extend(
            detail_description(&command.description, room, theme.ascii(), lines)
                .into_iter()
                .map(|line| {
                    finish_row(
                        vec![Span::styled(format!(" {line}"), theme.fg(Tok::Muted))],
                        w,
                    )
                }),
        );
        rows
    }
}
