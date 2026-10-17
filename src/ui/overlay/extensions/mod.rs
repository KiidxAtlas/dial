//! `/extensions`: one browser for MCP servers, plugins, skills and marketplaces.
//!
//! The overlay only shows rows and emits [`Intent`]s as `extensions:<json>` actions. Detection,
//! installs and config writes happen in `commands::extensions_browser`.
pub mod model;

use super::{
    DrawCtx, OverlayEvent,
    providers::key_footer,
    settings::{filled, pair_label, typed_char},
};
use crate::ui::{
    chrome::{self, Section, Status},
    keys::{Action, Keymap},
    theme::{Theme, Tok},
};
use crossterm::event::{Event, KeyEventKind};
use model::{
    Browser, Form, FormKind, Intent, Job, JobProgress, Origin, Row, SharedCatalog, SharedProgress,
    State, Tab,
};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub struct ExtensionsOverlay {
    pub browser: Browser,
    /// Written by the install worker; mirrored into `browser` on each tick.
    progress: Option<SharedProgress>,
    /// Pending catalog fetches, started by the command controller.
    catalogs: Vec<(Tab, SharedCatalog)>,
    catalog_job: bool,
    /// Set when a finished install may have changed what is installed; the app refreshes rows.
    needs_refresh: bool,
}

impl Default for ExtensionsOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionsOverlay {
    pub fn new() -> Self {
        Self {
            browser: Browser::new(),
            progress: None,
            catalogs: Vec::new(),
            catalog_job: false,
            needs_refresh: false,
        }
    }

    pub fn attach_catalog(&mut self, tab: Tab, listing: SharedCatalog) {
        self.catalogs.retain(|(pending, _)| *pending != tab);
        self.browser.begin_catalog(tab, String::new(), 1);
        self.catalogs.push((tab, listing));
    }

    fn apply_catalogs(&mut self) {
        let mut ready = Vec::new();
        self.catalogs.retain(|(_, shared)| {
            if let Ok(mut slot) = shared.lock() {
                if let Some(update) = slot.take() {
                    ready.push(update);
                    return false;
                }
            }
            true
        });
        for update in ready {
            self.browser.apply_catalog(update);
        }
    }

    /// Follow the progress of the install started by the app. Replaces any earlier job view.
    pub fn attach(&mut self, progress: SharedProgress) {
        self.progress = Some(progress);
        self.browser.job = Job::Idle;
        self.needs_refresh = false;
    }

    /// Mirror the worker's progress into the view. Called once per UI tick.
    pub fn tick(&mut self) {
        self.apply_catalogs();
        let Some(shared) = &self.progress else {
            return;
        };
        let (snapshot, form, catalog) = match shared.lock() {
            Ok(mut guard) => {
                let form = guard.form.take();
                let catalog = guard.catalog.take();
                (guard.clone(), form, catalog)
            }
            Err(_) => return,
        };
        if let Some(form) = form {
            self.browser.form = Some(form);
        }
        if let Some(update) = catalog {
            self.catalog_job = true;
            self.browser.apply_catalog(update);
        }
        let JobProgress {
            label,
            lines,
            result,
            ..
        } = snapshot;
        match (label, result) {
            (Some(label), None) => {
                if !matches!(self.browser.job, Job::Running { .. }) {
                    self.browser.start_job(label);
                }
                if let Job::Running { lines: shown, .. } = &mut self.browser.job {
                    *shown = lines;
                }
            }
            (label, Some((ok, message))) => {
                if !matches!(self.browser.job, Job::Done { .. }) {
                    if let (Some(label), false) =
                        (label, matches!(self.browser.job, Job::Running { .. }))
                    {
                        self.browser.start_job(label);
                    }
                    self.browser.finish_job(ok, message);
                    self.needs_refresh = !self.catalog_job;
                    self.catalog_job = false;
                }
            }
            (None, None) => {}
        }
    }

