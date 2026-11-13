//! Drawing the settings panel: controls, tab bar, search line, rows and footer.
use super::super::DrawCtx;
use super::format::{input_hint, value_text};
use super::memory::memory_summary;
use super::widgets::{
    editor_spans, filled, fit, highlighted, hint, hint_line, local_function_label, match_style,
    pair_label, scope_name, scroll_cell, spans_width, tab_strip,
};
use super::{
    ChoiceAction, EMBEDDING_MODEL_PATH, Item, LINK_SECTION, LINKS, MAX_PICK_ROWS, Mode, PICK_ROWS,
    PROVIDERS_TAB, Row, SettingsOverlay, haystack, pick_matches, search_match,
};
use crate::config::schema::{Kind, Source};
use crate::ui::chrome::{self, Status, status_chip};
use crate::ui::keys::{Action, Keymap};
use crate::ui::text;
use crate::ui::theme::{Sym, Theme, Tok};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

impl SettingsOverlay {
    pub(super) fn compose_controls(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        if self.bulk_review.is_some() {
            return self.bulk_compose(usize::from(w), usize::from(h), ctx);
        }
        let (w, h) = (usize::from(w), usize::from(h));
        let theme = ctx.theme;
        let rows = self.visible();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        let cursor_row = rows.get(self.cursor).copied();
        let body_min = h.min(3);
        let mut spare = h - body_min;
        let mut grant = || {
            if spare > 0 {
                spare -= 1;
                true
            } else {
                false
            }
        };
        // Keep navigation and search visible before optional details on short terminals.
        let (show_footer, show_search, show_tabs, show_scope, show_desc, show_desc2) =
            (grant(), grant(), grant(), grant(), grant(), grant());
        let body_rows = h - [
            show_tabs,
            show_scope,
            show_footer,
            show_desc,
            show_search,
            show_desc2,
        ]
        .iter()
        .filter(|b| **b)
        .count();
        self.page = body_rows.saturating_sub(1).max(1);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(h);
        if show_tabs {
            lines.push(self.tab_bar(theme, w));
        }
        if show_scope {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!(
                        " Editing: {} · Ctrl+S scope · Ctrl+O {}",
                        self.editing_scope_label(),
                        if self.changed_only {
                            "all settings"
                        } else {
                            "changed only"
                        }
                    ),
                    theme.fg(Tok::Accent),
                )],
                w,
            )));
        }
        if show_search {
            lines.push(self.search_line(theme, w));
        }

        let list_w_total = w;
        let searching = !self.search.is_empty() || self.changed_only;
        // Page summaries belong to the scrollable content, never to a changing header.
        let mut items: Vec<Item> = self
            .context_summary(ctx)
            .into_iter()
            .map(Item::Hint)
            .collect();
        let mut cursor_item = 0;
        let mut last_section = "";
        for (i, row) in rows.iter().enumerate() {
            let section = self.section_of(*row);
            if !searching && section != last_section {
                items.push(Item::Header(section));
                last_section = section;
            }
            if i == self.cursor {
                cursor_item = items.len();
            }
            items.push(Item::Row(i));
            if let Row::Setting(s) = row {
                let def = &crate::config::schema::registry()[*s];
                if let Mode::Pick {
                    def: picking,
                    choices,
                    cursor,
                    filter,
                } = &self.mode
                    && picking.path == def.path
                    && i == self.cursor
                {
                    let labels: Vec<String> = choices
                        .iter()
                        .map(|choice| self.choice_label(def, choice))
                        .collect();
                    let visible = pick_matches(&labels, choices, &filter.text);
                    let install_plan = self.download_armed.as_deref().and_then(|armed| {
                        self.value_choices.get(def.path)?.iter().find_map(|offer| {
                            match &offer.action {
                                ChoiceAction::Install { preview } if offer.value == armed => {
                                    Some(preview)
                                }
                                _ => None,
                            }
                        })
                    });
                    if let Some(plan) = install_plan {
                        cursor_item = items.len();
                        items.push(Item::Hint(
                            "Install this backend? Enter: yes; Esc: cancel.".into(),
                        ));
                        for line in plan.lines() {
                            for wrapped in
                                text::wrap_words(line, list_w_total.saturating_sub(8).max(1))
                            {
                                items.push(Item::Hint(wrapped));
                            }
                        }
                    }
                    // Show a window of matches beside the setting row, its section header and the
                    // filter line, capped so a long list (32 themes) never hides the rest of the
                    // panel; typing filters, and the window follows the cursor.
                    let pick_rows = body_rows.saturating_sub(3).clamp(PICK_ROWS, MAX_PICK_ROWS);
                    if choices.len() > PICK_ROWS || !filter.text.is_empty() {
                        items.push(Item::Filter {
                            editor: filter.clone(),
                            shown: visible.len(),
                            total: choices.len(),
                        });
                    }
                    if visible.is_empty() {
                        items.push(Item::Hint(format!(
                            "No option matches \"{}\". {}",
                            text::one_line(&filter.text),
                            if super::appearance::is_appearance(def) {
                                "Backspace edits the filter; Esc discards."
                            } else {
                                "Esc clears the filter."
                            }
                        )));
                    }
                    let at = visible.iter().position(|&n| n == *cursor).unwrap_or(0);
                    let first = at
                        .saturating_sub(pick_rows / 2)
                        .min(visible.len().saturating_sub(pick_rows));
                    for &n in visible.iter().skip(first).take(pick_rows) {
                        if n == *cursor && install_plan.is_none() {
                            cursor_item = items.len();
                        }
                        items.push(Item::Choice {
                            name: labels[n].clone(),
                            on: n == *cursor,
                        });
                    }
                }
                if i == self.cursor
                    && let Some(view) =
                        self.structured_view(def.path, body_rows.saturating_sub(4).max(PICK_ROWS))
                {
                    if let Some(browser) = view.browser {
                        items.push(Item::Hint(format!("In {}", browser.title)));
                        items.push(Item::Filter {
                            editor: browser.filter,
                            shown: browser.shown,
                            total: browser.total,
                        });
                        for (name, on) in browser.rows {
                            if on {
                                cursor_item = items.len();
                            }
                            items.push(Item::Choice { name, on });
                        }
                        if let Some(note) = browser.note {
                            items.push(Item::Hint(note));
                        }
                    }
                    for (name, on) in view.rows {
                        if on {
                            cursor_item = items.len();
                        }
                        items.push(Item::Choice { name, on });
                    }
                    if let Some((prompt, editor)) = view.input {
                        cursor_item = items.len();
                        items.push(Item::Input { prompt, editor });
                    }
                    if let Some(note) = view.note {
                        items.push(Item::Note(note));
                    }
                }
                if let Some(note) = self.notes.get(def.path) {
                    items.push(Item::Note(note.clone()));
                }
                if let Some(hint) = self.hints.get(def.path) {
                    items.push(Item::Hint(hint.clone()));
                }
                if let Some(offer) = self.setup_offers.get(def.path) {
                    if self.setup_armed.as_deref() == Some(offer.id.as_str()) {
                        items.extend(offer.preview.lines().map(|line| Item::Setup {
                            line: line.to_owned(),
                            chip: false,
                        }));
                    } else {
                        items.push(Item::Setup {
                            line: format!("Ctrl+L installs {}", offer.title),
                            chip: true,
                        });
                    }
                }
            }
        }
        let overflow = items.len() > body_rows;
        let list_w = list_w_total.saturating_sub(usize::from(overflow));
        let mut top = cursor_item;
        let mut bottom = cursor_item;
        if matches!(
            items.get(cursor_item),
            Some(Item::Choice { .. } | Item::Input { .. })
        ) {
            // Opening a picker near the bottom must reveal its alternatives, not just its
            // selected value. Keep the setting label with the choices when the block fits.
            while top > 0
                && matches!(
                    items.get(top - 1),
                    Some(Item::Choice { .. } | Item::Filter { .. } | Item::Hint(_))
                )
            {
                top -= 1;
            }
            top = top.saturating_sub(1);
            while matches!(
                items.get(bottom + 1),
                Some(Item::Choice { .. } | Item::Filter { .. } | Item::Input { .. })
            ) {
                bottom += 1;
            }
            if bottom - top + 1 > body_rows {
                top = cursor_item;
                bottom = cursor_item;
            }
        }
        if top > 0 && matches!(items.get(top - 1), Some(Item::Header(_))) {
            top -= 1;
        }
        while matches!(
            items.get(bottom + 1),
            Some(
                Item::Note(_)
                    | Item::Hint(_)
                    | Item::Setup { .. }
                    | Item::Filter { .. }
                    | Item::Input { .. }
            )
        ) {
            bottom += 1;
        }
        if top < self.scroll {
            self.scroll = top;
        }
        if bottom >= self.scroll + body_rows {
            self.scroll = bottom + 1 - body_rows.max(1);
        }
        self.scroll = self.scroll.min(items.len().saturating_sub(body_rows));

        let query_positions = |row: Row| -> Vec<usize> {
            match row {
                Row::Setting(i) if searching => {
                    let def = &crate::config::schema::registry()[i];
                    let label_len = def.label.chars().count();
                    search_match(&self.search, &haystack(def), def.description)
                        .map(|(_, p)| p.into_iter().filter(|p| *p < label_len).collect())
                        .unwrap_or_default()
                }
                Row::Link(i) if searching => {
                    let label = LINKS[i].0;
                    search_match(
                        &self.search,
                        &format!("{label} {LINK_SECTION} {PROVIDERS_TAB}"),
                        LINKS[i].1,
                    )
                    .map(|(_, p)| {
                        p.into_iter()
                            .filter(|p| *p < label.chars().count())
                            .collect()
                    })
                    .unwrap_or_default()
                }
                _ => Vec::new(),
            }
        };

        for y in 0..body_rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let item = items.get(self.scroll + y);
            let list_line = match item {
                None if rows.is_empty() && y == 0 => {
                    let msg = if self.changed_only && self.search.is_empty() {
                        format!(
                            " No saved {} overrides. Ctrl+O shows all settings.",
                            scope_name(self.scope)
                        )
                    } else if searching {
                        format!(
                            " No settings match \"{}\". Esc clears the search.",
                            text::one_line(&self.search)
                        )
                    } else {
                        " Nothing here".to_owned()
                    };
                    filled(
                        vec![Span::styled(msg, theme.fg(Tok::Muted))],
                        list_w,
                        Style::default(),
                    )
                }
                None => filled(vec![], list_w, Style::default()),
                Some(Item::Header(name)) => filled(
                    vec![Span::raw(" "), chrome::label(theme, name)],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Note(note)) => filled(
                    vec![Span::styled(
                        format!("    {} {}", theme.sym(Sym::Cross2), text::one_line(note)),
                        theme.fg(Tok::Error),
                    )],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Hint(hint)) => filled(
                    vec![Span::styled(
                        format!("    {} {}", theme.sym(Sym::Bullet), text::one_line(hint)),
                        theme.fg(Tok::Muted),
                    )],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Setup { line, chip }) => {
                    let mut spans = vec![Span::raw("    ")];
                    if *chip {
                        spans.push(status_chip(theme, Status::Missing, "available"));
                        spans.push(Span::styled(
                            format!(" {}", text::one_line(line)),
                            theme.fg(Tok::Muted),
                        ));
                    } else {
                        spans.push(Span::styled(text::one_line(line), theme.fg(Tok::Muted)));
                    }
                    filled(spans, list_w, Style::default())
                }
                Some(Item::Filter {
                    editor,
                    shown,
                    total,
                }) => {
                    let count = if editor.text.trim().is_empty() {
                        format!("{total} options")
                    } else {
                        format!("{shown} of {total}")
                    };
                    let mut spans = vec![Span::styled("      Filter: ", theme.fg(Tok::Muted))];
                    let room = list_w.saturating_sub(spans_width(&spans) + text::width(&count) + 3);
                    spans.extend(editor_spans(theme, editor, room, false));
                    spans.push(Span::styled(format!("   {count}"), theme.fg(Tok::Muted)));
                    filled(spans, list_w, Style::default())
                }
                Some(Item::Input { prompt, editor }) => {
                    let mut spans = vec![Span::styled(
                        format!("      {prompt}: "),
                        theme.fg(Tok::Muted),
                    )];
                    let room = list_w.saturating_sub(spans_width(&spans) + 1);
                    spans.extend(editor_spans(theme, editor, room, false));
                    filled(spans, list_w, Style::default())
                }
                Some(Item::Choice { name, on }) => {
                    let pointer = if *on {
                        theme.sym(Sym::RadioOn)
                    } else {
                        theme.sym(Sym::RadioOff)
                    };
                    let spans = vec![Span::styled(
                        format!("      {pointer} {}", text::one_line(name)),
                        theme.fg(if *on { Tok::Text } else { Tok::Muted }),
                    )];
                    if *on {
                        chrome::selected_row(theme, spans, list_w)
                    } else {
                        filled(spans, list_w, Style::default())
                    }
                }
                Some(Item::Row(i)) => {
                    let selected = *i == self.cursor;
                    self.row_line(
                        theme,
                        rows[*i],
                        selected,
                        list_w,
                        &query_positions(rows[*i]),
                    )
                }
            };
            spans.extend(list_line.spans);
            if overflow {
                spans.push(scroll_cell(theme, y, body_rows, items.len(), self.scroll));
            }
            lines.push(Line::from(spans));
        }

        if show_desc {
            let wanted = if show_desc2 { 2 } else { 1 };
            // The explanation leads; the path, source and restart state follow it, so a one-row
            // panel still says what the setting does. Save feedback and errors come first.
            let mut head: Vec<(String, Tok)> = Vec::new();
            let mut meta: Option<(String, Tok)> = None;
            if let Some(error) = &self.load_error {
                head.push((error.clone(), Tok::Error));
            }
            match cursor_row {
                Some(Row::Setting(i)) => {
                    let def = &crate::config::schema::registry()[i];
                    if def.path == "memory.backend" && matches!(self.mode, Mode::Browse) {
                        // What the selected backend does and, if setting one up, what remains.
                        if let Some(feedback) = &self.feedback {
                            head.push((feedback.clone(), Tok::Success));
                        }
                        let backend = self.memory_backend();
                        let models = ctx.keys.label(Action::ModelSelect);
                        head.push((memory_summary(&backend, &models), Tok::Text));
                    } else if matches!(self.mode, Mode::Edit { .. }) {
                        let hint = if matches!(def.kind, Kind::Toml) {
                            format!("Editing only the {} layer; inherited values are not copied. {}", scope_name(self.scope), input_hint(def.kind))
                        } else {
                            input_hint(def.kind_for(&self.effective))
                        };
                        head.push((hint, Tok::Muted));
                    } else if let Some(help) = self.structured_help() {
                        head.push((help, Tok::Muted));
                    } else {
                        if let Some(feedback) = &self.feedback {
                            head.push((feedback.clone(), Tok::Success));
                        }
                        meta = Some((self.source_detail(def), Tok::Dim));
                    }
                    head.push((def.description.to_owned(), Tok::Muted));
                }
                Some(Row::Advanced(_, _, count)) => head.push((
                    format!("{count} tuning settings: timeouts, limits and budgets. Search also finds them."),
                    Tok::Muted,
                )),
                Some(Row::Link(i)) => head.push((LINKS[i].1.to_owned(), Tok::Muted)),
                None => {}
            }
            // With two rows, the meta line takes the second so the path and source stay visible.
            let lead = if wanted > 1 && meta.is_some() {
                wanted - 1
            } else {
                wanted
            };
            // Every entry wraps between words; when rows are left over, the last shown row ends
            // with an ellipsis at a word boundary rather than a clipped half word.
            let room = w.saturating_sub(2).max(1);
            let wrapped: Vec<(String, Tok)> = head
                .into_iter()
                .flat_map(|(line, tok)| {
                    text::wrap_words(&text::one_line(&line), room)
                        .into_iter()
                        .map(move |row| (row, tok))
                })
                .collect();
            let hidden = wrapped.len() > lead;
            let mut desc: Vec<(String, Tok)> = wrapped.into_iter().take(lead).collect();
            if hidden && let Some((last, _)) = desc.last_mut() {
                *last = text::ellipsize_words(last, room, theme.ascii(), true);
            }
            if let Some(meta) = meta
                && (wanted > 1 || desc.is_empty())
            {
                desc.push(meta);
            }
            for n in 0..wanted {
                lines.push(match desc.get(n) {
                    Some((line, tok)) => Line::from(fit(
                        vec![Span::styled(
                            format!(" {}", text::one_line(line)),
                            theme.fg(*tok),
                        )],
                        w,
                    )),
                    None => Line::default(),
                });
            }
        }
        if show_footer {
            let hints =
                super::super::panel_keys::capped_footer(self.full_footer(ctx.keys), "F1 keys");
            lines.push(hint_line(theme, &hints, w));
        }
        lines
    }

    /// Every hint for the current mode, the Appearance preview key included; the footer shows a
    /// capped selection and the F1 key sheet lists them all.
    pub(super) fn full_footer(&self, keys: &Keymap) -> Vec<String> {
        let mut hints = self.footer(keys);
        if self.appearance_selected() {
            let preview = local_function_label(keys, 4, Action::ProviderSetup);
            hints.insert(1.min(hints.len()), format!("{preview} preview"));
        }
        hints
    }

    /// Every category with the number of settings saved in the edited scope, Providers last.
    pub(super) fn tab_counts(&self) -> Vec<(&'static str, usize)> {
        self.tabs
            .iter()
            .map(|(name, _)| *name)
            .chain(std::iter::once(PROVIDERS_TAB))
            .map(|name| {
                let count = crate::config::schema::registry()
                    .iter()
                    .filter(|def| {
                        def.tab == name
                            && self.scope_shown(def)
                            && crate::config::schema::get(self.layer_ref(self.scope), def.path)
                                .is_some()
                    })
                    .count();
                (name, count)
            })
            .collect()
    }

    pub(super) fn tab_bar(&self, theme: &Theme, w: usize) -> Line<'static> {
        let names: Vec<String> = self
            .tab_counts()
            .into_iter()
            .map(|(name, count)| {
                if count == 0 {
                    name.to_owned()
                } else {
                    format!("{name} {count}")
                }
            })
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        tab_strip(
            theme,
            &refs,
            self.tab,
            self.search.is_empty() && !self.changed_only,
            w,
        )
    }

    /// The search field reads as an input: a muted label, then the query with its caret, or a
    /// placeholder. It carries no selection marker; only the selected row does.
    pub(super) fn search_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let label = Span::styled(" Search: ", theme.fg(Tok::Muted));
        let spans = if self.search.is_empty() {
            vec![
                label,
                Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
                Span::styled("type a setting name, path or keyword", theme.fg(Tok::Dim)),
            ]
        } else {
            vec![
                label,
                Span::styled(text::one_line(&self.search), theme.fg(Tok::Text)),
                Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
            ]
        };
        Line::from(fit(spans, w))
    }

    pub(super) fn row_line(
        &self,
        theme: &Theme,
        row: Row,
        selected: bool,
        w: usize,
        positions: &[usize],
    ) -> Line<'static> {
        let fill = Style::default();
        let base = |tok: Tok| theme.fg(tok).patch(fill);
        let pointer = if selected {
            theme.sym(Sym::Pointer)
        } else {
            " "
        };
        let mut spans = vec![Span::styled(format!("{pointer} "), base(Tok::Accent))];
        let label_w = (w * 45 / 100).clamp(8, 32).min(w.saturating_sub(2));
        let (label, value_spans, tags, search_tag): (
            String,
            Vec<Span<'static>>,
            Vec<(String, Tok)>,
            String,
        ) = match row {
            Row::Link(i) => (
                LINKS[i].0.to_owned(),
                vec![Span::styled("open", base(Tok::Muted))],
                Vec::new(),
                PROVIDERS_TAB.to_owned(),
            ),
            Row::Advanced(tab, section, count) => {
                let verb = if self.advanced_open.contains(&(tab, section)) {
                    "Hide"
                } else {
                    "Show"
                };
                (
                    format!("{verb} advanced ({count})"),
                    Vec::new(),
                    Vec::new(),
                    tab.to_owned(),
                )
            }
            Row::Setting(i) => {
                let def = &crate::config::schema::registry()[i];
                let editing = match &self.mode {
                    Mode::Edit { def: d, editor } if d.path == def.path => Some(editor),
                    _ => None,
                };
                let value = self.current(def);
                let source = self.source(def.path);
                let value_spans = if let Some(editor) = editing {
                    let room = w.saturating_sub(2 + label_w + 2);
                    let mut s = vec![Span::styled("[", base(Tok::Muted))];
                    s.extend(editor_spans(
                        theme,
                        editor,
                        room,
                        def.sensitive() && !self.reveal_sensitive,
                    ));
                    s.push(Span::styled("]", base(Tok::Muted)));
                    s
                } else {
                    let text = if def.path == EMBEDDING_MODEL_PATH {
                        self.embedding_row_text(&value)
                    } else {
                        value_text(def, &value)
                    };
                    match def.kind_for(&self.effective) {
                        Kind::Bool => {
                            let on = value.as_bool().unwrap_or(false);
                            vec![Span::styled(
                                format!(
                                    "{} {text}",
                                    theme.sym(if on { Sym::RadioOn } else { Sym::RadioOff })
                                ),
                                base(if on { Tok::Success } else { Tok::Muted }),
                            )]
                        }
                        _ => vec![Span::styled(text, base(Tok::Text))],
                    }
                };
                let mut tags: Vec<(String, Tok)> = Vec::new();
                if editing.is_none() {
                    // A common source column makes inheritance explicit, including defaults.
                    tags.push((
                        format!("{} {source}", theme.sym(Sym::Bullet)),
                        if source == "default" {
                            Tok::Dim
                        } else {
                            Tok::Accent
                        },
                    ));
                    if self.application_changes.contains_key(def.path) {
                        tags.push((self.application_timing(def).to_owned(), Tok::Warning));
                    }
                    if def.restart {
                        tags.push(("restart".to_owned(), Tok::Warning));
                    }
                }
                (def.label.to_owned(), value_spans, tags, def.tab.to_owned())
            }
        };
        let label = text::one_line(&label);
        let shown = text::ellipsize(&label, label_w.saturating_sub(1), theme.ascii());
        let label_style = if selected {
            base(Tok::Text).add_modifier(Modifier::BOLD)
        } else {
            base(Tok::Text)
        };
        let hl = match_style(theme, label_style);
        let mut label_spans = if positions.is_empty() {
            vec![Span::styled(shown.clone(), label_style)]
        } else {
            highlighted(&shown, positions, label_style, hl)
        };
        spans.append(&mut label_spans);
        spans.push(Span::styled(
            " ".repeat(label_w.saturating_sub(text::width(&shown))),
            base(Tok::Text),
        ));

        let room = w.saturating_sub(2 + label_w);
        let value_w = spans_width(&value_spans);
        // In search mode the category replaces the tags: a muted tag at the right edge tells
        // where a hit lives without competing with its value.
        let mut tag_spans: Vec<Span<'static>> = Vec::new();
        if !self.search.is_empty() {
            let tag = text::ellipsize(&search_tag, room / 3, theme.ascii());
            let tag_w = text::width(&tag);
            let gap = room.saturating_sub(value_w.min(room) + tag_w + 1);
            if !tag.is_empty() && gap >= 2 {
                tag_spans.push(Span::styled(" ".repeat(gap), base(Tok::Text)));
                tag_spans.push(Span::styled(format!("{tag} "), base(Tok::Dim)));
            }
        } else {
            let mut tags = tags;
            loop {
                let width: usize = tags.iter().map(|(t, _)| text::width(t) + 1).sum::<usize>() + 1;
                if tags.is_empty() || value_w + width <= room {
                    break;
                }
                // Drop the least important tag first: restart, then the source.
                if let Some(p) = tags.iter().position(|(t, _)| t == "restart") {
                    tags.remove(p);
                } else {
                    tags.clear();
                }
            }
            let pad_to = (room * 55 / 100)
                .max(value_w)
                .min(room.saturating_sub(room / 4));
            let pad = pad_to.saturating_sub(value_w);
            if !tags.is_empty() {
                tag_spans.push(Span::styled(" ".repeat(pad.max(1)), base(Tok::Text)));
            }
            for (t, tok) in tags {
                tag_spans.push(Span::styled(format!("{t} "), base(tok)));
            }
        }
        let value_spans = if value_w > room {
            fit(value_spans, room.saturating_sub(1))
                .into_iter()
                .chain(std::iter::once(Span::styled(
                    theme.sym(Sym::Ellipsis).to_owned(),
                    base(Tok::Muted),
                )))
                .collect()
        } else {
            value_spans
        };
        spans.extend(value_spans);
        spans.extend(tag_spans);
        if selected {
            chrome::selected_row(theme, spans, w)
        } else {
            filled(spans, w, fill)
        }
    }

    pub(super) fn footer(&self, keys: &Keymap) -> Vec<String> {
        let pair = |a, b, what| {
            let label = pair_label(keys, a, b);
            if label.is_empty() {
                String::new()
            } else {
                format!("{label} {what}")
            }
        };
        match &self.mode {
            Mode::Edit { def, .. } => {
                let numeric = matches!(
                    def.kind_for(&self.effective),
                    Kind::Int { .. }
                        | Kind::AutoInt { .. }
                        | Kind::Float { .. }
                        | Kind::AutoFloat { .. }
                );
                vec![
                    hint(keys, Action::SelectConfirm, "save"),
                    if def.sensitive() {
                        "Ctrl+R reveal/hide".into()
                    } else {
                        String::new()
                    },
                    if numeric {
                        pair(Action::SelectUp, Action::SelectDown, "adjust")
                    } else {
                        String::new()
                    },
                    hint(keys, Action::SelectCancel, "cancel"),
                ]
            }
            Mode::Pick {
                def,
                choices,
                filter,
                ..
            } => {
                let filtering = !filter.text.is_empty();
                let matched = !self.pick_visible(def, choices, &filter.text).is_empty();
                let mut hints = Vec::new();
                if matched {
                    hints.push(hint(keys, Action::SelectConfirm, "save"));
                    hints.push(pair(
                        Action::SelectUp,
                        Action::SelectDown,
                        if super::appearance::is_appearance(def)
                            || matches!(def.kind_for(&self.effective), Kind::Theme)
                        {
                            "preview"
                        } else {
                            "choose"
                        },
                    ));
                }
                if choices.len() > PICK_ROWS || filtering {
                    hints.push("Type filter".to_owned());
                }
                hints.push(hint(
                    keys,
                    Action::SelectCancel,
                    if filtering && !super::appearance::is_appearance(def) {
                        "clear filter"
                    } else if super::appearance::is_appearance(def) {
                        "discard"
                    } else {
                        "cancel"
                    },
                ));
                hints
            }
            Mode::Path { .. } => vec![
                hint(keys, Action::SelectConfirm, "open/choose"),
                pair(Action::SelectUp, Action::SelectDown, "move"),
                "Type filter or path".to_owned(),
                "Ctrl+E type".to_owned(),
                hint(keys, Action::SelectCancel, "cancel"),
            ],
            Mode::List { editor, .. } => {
                let mut hints = vec![
                    hint(keys, Action::SelectConfirm, "save"),
                    "Space check".to_owned(),
                ];
                if editor.ordered() {
                    hints.push("Shift+↑↓ reorder".to_owned());
                }
                hints.push("Ctrl+E type".to_owned());
                hints.push(hint(keys, Action::SelectCancel, "cancel"));
                hints
            }
            Mode::Entries { .. } => vec![
                hint(keys, Action::SelectConfirm, "edit/save"),
                "Del remove".to_owned(),
                "Ctrl+E TOML text".to_owned(),
                hint(keys, Action::SelectCancel, "cancel"),
            ],
            Mode::Browse => {
                let verb = match self.current_row() {
                    Some(Row::Link(_)) => "open",
                    Some(Row::Advanced(tab, section, _)) => {
                        if self.advanced_open.contains(&(tab, section)) {
                            "hide"
                        } else {
                            "show"
                        }
                    }
                    Some(Row::Setting(i)) => {
                        let def = &crate::config::schema::registry()[i];
                        match (def.kind, def.source()) {
                            (Kind::Bool, _) if super::appearance::is_appearance(def) => "preview",
                            (Kind::Bool, _) => "toggle",
                            (_, Some(Source::Path(_))) => "browse",
                            (_, Some(_))
                            | (Kind::AutoBool | Kind::Enum(_) | Kind::Choice(_) | Kind::Theme, _) => {
                                "choose"
                            }
                            _ => "edit",
                        }
                    }
                    None => "select",
                };
                let mut hints = vec![
                    hint(keys, Action::SelectConfirm, verb),
                    if self.search.is_empty() {
                        "Type search".to_owned()
                    } else {
                        String::new()
                    },
                    pair(Action::SelectUp, Action::SelectDown, "move"),
                    if self.search.is_empty() && !self.changed_only {
                        pair(Action::TabPrev, Action::TabNext, "categories")
                    } else {
                        // Tab does not switch tabs while searching, so do not advertise it then.
                        String::new()
                    },
                    hint(keys, Action::SectionNext, "sections"),
                    "Ctrl+S scope".to_owned(),
                    "Ctrl+O changed only".to_owned(),
                    "Alt+Left back".to_owned(),
                    "Del clear override".to_owned(),
                    "Ctrl+Del review removals".to_owned(),
                    if self.setup_offers.is_empty() {
                        String::new()
                    } else {
                        "Ctrl+L set up".to_owned()
                    },
                    hint(
                        keys,
                        Action::SelectCancel,
                        if !self.search.is_empty() {
                            "clear search"
                        } else if self.changed_only {
                            "all settings"
                        } else {
                            "close"
                        },
                    ),
                ];
                hints.retain(|hint| !hint.is_empty());
                hints
            }
        }
    }
}
