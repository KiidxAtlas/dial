//! The interactive side pane `dial watch` opens in a terminal: one full-screen view with tabs for
//! the live feed, the plan, jobs, workers, side threads and chats. The Chats tab lists this
//! session's own conversation and one per project peer; Enter opens a text-message style thread
//! with an inline composer.
//!
//! The pane renders what the session publishes (`state.json`, see [`super::state`]) plus the
//! append-only feed, and never reads session internals. Its only effects are requests in the
//! session's private inbox: stop a job, a worker or a side answer, message a peer, or prompt this
//! session's model. Every stop asks for confirmation first. Approvals and questions stay in the
//! main window.
use super::state::{self, ChatRole, Job, JobKind, Request, Snapshot};
use super::{Event, load_image, next_event};
use crate::agent::workflow::todo::Status;
use crate::config::paths::Layout;
use crate::ui::chrome;
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

mod chat;
use chat::{Bubble, Side};

/// Feed entries kept in memory; older ones scroll away.
const FEED_LIMIT: usize = 500;
/// A snapshot older than this means the session stopped publishing (closed, or busy in a call
/// that does not repaint).
const STALE_MS: u64 = 5_000;
const STATE_EVERY: Duration = Duration::from_millis(250);
const MESSAGE_LIMIT: usize = 4000;
/// Messages shown as "Sending…" until the session publishes them.
const OUTBOX: usize = 16;
/// A sent message the session never published stops showing after this long.
const OUTBOX_MS: u64 = 120_000;
/// Rows the composer may grow to.
const COMPOSER_ROWS: usize = 3;

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
            Self::Peers => "Chats",
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
}

/// Whose conversation the Chats tab has open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// This session's own model.
    Session,
    /// A project peer, by instance id.
    Peer(String),
}