    /// Whether a finished install asked for fresh rows. Clears the flag.
    pub fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.needs_refresh)
    }

    /// Replace every tab with freshly built rows.
    pub fn set_snapshot(
        &mut self,
        mcp: Vec<Row>,
        plugins: Vec<Row>,
        skills: Vec<Row>,
        marketplaces: Vec<Row>,
    ) {
        self.browser.set_rows(Tab::Mcp, mcp);
        self.browser.set_rows(Tab::Plugins, plugins);
        self.browser.set_rows(Tab::Skills, skills);
        self.browser.set_rows(Tab::Marketplaces, marketplaces);
    }

    /// Route one key. Returns an action for the app or the controller to run.
    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == KeyEventKind::Release {
            return OverlayEvent::None;
        }
        let b = &mut self.browser;

        if b.form.is_some() {
            return form_key(b, key, keys);
        }

        if b.confirm.is_some() {
            if keys.matches(Action::SelectConfirm, key) {
                return match b.accept() {
                    Some(intent) => action(&intent),
                    None => OverlayEvent::None,
                };
            }
            if keys.matches(Action::SelectCancel, key) || typed_char(key) == Some('n') {
                b.decline();
                b.message = "Cancelled. Nothing was installed.".into();
            }
            return OverlayEvent::None;
        }

        if b.searching() {
            if keys.matches(Action::SelectCancel, key) {
                b.clear_search();
            } else if keys.matches(Action::SelectConfirm, key) {
                b.end_search();
                let tab = b.tab();
                if !matches!(tab, Tab::Plugins | Tab::Marketplaces) {
                    let query = b.query().to_owned();
                    self.catalogs.retain(|(pending, _)| *pending != tab);
                    self.catalog_job = true;
                    return action(&b.begin_catalog(tab, query, 1));
                }
            } else if key.code == crossterm::event::KeyCode::Backspace {
                b.pop_query();
            } else if let Some(c) = typed_char(key) {
                b.push_query(c);
            }
            return OverlayEvent::None;
        }

        if keys.matches(Action::SelectCancel, key) {
            if b.details_open {
                b.toggle_details();
                return OverlayEvent::None;
            }
            if !b.query().is_empty() {
                b.clear_search();
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        if keys.matches(Action::TabNext, key) {
            b.next_tab();
            return OverlayEvent::None;
        }
        if keys.matches(Action::TabPrev, key) {
            b.prev_tab();
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectUp, key) {
            b.move_cursor(-1);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectDown, key) {
            b.move_cursor(1);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectPageUp, key) {
            b.move_cursor(-8);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectPageDown, key) {
            b.move_cursor(8);
            return OverlayEvent::None;
        }
        if key.code == crossterm::event::KeyCode::Home {
            b.move_cursor(isize::MIN / 2);
            return OverlayEvent::None;
        }
        if key.code == crossterm::event::KeyCode::End {
            b.move_cursor(isize::MAX / 2);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectConfirm, key) {
            if let Some(intent) = enter_install(b) {
                return match b.request(intent) {
                    Some(now) => action(&now),
                    None => OverlayEvent::None,
                };
            }
            b.toggle_details();
            return OverlayEvent::None;
        }
        match typed_char(key) {
            Some('/') => {
                b.start_search();
                OverlayEvent::None
            }
            Some('r') | Some('n') => {
                let tab = b.tab();
                let state = b.catalog(tab);
                if state.loading || (typed_char(key) == Some('n') && !state.more) {
                    return OverlayEvent::None;
                }
                let more = typed_char(key) == Some('n');
                let page = if more { state.page + 1 } else { 1 };
                let query = if more {
                    state.query.clone()
                } else {
                    b.query().to_owned()
                };
                self.catalogs.retain(|(pending, _)| *pending != tab);
                self.catalog_job = true;
                action(&b.begin_catalog(tab, query, page))
            }
            Some('a') if b.tab() == Tab::Marketplaces => {
                b.form = Some(Form::marketplace());
                OverlayEvent::None
            }
            Some('v') => {
                b.toggle_details();
                OverlayEvent::None
            }
            Some(c) => match primary_intent(b, c) {
                Some(intent) => match b.request(intent) {
                    Some(now) => action(&now),
                    None => OverlayEvent::None,
                },
                None => OverlayEvent::None,
            },
            None => OverlayEvent::None,
        }
    }

    pub fn banner(&mut self, message: impl Into<String>) {
        self.browser.message = message.into();
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let browser = &self.browser;
        let body = |width: u16, height: u16| {
            let w = usize::from(width);
            let h = usize::from(height);
            let mut lines = Vec::new();

            // Tab strip.
            let mut tabs: Vec<Span<'static>> = vec![Span::raw(" ")];
            for tab in Tab::ALL {
                let style = tab_style(ctx.theme, tab == browser.tab());
                tabs.push(Span::styled(format!(" {} ", tab.label()), style));
                tabs.push(Span::raw(" "));
            }
            lines.push(Line::from(tabs));

            // Search line.
            let search = if browser.searching() {
                let summary = browser.match_summary();
                if summary.is_empty() {
                    format!("/ {}_", browser.query())
                } else {
                    format!("/ {}_  · {}", browser.query(), summary)
                }
            } else if browser.query().is_empty() {
                "/ to search".to_string()
            } else {
                format!(
                    "filter: {}  · {}  · Esc clear search",
                    browser.query(),
                    browser.match_summary()
                )
            };
            lines.push(Line::from(Span::styled(
                format!(" {search}"),
                ctx.theme.fg(Tok::Muted),
            )));
            let catalog = browser.catalog(browser.tab());
            lines.push(Line::from(Span::styled(
                format!(
                    " {} · {} shown",
                    catalog.message,
                    browser.visible(browser.tab()).len()
                ),
                ctx.theme.fg(Tok::Dim),
            )));

            // Confirm / job / message bar.
            if let Some(intent) = &browser.confirm {
                lines.push(Line::from(Span::styled(
                    format!("{}? Enter confirm · Esc cancel", intent.describe()),
                    ctx.theme.fg(Tok::Warning),
                )));
            } else {
                lines.push(job_line(ctx.theme, &browser.job, &browser.message));
            }

            // List and details, or the form in place of them.
            if let Some(form) = &browser.form {
                lines.extend(form_lines(ctx.theme, form));
            } else {
                let body_h = h.saturating_sub(lines.len() + 1);
                let rows = browser.visible(browser.tab());
                let cursor = browser.cursor();
                if browser.details_open {
                    if let Some(row) = browser.selected() {
                        for line in details_lines(ctx.theme, row) {
                            lines.push(line);
                        }
                    }
                } else if rows.is_empty() {
                    let empty = if browser.query().is_empty() {
                        "No catalog entries. r refreshes; / then Enter searches; Marketplaces tab: a adds an index."
                    } else {
                        "No matches on this tab. Left/Right checks the other tabs; Esc clears the search."
                    };
                    lines.push(Line::from(Span::styled(empty, ctx.theme.fg(Tok::Muted))));
                } else {
                    let start = cursor.saturating_sub(body_h.saturating_sub(1));
                    for (i, row) in rows.iter().enumerate().skip(start).take(body_h) {
                        lines.push(row_line(ctx.theme, row, i == cursor, w));
                    }
                }
            }

            let hints: Vec<(String, String)> = if browser.form.is_some() {
                vec![
                    (ctx.keys.label(Action::SelectConfirm), "submit".to_owned()),
                    ("Tab".to_owned(), "next".to_owned()),
                    (ctx.keys.label(Action::SelectCancel), "cancel".to_owned()),
                ]
            } else {
                let esc = if browser.details_open {
                    "back"
                } else if !browser.query().is_empty() {
                    "clear search"
                } else {
                    "close"
                };
                let mut items = vec![
                    (
                        pair_label(ctx.keys, Action::TabPrev, Action::TabNext),
                        "tabs".to_owned(),
                    ),
                    ("/".to_owned(), "search".to_owned()),
                    ("r".to_owned(), "refresh".to_owned()),
                    ("↓".to_owned(), "loads more".to_owned()),
                    (ctx.keys.label(Action::SelectConfirm), enter_label(browser)),
                    ("v".to_owned(), "details".to_owned()),
                    (
                        pair_label(ctx.keys, Action::SelectUp, Action::SelectDown),
                        "move".to_owned(),
                    ),
                    ("t".to_owned(), "test".to_owned()),
                    ("e".to_owned(), "enable".to_owned()),
                    ("x".to_owned(), "remove".to_owned()),
                    (ctx.keys.label(Action::SelectCancel), esc.to_owned()),
                ];
                if browser.tab() == Tab::Marketplaces {
                    items.insert(5, ("a".to_owned(), "add".to_owned()));
                }
                items
            };
            lines.push(key_footer(ctx.theme, &hints, w));
            lines
        };
        let area = frame.area();
        if area.width < 5 || area.height < 5 {
            // Too small for a frame: the body alone.
            let mut lines = body(area.width, area.height);
            lines.truncate(usize::from(area.height));
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
        let block =
            chrome::panel_block_titled(ctx.theme, Section::Connect, "Extensions", "MCP · plugins");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = body(inner.width, inner.height);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn tab_style(theme: &Theme, active: bool) -> Style {
    chrome::tab_style(theme, active)
}

fn action(intent: &Intent) -> OverlayEvent {
    match serde_json::to_string(intent) {
        Ok(json) => OverlayEvent::Action(format!("extensions:{json}")),
        Err(_) => OverlayEvent::None,
    }
}

/// Single-key actions on the selected row. Returns `None` when the key does nothing here.
fn primary_intent(b: &Browser, key: char) -> Option<Intent> {
    let row = b.selected()?;
    let installed = row.origin == Origin::Installed;
    match (b.tab(), key, installed) {
        (Tab::Mcp, 'i', false) if row.origin == Origin::Curated => {
            Some(Intent::InstallCuratedMcp { id: row.id.clone() })
        }
        // The first listing decides how a merged row installs: Smithery through its settings
        // form, every other remote listing through the shared resolver.
        (Tab::Mcp, 'i', false)
            if row.origin == Origin::Remote
                && row.tags.first().map(String::as_str) != Some("smithery") =>
        {
            Some(Intent::InstallMcp {
                id: row.id.clone(),
                values: Default::default(),
            })
        }
        (Tab::Mcp, 'i', false) if row.origin == Origin::Remote => Some(Intent::SmitheryInstall {
            name: row.id.clone(),
        }),
        (Tab::Mcp, 't', true) => Some(Intent::TestMcp { id: row.id.clone() }),
        (Tab::Mcp, 'e', true) => Some(Intent::ToggleMcp {
            id: row.id.clone(),
            enable: row.state == State::Disabled,
        }),
        (Tab::Mcp, 'x', true) => Some(Intent::RemoveMcp { id: row.id.clone() }),
        (Tab::Plugins, 'i', false) => Some(Intent::InstallPlugin { id: row.id.clone() }),
        (Tab::Plugins, 'x', true) => Some(Intent::RemovePlugin { id: row.id.clone() }),
        (Tab::Skills, 'x', true) => Some(Intent::RemoveSkill { id: row.id.clone() }),
        (Tab::Skills, 'i', false) if row.origin == Origin::Remote => Some(Intent::InstallSkill {
            source: row.id.clone(),
        }),
        (Tab::Marketplaces, 'x', true) => Some(Intent::RemoveMarketplace {
            name: row.id.clone(),
        }),
        (Tab::Marketplaces, 'e', true) => Some(Intent::ToggleMarketplace {
            name: row.id.clone(),
            enable: row.state == State::Disabled,
        }),
        _ => None,
    }
}

/// The install that Enter starts on an available row. `None` for installed rows, and for rows
/// with nothing to install, where Enter shows details instead.
fn enter_install(b: &Browser) -> Option<Intent> {
    let row = b.selected()?;
    if row.origin == Origin::Installed {
        return None;
    }
    primary_intent(b, 'i')
}

/// Footer label for Enter on the selected row.
fn enter_label(b: &Browser) -> String {
    if enter_install(b).is_some() {
        "install".to_owned()
    } else {
        "details".to_owned()
    }
}

/// The open form: title, note, one line per field (secrets as dots), and the last error.
fn form_lines(theme: &crate::ui::theme::Theme, form: &Form) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            form.title.clone(),
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(form.note.clone(), theme.fg(Tok::Muted))),
    ];
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let shown = if field.secret {
            "•".repeat(field.value.chars().count())
        } else {
            field.value.clone()
        };
        let marker = if focused { "› " } else { "  " };
        let label = if field.required {
            format!("{} *", field.label)
        } else {
            field.label.clone()
        };
        let mut spans = vec![
            Span::styled(
                format!("{marker}{label:<16}"),
                theme.fg(if focused { Tok::Text } else { Tok::Dim }),
            ),
            Span::styled(
                format!("{shown}{}", if focused { "_" } else { "" }),
                theme.fg(Tok::Text),
            ),
        ];
        if !field.hint.is_empty() {
            spans.push(Span::styled(
                format!("  {}", field.hint),
                theme.fg(Tok::Muted),
            ));
        }
        lines.push(Line::from(spans));
    }
    if let Some(error) = &form.error {
        lines.push(Line::from(vec![
            chrome::status_chip(theme, Status::Error, "error"),
            Span::styled(format!(" {error}"), theme.fg(Tok::Text)),
        ]));
    }
    lines
}

