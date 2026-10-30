//! The interactive side pane `dial watch` opens in a terminal: one full-screen view with tabs for
//! the live feed, the plan, jobs, workers, side threads and peers.
//!
//! The pane renders what the session publishes (`state.json`, see [`super::state`]) plus the
//! append-only feed, and never reads session internals. Its only effects are requests in the
//! session's private inbox: stop a job, a worker or a side answer, or message a peer. Every stop
//! asks for confirmation first. Approvals and questions stay in the main window.
use super::state::{self, Job, JobKind, Request, Snapshot};
use super::{Event, load_image, next_event};
use crate::agent::workflow::todo::Status;
use crate::config::paths::Layout;
use crate::ui::images::Thumbnail;
use crate::ui::text;
use crate::ui::theme::{Theme, Tok};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::io::BufRead;
use std::path::Path;
use std::time::{Duration, Instant};

/// Feed entries kept in memory; older ones scroll away.
const FEED_LIMIT: usize = 500;
/// A snapshot older than this means the session stopped publishing (closed, or busy in a call
/// that does not repaint).
const STALE_MS: u64 = 5_000;
const STATE_EVERY: Duration = Duration::from_millis(250);
const MESSAGE_LIMIT: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tab {
    Feed,
    Plan,
    Jobs,
    Workers,
    Threads,
    Peers,
}

const TABS: [Tab; 6] = [
    Tab::Feed,
    Tab::Plan,
    Tab::Jobs,
    Tab::Workers,
    Tab::Threads,
    Tab::Peers,
];

impl Tab {
    fn index(self) -> usize {
        TABS.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            Self::Feed => "Feed",
            Self::Plan => "Plan",
            Self::Jobs => "Jobs",
            Self::Workers => "Workers",
            Self::Threads => "Threads",
            Self::Peers => "Peers",
        }
    }
}

enum FeedItem {
    Text {
        title: String,
        body: String,
    },
    Image {
        label: String,
        image: Result<Thumbnail, String>,
    },
}