/// A message sent from this pane that the session has not published yet.
struct Pending {
    target: Target,
    text: String,
    at_ms: u64,
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
    /// The conversation open in the Chats tab, if any.
    chat: Option<Target>,
    /// The message being typed into the open conversation.
    draft: String,
    /// Sent from here, not yet published by the session.
    outbox: Vec<Pending>,
    /// Chat lines scrolled up from the newest; 0 follows new messages.
    chat_back: usize,
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
            chat: None,
            draft: String::new(),
            outbox: Vec::new(),
            chat_back: 0,
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
        let now = crate::ui::subagent_model::epoch_ms();
        let state = self.state.as_ref();
        self.outbox.retain(|pending| {
            now.saturating_sub(pending.at_ms) < OUTBOX_MS
                && !state.is_some_and(|state| published(state, pending))
        });
    }

    pub(super) fn paste(&mut self, pasted: &str) {
        if self.chat.is_none() {
            return;
        }
        for c in pasted.chars().filter(|c| *c == '\n' || !c.is_control()) {
            if self.draft.len() + c.len_utf8() > MESSAGE_LIMIT {
                break;
            }
            self.draft.push(c);
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
            Tab::Peers => state.peers.rows.len() + 1,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> KeyOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return KeyOutcome::Quit;
        }
        if let Mode::Confirm { request, prompt } = std::mem::replace(&mut self.mode, Mode::Browse) {
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
        if self.chat.is_some()
            && let Some(outcome) = self.chat_key(key)
        {
            return outcome;
        }
        self.status = None;
        match key.code {
            KeyCode::Char('q') => return KeyOutcome::Quit,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.switch(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.switch(TABS.len() - 1),
            KeyCode::Char(c @ '1'..='6') => {
                self.chat = None;
                self.tab = TABS[c as usize - '1' as usize];
            }
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
            KeyCode::PageUp => self.page_scroll(false),
            KeyCode::PageDown => self.page_scroll(true),
            KeyCode::Home | KeyCode::Char('g') => self.ends(false),
            KeyCode::End | KeyCode::Char('G') => self.ends(true),
            KeyCode::Char('x') => self.confirm_stop(),
            KeyCode::Enter | KeyCode::Char('m') if self.tab == Tab::Peers => self.open_chat(),
            _ => {}
        }
        KeyOutcome::Continue
    }

    /// Keys of the open conversation: typing goes to the composer, arrows scroll. `None` leaves
    /// the key to the tab-level handling (Tab switches tabs and closes the conversation).
    fn chat_key(&mut self, key: KeyEvent) -> Option<KeyOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.page.max(2) / 2;
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => return None,
            KeyCode::Esc if self.draft.is_empty() => {
                self.chat = None;
                self.status = None;
            }
            KeyCode::Esc => {
                self.draft.clear();
                self.status = Some("Draft cleared.".into());
            }
            KeyCode::Enter => return Some(self.send_draft()),
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Up => self.chat_back += 1,
            KeyCode::Down => self.chat_back = self.chat_back.saturating_sub(1),
            KeyCode::PageUp => self.chat_back += page,
            KeyCode::PageDown => self.chat_back = self.chat_back.saturating_sub(page),
            KeyCode::Home => self.chat_back = usize::MAX / 2,
            KeyCode::End => self.chat_back = 0,
            KeyCode::Char(c) if !ctrl && self.draft.len() + c.len_utf8() <= MESSAGE_LIMIT => {
                self.draft.push(c);
            }
            _ => {}
        }
        Some(KeyOutcome::Continue)
    }

    /// Send the draft to the open conversation; the message shows as "Sending…" until the
    /// session publishes it.
    fn send_draft(&mut self) -> KeyOutcome {
        let text = self.draft.trim().to_owned();
        let Some(target) = self.chat.clone() else {
            return KeyOutcome::Continue;
        };
        if text.is_empty() {
            return KeyOutcome::Continue;
        }
        let request = match &target {
            Target::Session => Request::Prompt { text: text.clone() },
            Target::Peer(id) => {
                if self.chat_peer().is_none() {
                    self.status = Some("That peer is no longer active; nothing was sent.".into());
                    return KeyOutcome::Continue;
                }
                Request::MessagePeer {
                    to: id.clone(),
                    text: text.clone(),
                }
            }
        };
        self.status = Some(match &target {
            Target::Session => "Sent to this session's model.".to_owned(),
            Target::Peer(_) => format!(
                "Sent to {}.",
                self.chat_peer().map(peer_name).unwrap_or_default()
            ),
        });
        if self.outbox.len() == OUTBOX {
            self.outbox.remove(0);
        }
        self.outbox.push(Pending {
            target,
            text,
            at_ms: crate::ui::subagent_model::epoch_ms(),
        });
        self.draft.clear();
        self.chat_back = 0;
        KeyOutcome::Send(request)
    }

    fn switch(&mut self, by: usize) {
        self.chat = None;
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

    /// The selected peer in the Chats list; row 0 is this session.
    fn selected_peer(&self) -> Option<&state::PeerRow> {
        let row = self.selected().checked_sub(1)?;
        self.state.as_ref()?.peers.rows.get(row)
    }

    /// The peer whose conversation is open, while it is still published.
    fn chat_peer(&self) -> Option<&state::PeerRow> {
        let Some(Target::Peer(id)) = &self.chat else {
            return None;
        };
        self.state
            .as_ref()?
            .peers
            .rows
            .iter()
            .find(|peer| &peer.id == id)
    }

    fn open_chat(&mut self) {
        if self.state.is_none() {
            self.status = Some("Waiting for the session.".into());
            return;
        }
        let target = match self.selected_peer() {
            Some(peer) => Target::Peer(peer.id.clone()),
            None => Target::Session,
        };
        if self.chat.as_ref() != Some(&target) {
            self.draft.clear();
        }
        self.chat = Some(target);
        self.chat_back = 0;
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
        frame.render_widget(
            Paragraph::new(self.footer(theme, usize::from(footer.width))),
            footer,
        );
        self.page = body.height as usize;
        let lines = if self.tab == Tab::Feed {
            self.feed_view(body.width as usize, body.height as usize, theme)
        } else if self.tab == Tab::Peers && self.chat.is_some() {
            self.chat_view(body.width as usize, body.height as usize, theme, now_ms)
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
            Tab::Peers => {
                let unread: usize = state
                    .peers
                    .rows
                    .iter()
                    .flat_map(|peer| &peer.messages)
                    .filter(|message| !message.outgoing && message.unread)
                    .count();
                return match (unread, state.peers.rows.len()) {
                    (0, 0) => String::new(),
                    (0, peers) => format!(" {peers}"),
                    (unread, _) => format!(" {unread} new"),
                };
            }
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

    fn footer(&self, theme: &Theme, width: usize) -> Line<'static> {
        match &self.mode {
            Mode::Confirm { prompt, .. } => footer_line(
                theme,
                Some(Span::styled(
                    format!("{}?", capitalize(prompt)),
                    theme.fg(Tok::Warning),
                )),
                &[("y", "confirm"), ("n", "keep it running")],
                width,
            ),
            Mode::Browse => {
                if self.tab == Tab::Peers && self.chat.is_some() {
                    let note = match &self.status {
                        Some(status) => Span::styled(status.clone(), theme.fg(Tok::Accent)),
                        None => Span::styled("Type a message", theme.fg(Tok::Muted)),
                    };
                    return footer_line(
                        theme,
                        Some(note),
                        &[
                            ("Enter", "send"),
                            ("↑↓ PgUp/PgDn", "scroll"),
                            ("Tab", "tabs"),
                            ("Esc", "back"),
                        ],
                        width,
                    );
                }
                let mut hints = vec![("←→", "tabs"), ("↑↓", "select"), ("PgUp/PgDn", "scroll")];
                match self.tab {
                    Tab::Jobs | Tab::Workers | Tab::Threads => hints.push(("x", "stop")),
                    Tab::Peers => hints.push(("Enter", "open chat")),
                    Tab::Feed | Tab::Plan => {}
                }
                hints.push(("q", "close"));
                // The newest action outcome (a status just set, else the session's last
                // recorded one) leads the row; the keys follow and the way out always stays.
                let note = match &self.status {
                    Some(status) => Some(Span::styled(status.clone(), theme.fg(Tok::Accent))),
                    None => self
                        .state
                        .as_ref()
                        .and_then(|state| state.outcomes.last())
                        .map(|last| Span::styled(last.clone(), theme.fg(Tok::Muted))),
                };
                footer_line(theme, note, &hints, width)
            }
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

    /// The open conversation: a pinned title, message bubbles with the newest at the bottom, a
    /// working indicator and the composer.
    fn chat_view(
        &self,
        width: usize,
        height: usize,
        theme: &Theme,
        now_ms: u64,
    ) -> Vec<Line<'static>> {
        let Some(state) = &self.state else {
            return vec![Line::styled(self.empty_text(), theme.fg(Tok::Muted))];
        };
        let own = if state.model.trim().is_empty() {
            "This session".to_owned()
        } else {
            state.model.clone()
        };
        let target = self.chat.clone().unwrap_or(Target::Session);
        let (title, working, mut bubbles) = match &target {
            Target::Session => (
                vec![
                    Span::styled(
                        "● ",
                        theme.fg(if state.busy { Tok::Accent } else { Tok::Muted }),
                    ),
                    Span::styled(
                        own.clone(),
                        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" · this session", theme.fg(Tok::Dim)),
                ],
                state.busy,
                session_bubbles(state, &own),
            ),
            Target::Peer(id) => {
                let Some(peer) = self.chat_peer() else {
                    return vec![Line::styled(
                        format!(
                            "Peer {} is no longer active in this checkout. Esc goes back.",
                            short(id)
                        ),
                        theme.fg(Tok::Muted),
                    )];
                };
                let mut title = vec![
                    Span::styled("● ", theme.fg(phase_tok(&peer.phase))),
                    Span::styled(
                        peer_name(peer),
                        theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {} · {}", short(&peer.id), peer.phase),
                        theme.fg(Tok::Dim),
                    ),
                ];
                if !peer.summary.trim().is_empty() {
                    title.push(Span::styled(
                        format!(" · {}", text::one_line(&peer.summary)),
                        theme.fg(Tok::Muted),
                    ));
                }
                (title, peer.phase == "working", peer_bubbles(peer, &own))
            }
        };
        bubbles.extend(
            self.outbox
                .iter()
                .filter(|pending| pending.target == target)
                .map(|pending| Bubble {
                    side: Side::Right,
                    sender: "You".into(),
                    text: pending.text.clone(),
                    at_ms: pending.at_ms,
                    receipt: Some(("Sending…".into(), Tok::Dim)),
                }),
        );
        let name = match &target {
            Target::Session => own.clone(),
            Target::Peer(_) => self.chat_peer().map(peer_name).unwrap_or_default(),
        };
        let mut lines = if bubbles.is_empty() {
            vec![Line::styled(
                match target {
                    Target::Session => {
                        "No messages yet. What you type here goes to this session's model, like a prompt in its main window."
                    }
                    Target::Peer(_) => {
                        "No messages yet. What you type here goes to this peer's model; a message wakes an idle or finished session."
                    }
                },
                theme.fg(Tok::Muted),
            )]
        } else {
            chat::render(&bubbles, width, now_ms, theme)
        };
        if working {
            lines.push(Line::styled(
                format!("{name} is working…"),
                theme.fg(Tok::Accent).add_modifier(Modifier::ITALIC),
            ));
        }
        let composer = self.composer(&name, width, theme);
        let lines = text::wrap(lines, width.max(1));
        let room = height.saturating_sub(2 + composer.len());
        let end = lines
            .len()
            .saturating_sub(self.chat_back.min(lines.len().saturating_sub(room)));
        let start = end.saturating_sub(room);
        let mut view = vec![clip_line(Line::from(title), width)];
        // Like a messenger, the newest messages sit just above the composer.
        view.extend(std::iter::repeat_n(Line::default(), room - (end - start)));
        view.extend_from_slice(&lines[start..end]);
        view.push(Line::styled("─".repeat(width), theme.fg(Tok::Border)));
        view.extend(composer);
        view
    }

    /// The composer under an open conversation, at most [`COMPOSER_ROWS`] rows (the newest).
    fn composer(&self, name: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.draft.is_empty() {
            return vec![clip_line(
                Line::from(vec![
                    Span::styled("› ", theme.fg(Tok::Accent)),
                    Span::styled(format!("Message {name}…"), theme.fg(Tok::Dim)),
                ]),
                width,
            )];
        }
        let rows = text::wrap_words(&format!("{}▏", self.draft), width.saturating_sub(2).max(1));
        let skip = rows.len().saturating_sub(COMPOSER_ROWS);
        rows.into_iter()
            .enumerate()
            .skip(skip)
            .map(|(n, row)| {
                Line::from(vec![
                    Span::styled(if n == 0 { "› " } else { "  " }, theme.fg(Tok::Accent)),
                    Span::styled(row, theme.fg(Tok::Text)),
                ])
            })
            .collect()
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
            Tab::Peers => String::new(),
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
            Tab::Peers => {
                let own = if state.model.trim().is_empty() {
                    "This session".to_owned()
                } else {
                    state.model.clone()
                };
                let last = state.conversation.last().map(|line| {
                    let who = match line.role {
                        ChatRole::User => "You: ",
                        ChatRole::Model | ChatRole::Event => "",
                    };
                    (format!("{who}{}", text::one_line(&line.text)), line.at_ms)
                });
                let mut rows = vec![conversation_row(
                    theme,
                    if state.busy { Tok::Accent } else { Tok::Muted },
                    format!("{own} (this session)"),
                    last,
                    0,
                    false,
                )];
                rows.extend(state.peers.rows.iter().map(|peer| {
                    let last = peer.messages.last().map(|message| {
                        let who = if message.outgoing { "You: " } else { "" };
                        (
                            format!("{who}{}", text::one_line(&message.text)),
                            message.at_ms,
                        )
                    });
                    let unread = peer
                        .messages
                        .iter()
                        .filter(|message| !message.outgoing && message.unread)
                        .count();
                    conversation_row(
                        theme,
                        phase_tok(&peer.phase),
                        format!("{} · {} · {}", peer_name(peer), short(&peer.id), peer.phase),
                        last.or_else(|| {
                            (!peer.summary.trim().is_empty())
                                .then(|| (text::one_line(&peer.summary), 0))
                        }),
                        unread,
                        !peer.overlapping.is_empty(),
                    )
                }));
                rows
            }
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
                match self.selected_peer() {
                    Some(peer) => {
                        out.title(&peer_name(peer));
                        out.field("Peer", &peer.id);
                        out.field("Phase", &peer.phase);
                        out.optional("Task", Some(&peer.summary));
                        out.field("Messages", &peer.messages.len().to_string());
                        out.list("Advisory claims", &peer.scopes, Tok::Text);
                        out.list("Overlaps yours", &peer.overlapping, Tok::Warning);
                    }
                    None => {
                        out.title("This session");
                        out.optional("Model", Some(&state.model));
                        out.field("Status", if state.busy { "working" } else { "idle" });
                        out.field("Messages", &state.conversation.len().to_string());
                        if let Some(error) = &peers.error {
                            out.field("Peers", error);
                        } else if peers.rows.is_empty() {
                            out.text("No other Dial sessions in this checkout.", Tok::Muted);
                        }
                    }
                }
                out.lines.push(Line::default());
                out.text(
                    "Enter opens the conversation. Automatic wake-up depends on peer settings and eligibility.",
                    Tok::Dim,
                );
                out.list("This session's advisory claims", &peers.scopes, Tok::Text);
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

/// A peer as a chat contact: its model, or its short id when it does not say.
fn peer_name(peer: &state::PeerRow) -> String {
    if peer.model.trim().is_empty() {
        format!("peer {}", short(&peer.id))
    } else {
        text::one_line(&peer.model)
    }
}

fn phase_tok(phase: &str) -> Tok {
    match phase {
        "working" => Tok::Accent,
        "idle" => Tok::Muted,
        _ => Tok::Warning,
    }
}

/// Whether the session already published a message this pane sent.
fn published(state: &Snapshot, pending: &Pending) -> bool {
    let recent = |at_ms: u64| at_ms + 10_000 >= pending.at_ms;
    match &pending.target {
        Target::Session => state.conversation.iter().any(|line| {
            line.role == ChatRole::User && line.text == pending.text && recent(line.at_ms)
        }),
        Target::Peer(id) => state
            .peers
            .rows
            .iter()
            .filter(|peer| &peer.id == id)
            .flat_map(|peer| &peer.messages)
            .any(|message| {
                message.outgoing && message.text == pending.text && recent(message.at_ms)
            }),
    }
}

/// This session's own thread: prompts on the right, replies on the left, events in the middle.
fn session_bubbles(state: &Snapshot, own: &str) -> Vec<Bubble> {
    state
        .conversation
        .iter()
        .map(|line| {
            let (side, sender) = match line.role {
                ChatRole::User => (Side::Right, "You".to_owned()),
                ChatRole::Model => (Side::Left, own.to_owned()),
                ChatRole::Event => (Side::Center, String::new()),
            };
            Bubble {
                side,
                sender,
                text: line.text.clone(),
                at_ms: line.at_ms,
                receipt: None,
            }
        })
        .collect()
}

/// A peer thread: this session on the right (its model, or "You" for what the human typed here),
/// the peer on the left. The newest outgoing message carries an inbox receipt, never agreement.
fn peer_bubbles(peer: &state::PeerRow, own: &str) -> Vec<Bubble> {
    let last_out = peer.messages.iter().rposition(|message| message.outgoing);
    peer.messages
        .iter()
        .enumerate()
        .map(|(n, message)| {
            let receipt = if message.outgoing {
                (Some(n) == last_out).then(|| {
                    // Unknown, not a failure: the peer may simply have ended.
                    if !message.receipt_available {
                        ("Receipt unavailable".to_owned(), Tok::Muted)
                    } else if message.unread {
                        ("Delivered".to_owned(), Tok::Muted)
                    } else {
                        ("Collected · read unconfirmed".to_owned(), Tok::Muted)
                    }
                })
            } else {
                message
                    .unread
                    .then(|| (format!("Queued for {own}"), Tok::Warning))
            };
            Bubble {
                side: if message.outgoing {
                    Side::Right
                } else {
                    Side::Left
                },
                sender: match (message.outgoing, message.by_user) {
                    (true, true) => "You".to_owned(),
                    (true, false) => own.to_owned(),
                    (false, _) => peer_name(peer),
                },
                text: message.text.clone(),
                at_ms: message.at_ms,
                receipt,
            }
        })
        .collect()
}

/// One row of the Chats list: status dot, contact, newest message preview and badges.
fn conversation_row(
    theme: &Theme,
    dot: Tok,
    name: String,
    last: Option<(String, u64)>,
    unread: usize,
    overlaps: bool,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled("● ", theme.fg(dot)),
        Span::styled(name, theme.fg(Tok::Text).add_modifier(Modifier::BOLD)),
    ];
    if unread > 0 {
        spans.push(Span::styled(
            format!("  {unread} new"),
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD),
        ));
    }
    if overlaps {
        spans.push(Span::styled("  overlaps", theme.fg(Tok::Warning)));
    }
    if let Some((preview, _)) = last {
        spans.push(Span::styled(format!("  {preview}"), theme.fg(Tok::Muted)));
    }
    Line::from(spans)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Fit a note and key hints without hiding the final (exit) hint.
fn footer_line(
    theme: &Theme,
    note: Option<Span<'static>>,
    hints: &[(&str, &str)],
    width: usize,
) -> Line<'static> {
    let Some(note) = note.filter(|note| !note.content.trim().is_empty()) else {
        return chrome::hint_line(theme, hints, width);
    };
    let mut line = chrome::hint_line_after(theme, &note.content, hints, width);
    if let Some(lead) = line.spans.first_mut()
        && lead.style == theme.fg(Tok::Muted)
    {
        lead.style = note.style;
    }
    line
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