/// Keys while a form is open: type into the focused field, move between fields, submit or cancel.
fn form_key(b: &mut Browser, key: &crossterm::event::KeyEvent, keys: &Keymap) -> OverlayEvent {
    use crossterm::event::KeyCode;
    if keys.matches(Action::SelectCancel, key) {
        b.form = None;
        b.message = "Cancelled. Nothing was installed.".into();
        return OverlayEvent::None;
    }
    if keys.matches(Action::SelectConfirm, key) {
        return submit_form(b);
    }
    let Some(form) = b.form.as_mut() else {
        return OverlayEvent::None;
    };
    match key.code {
        KeyCode::Tab | KeyCode::Down => form.focus_next(),
        KeyCode::BackTab | KeyCode::Up => form.focus_prev(),
        KeyCode::Backspace => form.backspace(),
        _ => {
            if let Some(c) = typed_char(key) {
                form.push(c);
            }
        }
    }
    OverlayEvent::None
}

/// Check the form's required fields, then turn it into the intent its kind asks for.
fn submit_form(b: &mut Browser) -> OverlayEvent {
    let Some(form) = b.form.as_mut() else {
        return OverlayEvent::None;
    };
    if let Some(label) = form.missing_required().map(str::to_owned) {
        form.error = Some(format!("{label} is required."));
        return OverlayEvent::None;
    }
    let Some(form) = b.form.take() else {
        return OverlayEvent::None;
    };
    let values = form.values();
    let intent = match form.kind {
        FormKind::Key { name } => Intent::SmitheryKey {
            name,
            key: values.get("api_key").cloned().unwrap_or_default(),
        },
        FormKind::Config { name } => Intent::SmitheryDeploy { name, values },
        FormKind::McpInputs { id } => Intent::InstallMcp { id, values },
        FormKind::Marketplace => Intent::AddMarketplace {
            name: values.get("name").cloned().unwrap_or_default(),
            index: values.get("index").cloned().unwrap_or_default(),
        },
    };
    match b.request(intent) {
        Some(now) => action(&now),
        None => OverlayEvent::None,
    }
}