enum Mode {
    Browse,
    Confirm { request: Request, prompt: String },
    Compose { to: String, text: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyOutcome {
    Continue,
    Quit,
    Send(Request),
}

pub(super) struct Pane {
    tab: Tab,
    selected: [usize; 6],
    /// Detail lines scrolled past, per tab.
    scroll: [usize; 6],
    /// Feed lines scrolled up from the newest; 0 follows new output.
    feed_back: usize,
    feed: Vec<FeedItem>,
    state: Option<Snapshot>,
    error: Option<String>,
    status: Option<String>,
    mode: Mode,
    /// Body height at the last render, for page keys.
    page: usize,
}

impl Pane {
    pub(super) fn new() -> Self {
        Self {
            tab: Tab::Feed,
            selected: [0; 6],
            scroll: [0; 6],
            feed_back: 0,
            feed: Vec::new(),
            state: None,
            error: None,
            status: None,
            mode: Mode::Browse,
            page: 10,
        }
    }

    pub(super) fn push_feed(&mut self, dir: &Path, event: Event) {
        let item = match event {
            Event::Text { title, body } => FeedItem::Text {
                title: text::one_line(&title),
                body: text::sanitize(&body),
            },
            Event::Image { label, file } => FeedItem::Image {
                image: load_image(dir, &file, &label, false).map_err(|error| format!("{error:#}")),
                label: text::one_line(&label),
            },
        };
        self.feed.push(item);
        if self.feed.len() > FEED_LIMIT {
            self.feed.drain(..self.feed.len() - FEED_LIMIT);
        }
    }

    pub(super) fn set_state(&mut self, result: Result<Option<Snapshot>>) {
        match result {
            Ok(state) => {
                self.state = state;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        for tab in TABS {
            let count = self.count(tab);
            let selected = &mut self.selected[tab.index()];
            *selected = (*selected).min(count.saturating_sub(1));
        }
    }

    pub(super) fn paste(&mut self, pasted: &str) {
        if let Mode::Compose { text: draft, .. } = &mut self.mode {
            for c in pasted.chars().filter(|c| !c.is_control()) {
                if draft.len() + c.len_utf8() > MESSAGE_LIMIT {
                    break;
                }
                draft.push(c);
            }
        }
    }

    fn count(&self, tab: Tab) -> usize {
        let Some(state) = &self.state else {
            return if tab == Tab::Feed { self.feed.len() } else { 0 };
        };
        match tab {
            Tab::Feed => self.feed.len(),
            Tab::Plan => state.plan.len(),
            Tab::Jobs => state.jobs.len(),
            Tab::Workers => state.workers.len(),
            Tab::Threads => state.threads.len(),
            Tab::Peers => state.peers.rows.len(),
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> KeyOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return KeyOutcome::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Browse => {}
            Mode::Confirm { request, prompt } => {
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.status = Some(format!("Requested: {prompt}"));
                        KeyOutcome::Send(request)
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        self.status = Some("Nothing was stopped.".into());
                        KeyOutcome::Continue
                    }
                    _ => {
                        self.mode = Mode::Confirm { request, prompt };
                        KeyOutcome::Continue
                    }
                };
            }
            Mode::Compose { to, mut text } => {
                match key.code {
                    KeyCode::Esc => self.status = Some("Message discarded.".into()),
                    KeyCode::Enter if !text.trim().is_empty() => {
                        self.status = Some(format!("Message queued for {}.", short(&to)));
                        return KeyOutcome::Send(Request::MessagePeer { to, text });
                    }
                    KeyCode::Backspace => {
                        text.pop();
                        self.mode = Mode::Compose { to, text };
                    }
                    KeyCode::Char(c) if !ctrl => {
                        if text.len() + c.len_utf8() <= MESSAGE_LIMIT {
                            text.push(c);
                        }
                        self.mode = Mode::Compose { to, text };
                    }
                    _ => self.mode = Mode::Compose { to, text },
                }
                return KeyOutcome::Continue;
            }
        }
        self.status = None;
        match key.code {
            KeyCode::Char('q') => return KeyOutcome::Quit,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.switch(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.switch(TABS.len() - 1),
            KeyCode::Char(c @ '1'..='6') => self.tab = TABS[c as usize - '1' as usize],
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::PageUp => self.page_scroll(false),
            KeyCode::PageDown => self.page_scroll(true),
            KeyCode::Home | KeyCode::Char('g') => self.ends(false),
            KeyCode::End | KeyCode::Char('G') => self.ends(true),
            KeyCode::Char('x') => self.confirm_stop(),
            KeyCode::Char('m') if self.tab == Tab::Peers => self.compose(),
            _ => {}
        }
        KeyOutcome::Continue
    }

    fn switch(&mut self, by: usize) {
        self.tab = TABS[(self.tab.index() + by) % TABS.len()];
    }

    fn step(&mut self, down: bool) {
        let i = self.tab.index();
        if self.tab == Tab::Feed {
            self.feed_back = if down {
                self.feed_back.saturating_sub(1)
            } else {
                self.feed_back + 1
            };
            return;
        }
        let count = self.count(self.tab);
        let next = if down {
            (self.selected[i] + 1).min(count.saturating_sub(1))
        } else {
            self.selected[i].saturating_sub(1)
        };
        if next != self.selected[i] {
            self.selected[i] = next;
            self.scroll[i] = 0;
        }
    }

    fn page_scroll(&mut self, down: bool) {
        let page = self.page.max(2) / 2;
        if self.tab == Tab::Feed {
            self.feed_back = if down {
                self.feed_back.saturating_sub(page)
            } else {
                self.feed_back + page
            };
            return;
        }
        let scroll = &mut self.scroll[self.tab.index()];
        *scroll = if down {
            *scroll + page
        } else {
            scroll.saturating_sub(page)
        };
    }

    fn ends(&mut self, end: bool) {
        let i = self.tab.index();
        if self.tab == Tab::Feed {
            self.feed_back = if end { 0 } else { usize::MAX / 2 };
            return;
        }
        self.selected[i] = if end {
            self.count(self.tab).saturating_sub(1)
        } else {
            0
        };
        self.scroll[i] = 0;
    }

    fn selected(&self) -> usize {
        self.selected[self.tab.index()]
    }

    fn confirm_stop(&mut self) {
        let Some(state) = &self.state else {
            self.status = Some("Waiting for the session.".into());
            return;
        };
        let selected = self.selected();
        let found = match self.tab {
            Tab::Jobs => state
                .jobs
                .get(selected)
                .filter(|job| job.running)
                .map(|job| {
                    let request = match job.kind {
                        JobKind::Process => Request::StopProcess {
                            name: job.name.clone(),
                        },
                        JobKind::Command => Request::CancelCommand {
                            label: job.name.clone(),
                        },
                    };
                    (request, format!("stop {}", job.name))
                }),
            Tab::Workers => state
                .workers
                .get(selected)
                .filter(|worker| worker.cancellable)
                .map(|worker| {
                    (
                        Request::CancelWorker {
                            id: worker.id.clone(),
                        },
                        format!("cancel worker {}", worker_name(worker)),
                    )
                }),
            Tab::Threads => state
                .threads
                .iter()
                .any(|thread| thread.streaming.is_some())
                .then(|| {
                    (
                        Request::CancelSideAnswer,
                        "stop the streaming side answer".to_owned(),
                    )
                }),
            _ => None,
        };
        match found {
            Some((request, prompt)) => self.mode = Mode::Confirm { request, prompt },
            None => self.status = Some("Nothing running to stop here.".into()),
        }
    }

    fn compose(&mut self) {
        match self
            .state
            .as_ref()
            .and_then(|state| state.peers.rows.get(self.selected()))
        {
            Some(peer) => {
                self.mode = Mode::Compose {
                    to: peer.id.clone(),
                    text: String::new(),
                }
            }
            None => self.status = Some("No peer selected.".into()),
        }
    }

    pub(super) fn render(&mut self, frame: &mut Frame, theme: &Theme, now_ms: u64) {
        let area = frame.area();
        if area.height < 3 || area.width < 12 {
            return;
        }
        let header = Rect { height: 1, ..area };
        let footer = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };
        let body = Rect {
            y: area.y + 1,
            height: area.height - 2,
            ..area
        };
        let (tabs, fresh) = self.header(theme, now_ms);
        // Freshness is the one warning the user must always see, so it keeps
        // its own right-hand slot and the tab strip is clipped instead.
        let fresh_width = u16::try_from(fresh.width())
            .unwrap_or(u16::MAX)
            .min(header.width);
        let tabs_area = Rect {
            width: header.width - fresh_width,
            ..header
        };
        let fresh_area = Rect {
            x: header.x + tabs_area.width,
            width: fresh_width,
            ..header
        };
        frame.render_widget(Paragraph::new(tabs), tabs_area);
        frame.render_widget(Paragraph::new(Line::from(fresh)), fresh_area);
        frame.render_widget(Paragraph::new(self.footer(theme)), footer);
        self.page = body.height as usize;
        let lines = if self.tab == Tab::Feed {
            self.feed_view(body.width as usize, body.height as usize, theme)
        } else {
            self.split_view(body.width as usize, body.height as usize, theme)
        };
        frame.render_widget(Paragraph::new(lines), body);
    }

    fn header(&self, theme: &Theme, now_ms: u64) -> (Line<'static>, Span<'static>) {
        let mut spans = Vec::new();
        for (n, tab) in TABS.iter().enumerate() {
            let label = format!(" {} {}{} ", n + 1, tab.title(), self.badge(*tab));
            let style = if *tab == self.tab {
                theme
                    .fg(Tok::Accent)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                theme.fg(Tok::Muted)
            };
            spans.push(Span::styled(label, style));
        }
        let (fresh, tok) = self.freshness(now_ms);
        (
            Line::from(spans),
            Span::styled(format!(" {fresh}"), theme.fg(tok)),
        )
    }

    fn badge(&self, tab: Tab) -> String {
        let Some(state) = &self.state else {
            return String::new();
        };
        let n = match tab {
            Tab::Feed => return String::new(),
            Tab::Plan => {
                let done = state
                    .plan
                    .iter()
                    .filter(|task| task.status == Status::Completed)
                    .count();
                return if state.plan.is_empty() {
                    String::new()
                } else {
                    format!(" {done}/{}", state.plan.len())
                };
            }
            Tab::Jobs => state.jobs.iter().filter(|job| job.running).count(),
            Tab::Workers => state
                .workers
                .iter()
                .filter(|worker| worker.status == "running" || worker.status == "waiting")
                .count(),
            Tab::Threads => state
                .threads
                .iter()
                .filter(|thread| thread.streaming.is_some())
                .count(),
            Tab::Peers => state.peers.rows.len(),
        };
        if n == 0 {
            String::new()
        } else {
            format!(" {n}")
        }
    }

    fn freshness(&self, now_ms: u64) -> (String, Tok) {
        if let Some(error) = &self.error {
            return (text::clip(error, 80), Tok::Error);
        }
        let Some(state) = &self.state else {
            return ("waiting for the session".into(), Tok::Muted);
        };
        let age = now_ms.saturating_sub(state.updated_ms);
        if age > STALE_MS {
            (format!("no update for {}s", age / 1000), Tok::Warning)
        } else if state.busy {
            ("turn running".into(), Tok::Accent)
        } else {
            ("idle".into(), Tok::Muted)
        }
    }

    fn footer(&self, theme: &Theme) -> Line<'static> {
        match &self.mode {
            Mode::Confirm { prompt, .. } => Line::from(vec![
                Span::styled(format!("{}? ", capitalize(prompt)), theme.fg(Tok::Warning)),
                Span::styled("y confirm · n keep it running", theme.fg(Tok::Muted)),
            ]),
            Mode::Compose { to, text } => Line::from(vec![
                Span::styled(format!("To {}: ", short(to)), theme.fg(Tok::Accent)),
                Span::styled(format!("{text}▏"), theme.fg(Tok::Text)),
                Span::styled("  Enter send · Esc discard", theme.fg(Tok::Muted)),
            ]),
            Mode::Browse => match &self.status {
                Some(status) => Line::styled(status.clone(), theme.fg(Tok::Accent)),
                None => {
                    let action = match self.tab {
                        Tab::Jobs | Tab::Workers | Tab::Threads => " · x stop",
                        Tab::Peers => " · m message",
                        Tab::Feed | Tab::Plan => "",
                    };
                    let outcome = self
                        .state
                        .as_ref()
                        .and_then(|state| state.outcomes.last())
                        .map(|last| format!(" · {last}"))
                        .unwrap_or_default();
                    Line::styled(
                        format!(
                            "←→ tabs · ↑↓ select · PgUp/PgDn scroll{action} · q close{outcome}"
                        ),
                        theme.fg(Tok::Muted),
                    )
                }
            },
        }
    }

    fn feed_view(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.feed.is_empty() {
            lines.push(Line::styled(
                "Edits, reviews, screenshots and turn results appear here.",
                theme.fg(Tok::Muted),
            ));
        }
        for item in &self.feed {
            match item {
                FeedItem::Text { title, body } => {
                    lines.push(Line::styled(
                        title.clone(),
                        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    ));
                    for line in body.lines() {
                        let tok = if line.starts_with('+') {
                            Tok::Success
                        } else if line.starts_with('-') {
                            Tok::Error
                        } else {
                            Tok::Text
                        };
                        lines.push(Line::styled(line.to_owned(), theme.fg(tok)));
                    }
                }
                FeedItem::Image { label, image } => {
                    lines.push(Line::styled(
                        label.clone(),
                        theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    ));
                    match image {
                        Ok(thumbnail) => lines.extend(thumbnail.rows(width as u16, theme)),
                        Err(error) => lines.push(Line::styled(
                            format!("image unavailable: {error}"),
                            theme.fg(Tok::Error),
                        )),
                    }
                }
            }
            lines.push(Line::default());
        }
        let lines = text::wrap(lines, width.max(1));
        let end = lines
            .len()
            .saturating_sub(self.feed_back.min(lines.len().saturating_sub(height)));
        let start = end.saturating_sub(height);
        lines[start..end].to_vec()
    }

    fn split_view(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let items = self.items(theme);
        let mut lines = Vec::new();
        if items.is_empty() {
            lines.push(Line::styled(self.empty_text(), theme.fg(Tok::Muted)));
        }
        let list_height = items.len().clamp(1, (height * 2 / 5).max(3)).min(height);
        let selected = self.selected();
        let first = (selected + 1).saturating_sub(list_height);
        for (n, item) in items.into_iter().enumerate().skip(first).take(list_height) {
            let clipped = clip_line(item, width);
            lines.push(if n == selected {
                clipped.patch_style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                clipped
            });
        }
        lines.truncate(list_height);
        if lines.len() + 1 >= height {
            return lines;
        }
        lines.push(Line::styled("─".repeat(width), theme.fg(Tok::Border)));
        let detail = text::wrap(self.detail(theme), width.max(1));
        let room = height - lines.len();
        let scroll = self.scroll[self.tab.index()].min(detail.len().saturating_sub(room));
        lines.extend(detail.into_iter().skip(scroll).take(room));
        lines
    }

    fn empty_text(&self) -> String {
        if self.state.is_none() {
            return match &self.error {
                Some(error) => format!("Session state unavailable: {error}"),
                None => "Waiting for the session to publish its state…".into(),
            };
        }
        match self.tab {
            Tab::Feed => String::new(),
            Tab::Plan => "No plan yet. The agent's task list appears here.".into(),
            Tab::Jobs => {
                "No jobs. Background processes and commands such as /update appear here.".into()
            }
            Tab::Workers => "No workers in this session.".into(),
            Tab::Threads => "No side threads. /btw <question> starts one.".into(),
            Tab::Peers => self
                .state
                .as_ref()
                .and_then(|state| state.peers.error.clone())
                .unwrap_or_else(|| "No other Dial sessions in this checkout.".into()),
        }
    }

    fn items(&self, theme: &Theme) -> Vec<Line<'static>> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        match self.tab {
            Tab::Feed => Vec::new(),
            Tab::Plan => state
                .plan
                .iter()
                .map(|task| {
                    let (mark, tok) = status_mark(task.status);
                    Line::from(vec![
                        Span::styled(format!("{mark} "), theme.fg(tok)),
                        Span::styled(task.content.clone(), theme.fg(Tok::Text)),
                    ])
                })
                .collect(),
            Tab::Jobs => state
                .jobs
                .iter()
                .map(|job| {
                    Line::from(vec![
                        Span::styled(
                            if job.running { "● " } else { "○ " },
                            theme.fg(if job.running { Tok::Accent } else { Tok::Dim }),
                        ),
                        Span::styled(job.name.clone(), theme.fg(Tok::Text)),
                        Span::styled(
                            format!("  {} · {}", job_kind(job), job.status),
                            theme.fg(Tok::Muted),
                        ),
                    ])
                })
                .collect(),
            Tab::Workers => state
                .workers
                .iter()
                .map(|worker| {
                    Line::from(vec![
                        Span::styled(
                            format!("{:<9} ", worker.status),
                            theme.fg(worker_tok(&worker.status)),
                        ),
                        Span::styled(worker_name(worker).to_owned(), theme.fg(Tok::Text)),
                        Span::styled(
                            format!(
                                "  {}",
                                text::one_line(worker.current.as_deref().unwrap_or(&worker.task))
                            ),
                            theme.fg(Tok::Muted),
                        ),
                    ])
                })
                .collect(),
            Tab::Threads => state
                .threads
                .iter()
                .map(|thread| {
                    let live = if thread.streaming.is_some() {
                        "  streaming"
                    } else {
                        ""
                    };
                    Line::from(vec![
                        Span::styled(format!("#{} ", thread.id), theme.fg(Tok::Dim)),
                        Span::styled(text::one_line(&thread.title), theme.fg(Tok::Text)),
                        Span::styled(
                            format!("  {} exchange(s){live}", thread.exchanges.len()),
                            theme.fg(Tok::Muted),
                        ),
                    ])
                })
                .collect(),
            Tab::Peers => state
                .peers
                .rows
                .iter()
                .map(|peer| {
                    let overlap = if peer.overlapping.is_empty() {
                        ""
                    } else {
                        "  overlaps"
                    };
                    Line::from(vec![
                        Span::styled(format!("{} ", short(&peer.id)), theme.fg(Tok::Dim)),
                        Span::styled(format!("{:<8} ", peer.phase), theme.fg(Tok::Muted)),
                        Span::styled(text::one_line(&peer.summary), theme.fg(Tok::Text)),
                        Span::styled(overlap, theme.fg(Tok::Warning)),
                    ])
                })
                .collect(),
        }
    }

    fn detail(&self, theme: &Theme) -> Vec<Line<'static>> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        let selected = self.selected();
        let mut out = Detail {
            lines: Vec::new(),
            theme,
        };
        match self.tab {
            Tab::Feed => {}
            Tab::Plan => {
                if let Some(task) = state.plan.get(selected) {
                    out.title(&task.content);
                    out.field("Status", status_label(task.status));
                    out.optional("Phase", task.phase.as_deref());
                    out.optional("Note", task.note.as_deref());
                    out.optional("Finding", task.finding.as_deref());
                    if !task.evidence.is_empty() {
                        out.heading("Evidence");
                        for item in &task.evidence {
                            out.text(&format!("· {item}"), Tok::Text);
                        }
                    }
                }
            }
            Tab::Jobs => {
                if let Some(job) = state.jobs.get(selected) {
                    out.title(&job.name);
                    out.field("Kind", job_kind(job));
                    out.field("Status", &job.status);
                    if !job.detail.is_empty() {
                        out.field("Command", &job.detail);
                    }
                    out.heading("Output");
                    if job.output.is_empty() {
                        out.text("(no output yet)", Tok::Dim);
                    }
                    for line in &job.output {
                        out.text(line, Tok::ToolOutput);
                    }
                }
            }
            Tab::Workers => {
                if let Some(worker) = state.workers.get(selected) {
                    out.title(worker_name(worker));
                    out.field("Agent", &format!("{} · {}", worker.agent, worker.model));
                    out.field("Status", &worker.status);
                    out.field("Task", &worker.task);
                    out.optional("Now", worker.current.as_deref());
                    let mut numbers = format!("{} step(s)", worker.steps);
                    if let Some(ms) = worker.elapsed_ms {
                        numbers.push_str(&format!(" · {}s", ms / 1000));
                    }
                    if let Some(cost) = worker.cost_usd {
                        numbers.push_str(&format!(" · ${cost:.2}"));
                    }
                    out.field("Usage", &numbers);
                    if !worker.activity.is_empty() {
                        out.heading("Recent activity");
                        for line in &worker.activity {
                            out.text(line, Tok::Text);
                        }
                    }
                }
            }
            Tab::Threads => {
                if let Some(thread) = state.threads.get(selected) {
                    out.title(&thread.title);
                    for exchange in thread.exchanges.iter().chain(&thread.streaming) {
                        out.text(&format!("Q: {}", exchange.question), Tok::Accent);
                        out.text(&exchange.answer, Tok::Text);
                        if exchange.outcome != "answered" {
                            out.text(&format!("({})", exchange.outcome), Tok::Dim);
                        }
                        out.lines.push(Line::default());
                    }
                }
            }
            Tab::Peers => {
                let peers = &state.peers;
                if let Some(peer) = peers.rows.get(selected) {
                    out.title(&peer.summary);
                    out.field("Peer", &peer.id);
                    out.field("Phase", &peer.phase);
                    out.list("Claims", &peer.scopes, Tok::Text);
                    out.list("Overlaps yours", &peer.overlapping, Tok::Warning);
                    out.lines.push(Line::default());
                }
                out.list("This session claims", &peers.scopes, Tok::Text);
                if peers.queued > 0 {
                    out.field("Unread peer mail", &peers.queued.to_string());
                }
            }
        }
        out.lines
    }
}

