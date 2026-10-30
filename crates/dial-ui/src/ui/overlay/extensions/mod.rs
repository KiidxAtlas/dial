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
#[cfg(test)]
use model::JobProgress;
use model::{
    Browser, Form, FormKind, Intent, Job, Origin, Row, SharedCatalog, SharedProgress, State, Tab,
};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::{Duration, Instant};

pub struct ExtensionsOverlay {
    pub browser: Browser,
    /// Keep every admitted request subscribed until its result arrives, including when a
    /// later request is rejected while an earlier install is still running.
    progress: Vec<(SharedProgress, bool)>,
    /// Pending catalog fetches, started by the command controller.
    catalogs: Vec<(Tab, SharedCatalog)>,
    catalog_job: bool,
    /// Set when a finished install may have changed what is installed; the app refreshes rows.
    needs_refresh: bool,
    /// When the search text last changed and has not been sent to the MCP catalogs yet.
    query_edited: Option<Instant>,
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
            progress: Vec::new(),
            catalogs: Vec::new(),
            catalog_job: false,
            needs_refresh: false,
            query_edited: None,
        }
    }

    /// Pause after the last keystroke before the MCP catalogs are searched while typing.
    pub const SEARCH_PAUSE: Duration = Duration::from_millis(500);

    /// Follow a background catalog fetch for `tab` and `query`, replacing any still pending for it.
    pub fn attach_catalog(&mut self, tab: Tab, query: String, listing: SharedCatalog) {
        self.catalogs.retain(|(pending, _)| *pending != tab);
        self.browser.begin_catalog(tab, query, 1);
        self.catalogs.push((tab, listing));
    }

    /// The MCP query to search the catalogs for once typing has paused for [`Self::SEARCH_PAUSE`],
    /// so a server missing from the loaded rows is found without pressing Enter. Waits while a
    /// listing is still loading, so the newest text is searched once it lands.
    pub fn catalog_search_due(&mut self, now: Instant) -> Option<String> {
        let edited = self.query_edited?;
        if now.saturating_duration_since(edited) < Self::SEARCH_PAUSE
            || self.browser.catalog(Tab::Mcp).loading
        {
            return None;
        }
        self.query_edited = None;
        self.browser
            .catalog_search_pending()
            .then(|| self.browser.query().trim().to_owned())
    }

    fn apply_catalogs(&mut self) {
        let mut ready = Vec::new();
        self.catalogs.retain(|(_, shared)| {
            if let Ok(mut slot) = shared.lock()
                && let Some(update) = slot.take()
            {
                ready.push(update);
                return false;
            }
            true
        });
        for update in ready {
            self.browser.apply_catalog(update);
        }
    }

    /// Follow a request without losing an earlier worker's form or completion.
    pub fn attach(&mut self, progress: SharedProgress) {
        self.progress
            .push((progress, std::mem::take(&mut self.catalog_job)));
    }

    /// Mirror the worker's progress into the view. Called once per UI tick.
    pub fn tick(&mut self) {
        self.apply_catalogs();
        for (shared, mut catalog_job) in std::mem::take(&mut self.progress) {
            let snapshot = match shared.lock() {
                Ok(mut guard) => {
                    let snapshot = guard.clone();
                    guard.form = None;
                    guard.catalog = None;
                    Some(snapshot)
                }
                Err(_) => None,
            };
            let Some(snapshot) = snapshot else {
                self.progress.push((shared, catalog_job));
                continue;
            };
            if let Some(form) = snapshot.form {
                self.browser.form = Some(form);
            }
            if let Some(update) = snapshot.catalog {
                catalog_job = true;
                self.browser.apply_catalog(update);
            }
            match (snapshot.label, snapshot.result) {
                (label, Some((ok, message))) => {
                    self.browser
                        .start_job(label.unwrap_or_else(|| "Extensions".into()));
                    self.browser.finish_job(ok, message);
                    self.needs_refresh |= !catalog_job;
                }
                (label, None) => {
                    if let Some(label) = label {
                        self.browser.job = Job::Running {
                            label,
                            lines: snapshot.lines,
                        };
                    }
                    self.progress.push((shared, catalog_job));
                }
            }
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
        if let Event::Paste(text) = event {
            // Single-line fields accept literal text only; pasted control characters never
            // navigate, submit, or become terminal control sequences.
            if let Some(form) = self.browser.form.as_mut() {
                for c in text.chars().filter(|c| !c.is_control()) {
                    form.push(c);
                }
            } else if self.browser.confirm.is_none() && self.browser.searching() {
                for c in text.chars().filter(|c| !c.is_control()) {
                    self.browser.push_query(c);
                }
                self.query_edited = Some(Instant::now());
            }
            return OverlayEvent::None;
        }
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

        // Navigation leaves the text field but retains its filter. Printable bindings
        // (such as vim-style j/k) remain search text rather than triggering actions.
        if b.searching()
            && typed_char(key).is_none()
            && (keys.matches(Action::SelectUp, key)
                || keys.matches(Action::SelectDown, key)
                || keys.matches(Action::SelectPageUp, key)
                || keys.matches(Action::SelectPageDown, key)
                || keys.matches(Action::TabNext, key)
                || keys.matches(Action::TabPrev, key)
                || matches!(
                    key.code,
                    crossterm::event::KeyCode::Home | crossterm::event::KeyCode::End
                ))
        {
            b.end_search();
        }

        if b.searching() {
            if keys.matches(Action::SelectCancel, key) {
                b.clear_search();
            } else if keys.matches(Action::SelectConfirm, key) {
                b.end_search();
                let tab = b.tab();
                let catalog = b.catalog(tab);
                if !matches!(tab, Tab::Plugins | Tab::Marketplaces)
                    && !catalog.loading
                    && (catalog.page == 0 || catalog.query.trim() != b.query().trim())
                {
                    let query = b.query().to_owned();
                    return self.browse(tab, query, 1);
                }
            } else if key.code == crossterm::event::KeyCode::Backspace {
                b.pop_query();
                self.query_edited = Some(Instant::now());
            } else if let Some(c) = typed_char(key) {
                b.push_query(c);
                self.query_edited = Some(Instant::now());
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
            return self.load_more_near_end();
        }
        if keys.matches(Action::SelectPageUp, key) {
            b.move_cursor(-8);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectPageDown, key) {
            b.move_cursor(8);
            return self.load_more_near_end();
        }
        if key.code == crossterm::event::KeyCode::Home {
            b.move_cursor(isize::MIN / 2);
            return OverlayEvent::None;
        }
        if key.code == crossterm::event::KeyCode::End {
            b.move_cursor(isize::MAX / 2);
            return self.load_more_near_end();
        }
        if keys.matches(Action::SelectConfirm, key) {
            if let Some(intent) = enter_install(b) {
                return match b.request(intent) {
                    Some(now) => action(&now),
                    None => OverlayEvent::None,
                };
            }
            if b.catalog_search_pending() && b.visible(Tab::Mcp).is_empty() {
                let query = b.query().to_owned();
                return self.browse(Tab::Mcp, query, 1);
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
                if typed_char(key) == Some('n') {
                    return self.load_next_page();
                }
                let query = b.query().to_owned();
                self.browse(tab, query, 1)
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

    /// Rows from the end of the list at which the next catalog page is requested.
    const PREFETCH: usize = 5;

    /// Infinite scroll: when the cursor nears the last row of a catalog tab that has more pages,
    /// request the next one. Does nothing while a page or an install is already running.
    fn load_more_near_end(&mut self) -> OverlayEvent {
        let b = &self.browser;
        let tab = b.tab();
        if matches!(tab, Tab::Plugins | Tab::Marketplaces)
            || matches!(b.job, Job::Running { .. })
            || b.cursor() + Self::PREFETCH < b.visible(tab).len()
        {
            return OverlayEvent::None;
        }
        self.load_next_page()
    }

    /// Request the page after the last one loaded for the current tab, if there is one.
    fn load_next_page(&mut self) -> OverlayEvent {
        let b = &mut self.browser;
        let tab = b.tab();
        let state = b.catalog(tab);
        if state.loading || !state.more || state.query.trim() != b.query().trim() {
            return OverlayEvent::None;
        }
        let (query, page) = (state.query.clone(), state.page + 1);
        self.browse(tab, query, page)
    }

    /// Request one catalog page for `tab`, replacing any fetch still pending for it.
    fn browse(&mut self, tab: Tab, query: String, page: usize) -> OverlayEvent {
        self.catalogs.retain(|(pending, _)| *pending != tab);
        self.catalog_job = true;
        self.query_edited = None;
        action(&self.browser.begin_catalog(tab, query, page))
    }

    pub fn banner(&mut self, message: impl Into<String>, is_error: bool) {
        let message = message.into();
        if is_error {
            self.browser.job = Job::Done {
                label: "Extensions".into(),
                ok: false,
                message,
            };
        } else {
            // A later neutral notice supersedes an earlier failure; a running install stays.
            if matches!(self.browser.job, Job::Done { ok: false, .. }) {
                self.browser.job = Job::Idle;
            }
            self.browser.message = message;
        }
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
                let mut search = format!("/ {}_", browser.query());
                let summary = browser.match_summary();
                if !summary.is_empty() {
                    search.push_str(&format!("  · {summary}"));
                }
                if browser.catalog_search_pending() {
                    search.push_str("  · searching every MCP catalog when you pause, or Enter now");
                }
                search
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
            let shown = browser.visible(browser.tab()).len();
            let mut status = Vec::new();
            if catalog.loading {
                status.push(format!("Loading {}…", browser.tab().label()));
            } else if !catalog.message.trim().is_empty() {
                status.push(crate::ui::text::one_line(&catalog.message));
            }
            if shown > 0 {
                status.push(format!("{shown} shown"));
            }
            lines.push(Line::from(Span::styled(
                format!(" {}", status.join(" · ")),
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
                    let empty = if catalog.loading {
                        "Loading the catalog…".to_owned()
                    } else if browser.catalog_search_pending() {
                        format!(
                            "Nothing loaded matches \"{}\". The official registry, Smithery, npm and GitHub are searched when you pause typing, or press Enter.",
                            browser.query().trim()
                        )
                    } else if !browser.query().is_empty() {
                        "No matches on this tab. Left/Right checks the other tabs; Esc clears the search.".to_owned()
                    } else if browser.tab() == Tab::Marketplaces {
                        "No marketplaces yet. Press a to add a plugin index, or r to refresh."
                            .to_owned()
                    } else {
                        format!(
                            "Nothing listed under {} yet. Press r to refresh, or / to search the catalog.",
                            browser.tab().label()
                        )
                    };
                    // Wrapped so the way forward is never clipped at narrow widths.
                    for row in crate::ui::text::wrap_words(&empty, w.saturating_sub(2).max(1))
                        .into_iter()
                        .take(body_h.max(1))
                    {
                        lines.push(Line::from(Span::styled(
                            format!(" {row}"),
                            ctx.theme.fg(Tok::Muted),
                        )));
                    }
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
            } else if browser.searching() {
                vec![
                    (
                        pair_label(ctx.keys, Action::SelectUp, Action::SelectDown),
                        "browse results".to_owned(),
                    ),
                    (
                        ctx.keys.label(Action::SelectConfirm),
                        "finish search".to_owned(),
                    ),
                    (
                        ctx.keys.label(Action::SelectCancel),
                        "clear search".to_owned(),
                    ),
                ]
            } else {
                let esc = if browser.details_open {
                    "back"
                } else if !browser.query().is_empty() {
                    "clear search"
                } else {
                    "close"
                };
                // Most useful first; the footer drops hints before the last (the way out) when
                // space runs short. Row actions appear only when they apply to the selection.
                let mut items = Vec::new();
                items.push((
                    pair_label(ctx.keys, Action::SelectUp, Action::SelectDown),
                    "select".to_owned(),
                ));
                if browser.selected().is_some() {
                    items.push((ctx.keys.label(Action::SelectConfirm), enter_label(browser)));
                }
                for (key, label) in [('t', "test"), ('e', "enable"), ('x', "remove")] {
                    if let Some(intent) = primary_intent(browser, key) {
                        let label = match (&intent, label) {
                            (Intent::ToggleMcp { enable: false, .. }, _)
                            | (Intent::ToggleMarketplace { enable: false, .. }, _) => "disable",
                            (_, label) => label,
                        };
                        items.push((key.to_string(), label.to_owned()));
                    }
                }
                if browser.tab() == Tab::Marketplaces {
                    items.push(("a".to_owned(), "add".to_owned()));
                }
                items.push((
                    pair_label(ctx.keys, Action::TabPrev, Action::TabNext),
                    "tabs".to_owned(),
                ));
                items.push(("/".to_owned(), "search".to_owned()));
                if browser.selected().is_some() && enter_install(browser).is_some() {
                    items.push(("v".to_owned(), "details".to_owned()));
                }
                items.push(("r".to_owned(), "refresh".to_owned()));
                if catalog.more {
                    items.push(("↓".to_owned(), "loads more".to_owned()));
                }
                items.push((ctx.keys.label(Action::SelectCancel), esc.to_owned()));
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
        (Tab::Mcp, 'i', false) if row.origin == Origin::Foreign => {
            Some(Intent::ImportMcp { id: row.id.clone() })
        }
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
    ];
    for tag in &row.tags {
        spans.push(Span::styled(format!(" #{tag}"), theme.fg(Tok::Dim)));
    }
    spans.push(Span::styled(
        format!("  {}", crate::ui::text::one_line(&row.summary)),
        theme.fg(Tok::Muted),
    ));
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
mod tests;