fn state_status(state: State) -> Status {
    match state {
        State::Connected | State::Enabled => Status::Ready,
        State::Available | State::Disabled => Status::Missing,
        State::NeedsSignIn => Status::Warn,
        State::Failing => Status::Error,
    }
}

fn row_line(
    theme: &crate::ui::theme::Theme,
    row: &Row,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    // Columns: name, status, then the summary, so rows read down as a table.
    let title_w = (width / 4).clamp(12, 28);
    let title = crate::ui::text::ellipsize(
        &crate::ui::text::one_line(&row.title),
        title_w,
        theme.ascii(),
    );
    let chip = chrome::status_chip(theme, state_status(row.state), row.state.label());
    let chip_w: usize = 14;
    let chip_pad = chip_w.saturating_sub(crate::ui::text::width(&chip.content));
    let mut spans = vec![
        Span::styled(format!(" {marker}"), theme.fg(Tok::Accent)),
        Span::styled(
            format!(
                "{title:<title_w$}",
                title_w = title_w.max(crate::ui::text::width(&title))
            ),
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        chip,
        Span::raw(" ".repeat(chip_pad)),
        Span::styled(row.summary.clone(), theme.fg(Tok::Muted)),
    ];
    for tag in &row.tags {
        spans.push(Span::styled(format!(" #{tag}"), theme.fg(Tok::Dim)));
    }
    let style: Style = if selected {
        chrome::selected_style(theme)
    } else {
        Style::default()
    };
    filled(spans, width, style)
}

fn details_lines(theme: &crate::ui::theme::Theme, row: &Row) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(chrome::tag(theme, Section::Connect, &row.title))];
    out.push(Line::from(vec![
        chrome::status_chip(theme, state_status(row.state), row.state.label()),
        Span::styled(format!(" {}", row.id), theme.fg(Tok::Muted)),
    ]));
    for detail in &row.details {
        out.push(Line::from(Span::styled(
            detail.clone(),
            theme.fg(Tok::Text),
        )));
    }
    out
}