/// Builds wrapped-later detail lines with consistent styling.
struct Detail<'a> {
    lines: Vec<Line<'static>>,
    theme: &'a Theme,
}

impl Detail<'_> {
    fn title(&mut self, title: &str) {
        self.lines.push(Line::styled(
            text::one_line(title),
            self.theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        ));
    }

    fn heading(&mut self, heading: &str) {
        self.lines.push(Line::styled(
            heading.to_owned(),
            self.theme.fg(Tok::Muted).add_modifier(Modifier::BOLD),
        ));
    }

    fn field(&mut self, name: &str, value: &str) {
        self.lines.push(Line::from(vec![
            Span::styled(format!("{name}: "), self.theme.fg(Tok::Muted)),
            Span::styled(text::one_line(value), self.theme.fg(Tok::Text)),
        ]));
    }

    fn optional(&mut self, name: &str, value: Option<&str>) {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            self.field(name, value);
        }
    }

    fn list(&mut self, name: &str, values: &[String], tok: Tok) {
        if values.is_empty() {
            return;
        }
        self.heading(name);
        for value in values {
            self.text(&format!("· {value}"), tok);
        }
    }

    fn text(&mut self, value: &str, tok: Tok) {
        for line in text::sanitize(value).lines() {
            self.lines
                .push(Line::styled(line.to_owned(), self.theme.fg(tok)));
        }
    }
}

fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let room = width.saturating_sub(used);
        if room == 0 {
            break;
        }
        let content = text::clip(&span.content, room);
        used += text::width(&content);
        spans.push(Span::styled(content, span.style));
    }
    Line::from(spans)
}

fn status_mark(status: Status) -> (&'static str, Tok) {
    match status {
        Status::InProgress => ("▸", Tok::Accent),
        Status::Completed => ("✓", Tok::Success),
        Status::Pending => ("○", Tok::Muted),
        Status::Blocked => ("!", Tok::Warning),
        Status::Abandoned => ("–", Tok::Dim),
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::InProgress => "in progress",
        Status::Completed => "completed",
        Status::Pending => "pending",
        Status::Blocked => "blocked",
        Status::Abandoned => "dropped",
    }
}

fn job_kind(job: &Job) -> &'static str {
    match job.kind {
        JobKind::Process => "process",
        JobKind::Command => "command",
    }
}

fn worker_tok(status: &str) -> Tok {
    match status {
        "running" => Tok::Accent,
        "waiting" => Tok::Warning,
        "failed" => Tok::Error,
        "done" => Tok::Success,
        _ => Tok::Dim,
    }
}

fn worker_name(worker: &state::Worker) -> &str {
    if worker.name.trim().is_empty() {
        &worker.id
    } else {
        &worker.name
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Runs the pane until the user closes it. The terminal is restored on every exit path.
pub(super) fn run(
    layout: &Layout,
    session: &str,
    dir: &Path,
    reader: &mut impl BufRead,
) -> Result<i32> {
    use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event as Input};
    use crossterm::{cursor, execute, terminal};

    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                std::io::stdout(),
                DisableBracketedPaste,
                terminal::LeaveAlternateScreen,
                cursor::Show
            );
        }
    }

    terminal::enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )?;
    let mut screen =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let theme = Theme::dark();
    let mut pane = Pane::new();
    let mut pending = Vec::new();
    let mut read_at: Option<Instant> = None;
    loop {
        while let Some(event) = next_event(reader, &mut pending)? {
            pane.push_feed(dir, event);
        }
        if read_at.is_none_or(|at| at.elapsed() >= STATE_EVERY) {
            read_at = Some(Instant::now());
            pane.set_state(state::read(layout, session));
        }
        screen.draw(|frame| pane.render(frame, &theme, crate::ui::subagent_model::epoch_ms()))?;
        if !crossterm::event::poll(Duration::from_millis(150))? {
            continue;
        }
        match crossterm::event::read()? {
            Input::Key(key) if key.kind != KeyEventKind::Release => match pane.key(key) {
                KeyOutcome::Quit => return Ok(0),
                KeyOutcome::Send(request) => {
                    if let Err(error) = state::send(layout, session, &request) {
                        pane.status = Some(format!("Could not send the request: {error:#}"));
                    }
                }
                KeyOutcome::Continue => {}
            },
            Input::Paste(text) => pane.paste(&text),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