fn job_line(theme: &crate::ui::theme::Theme, job: &Job, message: &str) -> Line<'static> {
    match job {
        Job::Running { label, lines } => {
            let last = lines.last().cloned().unwrap_or_default();
            Line::from(vec![
                chrome::status_chip(theme, Status::Warn, "working"),
                Span::styled(format!(" {label}… {last}"), theme.fg(Tok::Text)),
            ])
        }
        Job::Done { label, ok, message } => {
            let status = if *ok { Status::Ready } else { Status::Error };
            let chip = if *ok { "done" } else { "failed" };
            Line::from(vec![
                chrome::status_chip(theme, status, chip),
                Span::styled(format!(" {label}: {message}"), theme.fg(Tok::Text)),
            ])
        }
        Job::Idle => Line::from(Span::styled(message.to_string(), theme.fg(Tok::Muted))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, origin: Origin, state: State) -> Row {
        Row {
            id: id.into(),
            title: id.into(),
            summary: String::new(),
            tags: vec![],
            origin,
            state,
            details: vec![],
        }
    }

    #[test]
    fn arrow_navigation_renders_every_tab_and_wraps() {
        use crate::ui::overlay::settings::testkit::{Rig, key, render, screen, status_preview};
        use crossterm::event::KeyCode;

        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        for (width, height) in [(120, 30), (40, 12), (4, 4)] {
            let mut overlay = ExtensionsOverlay::new();
            for tab in Tab::ALL {
                overlay
                    .browser
                    .set_rows(tab, vec![row("fixture", Origin::Installed, State::Enabled)]);
            }
            for direction in [KeyCode::Right, KeyCode::Left] {
                for _ in Tab::ALL {
                    assert!(matches!(
                        overlay.handle(&key(direction), &rig.keys),
                        OverlayEvent::None
                    ));
                    let buffer = render(width, height, |frame| overlay.draw(frame, &ctx));
                    if width == 120 {
                        assert!(screen(&buffer).contains("fixture"));
                    }
                }
                assert_eq!(overlay.browser.tab(), Tab::Mcp);
            }
        }
    }

    #[test]
    fn letter_keys_never_remove_without_confirmation() {
        for tab in Tab::ALL {
            let mut b = Browser::new();
            while b.tab() != tab {
                b.next_tab();
            }
            b.set_rows(tab, vec![row("pkg", Origin::Installed, State::Enabled)]);
            for c in 'a'..='z' {
                let Some(intent) = primary_intent(&b, c) else {
                    continue;
                };
                let remove = matches!(
                    intent,
                    Intent::RemoveMcp { .. }
                        | Intent::RemovePlugin { .. }
                        | Intent::RemoveSkill { .. }
                );
                let ran_now = b.request(intent).is_some();
                assert!(
                    !(remove && ran_now),
                    "key {c} on {tab:?} removed without confirmation"
                );
                b.decline();
            }
        }
    }

    #[test]
    fn remove_waits_for_enter_and_esc_cancels() {
        let mut b = Browser::new();
        b.set_rows(
            Tab::Mcp,
            vec![row("memory", Origin::Installed, State::Enabled)],
        );
        let intent = primary_intent(&b, 'x').expect("x removes the selected server");
        assert!(b.request(intent).is_none());
        assert!(b.confirm.is_some());
        b.decline();
        assert!(b.confirm.is_none());
    }

    #[test]
    fn enter_installs_available_rows_and_shows_details_for_installed_ones() {
        let mut b = Browser::new();
        b.set_rows(
            Tab::Plugins,
            vec![
                row("alpha@market", Origin::Remote, State::Available),
                row("beta", Origin::Installed, State::Enabled),
            ],
        );
        b.next_tab();
        assert_eq!(
            enter_install(&b),
            Some(Intent::InstallPlugin {
                id: "alpha@market".into()
            })
        );
        assert_eq!(enter_label(&b), "install");
        b.move_cursor(1);
        assert!(enter_install(&b).is_none());
        assert_eq!(enter_label(&b), "details");
    }
}
