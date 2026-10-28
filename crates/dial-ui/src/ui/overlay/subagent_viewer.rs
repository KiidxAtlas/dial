//! Full-screen view of one worker's conversation, live while it runs. The conversation is rendered
//! by the main transcript renderer from the worker's JSONL archive, which is read incrementally.
//! The roster and the cancel/steer/resume routing live in the hub; this panel only reports
//! operator intent as `subagent-*` action strings for `subagent_model::act`.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::event::{Event, KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;

use super::{
    DrawCtx, OverlayEvent,
    settings::{editor_spans, filled, hint, hint_line, is_ctrl, panel, pressed},
};
use crate::{
    config::paths::Layout,
    llm::model_config::UiConfig,
    runtime::Tree,
    ui::{
        Rows,
        chrome::Section,
        editor::Editor,
        fx,
        keys::{Action, Keymap},
        subagent_model::{self, AgentEntry, AgentStatus},
        text,
        theme::{Sym, Theme, Tok},
        transcript::{Replay, Transcript, TranscriptCtx},
    },
};

/// Transcript file polled for new records at most this often.
const TAIL_EVERY: Duration = Duration::from_millis(250);
/// Roster snapshot (worker status, transcript path) refreshed at most this often.
const MODEL_EVERY: Duration = Duration::from_secs(1);
/// Two Left presses this close together, on an empty draft, return to the hub.
const LEFT_TWICE: Duration = Duration::from_millis(600);
/// Lines moved per mouse wheel tick.
const WHEEL_LINES: isize = 3;

/// Message records of a child transcript, replayed into the main transcript model. The file is
/// only ever appended to, so a read keeps its byte offset and consumes only complete lines.
#[derive(Default)]
pub struct Conversation {
    transcript: Transcript,
    replay: Replay,
    /// Provisional public prose, kept separate so the authoritative assistant message
    /// can replace it without replaying or cloning the settled conversation.
    streaming: Transcript,
    pending_revision: u64,
    /// Tool calls currently running (or settled but not yet recorded), with the revision read.
    live: Vec<crate::agent::workers::child_transcript::LiveTool>,
    live_revision: u64,
    /// First transcript block of each message record: `entries[i]` is entry `i`.
    entries: Vec<usize>,
    /// Bytes consumed so far. Always ends on a complete line.
    offset: u64,
    /// Device and inode of the file the offset refers to.
    identity: (u64, u64),
    /// The first bytes of the file as they were read, to catch an in-place rewrite.
    head: Vec<u8>,
    /// A capped read left complete records unread; they are replayed on the next poll.
    behind: bool,
    revision: u64,
}

/// Leading bytes compared on every poll to detect a rewrite that kept the same inode and size.
const HEAD: usize = 256;
/// Most bytes of new records replayed by one poll.
const CHUNK: u64 = 4 << 20;

impl Conversation {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Applies the complete records appended since the last call. The file is replayed from the
    /// start when it was truncated, replaced (another inode) or rewritten in place. Returns whether
    /// anything changed. Only appended bytes are parsed; a partial last line waits for its newline.
    pub fn follow(&mut self, path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let Ok(mut file) = File::open(path) else {
            return false;
        };
        let len = meta.len();
        let identity = file_identity(&meta);
        if self.is_rewritten(&mut file, identity, len) {
            *self = Self {
                revision: self.revision + 1,
                ..Self::default()
            };
        }
        if len <= self.offset {
            return self.follow_pending(path);
        }
        // Bounded per poll: a long transcript is replayed over several polls instead of
        // blocking one paint. A single record longer than the chunk is still read whole.
        let chunk_end = len.min(self.offset.saturating_add(CHUNK));
        let Ok(mut bytes) = read_range(&mut file, self.offset, chunk_end) else {
            return false;
        };
        let mut complete = bytes.iter().rposition(|byte| *byte == b'\n');
        let mut behind = chunk_end < len;
        if complete.is_none() && behind {
            let Ok(rest) = read_range(&mut file, self.offset, len) else {
                return false;
            };
            bytes = rest;
            complete = bytes.iter().rposition(|byte| *byte == b'\n');
            behind = false;
        }
        self.behind = behind;
        // A record still being written has no newline yet; it is read on a later poll.
        let Some(end) = complete.map(|i| i + 1) else {
            return self.follow_pending(path);
        };
        for line in bytes[..end].split(|byte| *byte == b'\n') {
            self.record(line);
        }
        self.offset += end as u64;
        self.identity = identity;
        if self.head.len() < HEAD {
            self.head = read_range(&mut file, 0, self.offset.min(HEAD as u64)).unwrap_or_default();
        }
        self.revision += 1;
        self.follow_pending(path);
        true
    }

    fn follow_pending(&mut self, path: &Path) -> bool {
        let streaming = self.follow_streaming(path);
        let live = self.follow_live(path);
        streaming || live
    }

    fn follow_streaming(&mut self, path: &Path) -> bool {
        let Some((revision, text)) =
            crate::agent::workers::child_transcript::pending_assistant(path, self.pending_revision)
        else {
            return false;
        };
        self.pending_revision = revision;
        self.streaming = Transcript::new();
        if !text.is_empty() {
            self.streaming.assistant_delta(&text);
        }
        self.revision += 1;
        true
    }

    fn follow_live(&mut self, path: &Path) -> bool {
        let Some((revision, tools)) =
            crate::agent::workers::child_transcript::live_tools(path, self.live_revision)
        else {
            return false;
        };
        self.live_revision = revision;
        self.live = tools;
        self.revision += 1;
        true
    }

    fn is_rewritten(&self, file: &mut File, identity: (u64, u64), len: u64) -> bool {
        if self.offset == 0 {
            return false;
        }
        if identity != self.identity || len < self.offset {
            return true;
        }
        // Same file: a rewrite that kept the size still changes the leading bytes.
        match read_range(file, 0, self.head.len() as u64) {
            Ok(head) => head != self.head,
            Err(_) => true,
        }
    }

    fn record(&mut self, line: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        // Older archives may contain delta records. Do not display their raw
        // fragments: only authoritative messages and cumulative safe snapshots.
        if let Some("message") = value["type"].as_str() {
            if value["message"]["role"] == "assistant" {
                self.streaming = Transcript::new();
            }
            let first = self.replay.push(&mut self.transcript, &value["message"]);
            self.entries.push(first);
        }
    }

    /// The worker has ended: tools that never reported a result are marked interrupted.
    fn finish(&mut self) {
        self.transcript.finish_turn();
        self.streaming.finish_turn();
        self.revision += 1;
    }

    /// Whether complete records remain beyond what was replayed so far.
    fn is_behind(&self) -> bool {
        self.behind
    }
}

/// Reads `start..end` from `file`.
fn read_range(file: &mut File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(end.saturating_sub(start))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Device and inode: a replaced file is a new inode even when it is longer. Modification time is
/// not used because every append changes it.
#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Scroll position of the body. `follow` keeps the newest rows in view as they arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scroll {
    pub top: usize,
    pub follow: bool,
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            top: 0,
            follow: true,
        }
    }
}

impl Scroll {
    fn max_top(total: usize, view: usize) -> usize {
        total.saturating_sub(view)
    }

    /// Settles the position for a body of `total` rows shown in `view` rows.
    pub fn clamp(&mut self, total: usize, view: usize) {
        let max = Self::max_top(total, view);
        self.top = if self.follow { max } else { self.top.min(max) };
    }

    /// Moves by `delta` rows. Reaching the bottom resumes following; moving up pauses it.
    pub fn by(&mut self, delta: isize, total: usize, view: usize) {
        let max = Self::max_top(total, view);
        // While following, the stored top is only settled by the next draw; move from the bottom.
        let base = if self.follow { max } else { self.top.min(max) };
        self.top = (base as isize + delta).clamp(0, max as isize) as usize;
        self.follow = self.top >= max;
    }

    pub fn to_top(&mut self) {
        self.top = 0;
        self.follow = false;
    }

    pub fn to_bottom(&mut self) {
        self.follow = true;
    }
}

/// Where a message entry starts in the rendered rows, if it has been replayed yet.
pub fn entry_row(entries: &[usize], anchors: &[usize], entry: usize) -> Option<usize> {
    let block = *entries.get(entry)?;
    anchors.get(block).copied()
}

/// How the composer's message reaches the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Delivered at the worker's next step.
    Steer,
    /// Continues a finished worker through the resume path.
    Resume,
    /// Cannot receive text; the reason is shown in the composer.
    Unavailable(&'static str),
}

pub fn route(agent: Option<&AgentEntry>) -> Route {
    let Some(agent) = agent else {
        return Route::Unavailable("Loading worker state…");
    };
    if agent.steerable {
        return Route::Steer;
    }
    if agent.resumable {
        return Route::Resume;
    }
    match agent.status {
        AgentStatus::Cancelled => Route::Unavailable("Cancelled workers cannot continue"),
        AgentStatus::Running | AgentStatus::Waiting => {
            Route::Unavailable("This worker cannot take a message right now")
        }
        AgentStatus::Done | AgentStatus::Failed => {
            Route::Unavailable("This worker cannot be continued")
        }
    }
}

struct Cache {
    /// Width, expansion, conversation revision and (while running) the animation frame.
    key: (u16, bool, u64, u64),
    rows: Rows,
    /// First row of each transcript block.
    anchors: Vec<usize>,
}

/// Editors belong to the parent session controller, not a transient overlay. Closing or switching
/// a worker view preserves its buffer, cursor and undo history without touching the main draft.
#[derive(Clone, Default)]
pub struct WorkerEditors(Arc<Mutex<BTreeMap<String, Editor>>>);

impl WorkerEditors {
    pub fn clear(&self, session: &str, worker: &str) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&format!("{session}/{worker}"));
    }
}

pub struct SubagentViewer {
    id: String,
    session_id: String,
    from_hub: bool,
    ui: UiConfig,
    agent: Option<AgentEntry>,
    path: Option<PathBuf>,
    conversation: Conversation,
    finished_applied: bool,
    next_model: Instant,
    next_tail: Instant,
    scroll: Scroll,
    /// Message entry to scroll to once it has been replayed.
    pending_entry: Option<usize>,
    expanded: bool,
    draft: Editor,
    editors: Option<WorkerEditors>,
    editor_key: String,
    pending_submit: bool,
    notice: Option<String>,
    left_at: Option<Instant>,
    cache: Option<Cache>,
    /// Total rows and visible rows from the last draw, for paging and wheel scrolling.
    geometry: (usize, usize),
}

impl SubagentViewer {
    pub fn new(
        id: String,
        session_id: String,
        entry: Option<usize>,
        from_hub: bool,
        ui: UiConfig,
        now: Instant,
    ) -> Self {
        Self {
            editor_key: format!("{session_id}/{id}"),
            editors: None,
            pending_submit: false,
            id,
            session_id,
            from_hub,
            ui,
            agent: None,
            path: None,
            conversation: Conversation::default(),
            finished_applied: false,
            next_model: now,
            next_tail: now,
            scroll: Scroll::default(),
            pending_entry: entry,
            expanded: false,
            draft: Editor::default(),
            notice: None,
            left_at: None,
            cache: None,
            geometry: (0, 0),
        }
    }
    pub fn with_editors(mut self, editors: WorkerEditors) -> Self {
        if let Some(editor) = editors
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.editor_key)
        {
            self.draft = editor;
        }
        self.editors = Some(editors);
        self
    }

    /// A delivery failure keeps the submitted buffer available for correction or retry.
    pub fn action_failed(&mut self, message: impl Into<String>) {
        self.pending_submit = false;
        self.set_notice(message);
    }

    pub fn action_succeeded(&mut self, message: impl Into<String>) {
        if std::mem::take(&mut self.pending_submit) {
            self.draft.set("");
        }
        self.set_notice(message);
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Shows a line in the footer, e.g. the outcome of an action the app just ran.
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
    }

    /// Refreshes the worker's status (about once a second) and its transcript (about four times a
    /// second). Cheap when nothing is due; call it from the app's periodic tick, never from draw.
    pub fn refresh(
        &mut self,
        now: Instant,
        layout: &Layout,
        runtime: &Tree,
        live: Option<&crate::agent::LiveTaskControl>,
    ) {
        if now >= self.next_model {
            self.next_model = now + MODEL_EVERY;
            let snapshot = subagent_model::snapshot(layout, &self.session_id, runtime, live);
            self.agent = snapshot
                .agents
                .into_iter()
                .find(|agent| agent.id == self.id);
            self.path = self
                .agent
                .as_ref()
                .and_then(|agent| agent.transcript.clone());
        }
        if now >= self.next_tail {
            self.next_tail = now + TAIL_EVERY;
            if let Some(path) = &self.path {
                self.conversation.follow(path);
            }
            let finished = self
                .agent
                .as_ref()
                .is_some_and(|agent| agent.status.is_finished());
            if finished && !self.finished_applied && !self.conversation.is_behind() {
                self.conversation.finish();
                self.finished_applied = true;
            } else if !finished {
                self.finished_applied = false;
            }
        }
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap, now: Instant) -> OverlayEvent {
        match event {
            Event::Key(key) if pressed(key) => self.key(key, keys, now),
            // Pasted steering goes into the draft (the editor sanitizes it); it never submits.
            Event::Paste(text) => {
                self.notice = None;
                self.draft.insert(text);
                OverlayEvent::None
            }
            Event::Mouse(mouse) => self.wheel(mouse),
            _ => OverlayEvent::None,
        }
    }

    fn wheel(&mut self, mouse: &MouseEvent) -> OverlayEvent {
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -WHEEL_LINES,
            MouseEventKind::ScrollDown => WHEEL_LINES,
            _ => return OverlayEvent::None,
        };
        self.scroll_by(delta);
        OverlayEvent::None
    }

    fn scroll_by(&mut self, delta: isize) {
        let (total, view) = self.geometry;
        self.scroll.by(delta, total, view);
    }

    fn page(&self) -> isize {
        let (_, view) = self.geometry;
        view.saturating_sub(1).max(1) as isize
    }

    fn key(&mut self, key: &KeyEvent, keys: &Keymap, now: Instant) -> OverlayEvent {
        self.notice = None;
        let empty = self.draft.text.is_empty();

        if keys.matches(Action::SelectCancel, key) {
            return if self.from_hub {
                OverlayEvent::Action("subagent-hub".to_owned())
            } else {
                OverlayEvent::Close
            };
        }
        if is_ctrl(key, 'c') {
            return if self.agent.as_ref().is_some_and(|agent| agent.cancellable) {
                OverlayEvent::Action(format!("subagent-cancel:{}", self.id))
            } else {
                self.notice = Some("This worker is not running".to_owned());
                OverlayEvent::None
            };
        }
        if keys.matches(Action::Submit, key) {
            let text = self.draft.text.trim().to_owned();
            if text.is_empty() {
                return OverlayEvent::None;
            }
            if text == "/back" {
                self.draft.set("");
                return OverlayEvent::Close;
            }
            for (command, axis) in [
                ("/model", "model"),
                ("/thinking", "thinking"),
                ("/effort", "thinking"),
            ] {
                if text == command {
                    self.notice = Some(format!(
                        "Usage: {command} <{}>; changes this worker only",
                        if axis == "model" {
                            "profile or model"
                        } else {
                            "off|low|medium|high|ultra"
                        }
                    ));
                    return OverlayEvent::None;
                }
                if let Some(value) = text
                    .strip_prefix(command)
                    .and_then(|rest| rest.strip_prefix(' '))
                {
                    let value = value.trim();
                    if !value.is_empty() {
                        self.pending_submit = true;
                        return OverlayEvent::Action(format!(
                            "subagent-config:{}\n{axis}\n{value}",
                            self.id
                        ));
                    }
                }
            }
            return match route(self.agent.as_ref()) {
                Route::Steer => {
                    self.pending_submit = true;
                    OverlayEvent::Action(format!("subagent-message:{}\n{text}", self.id))
                }
                Route::Resume => {
                    self.pending_submit = true;
                    OverlayEvent::Action(format!("subagent-resume:{}\n{text}", self.id))
                }
                Route::Unavailable(reason) => {
                    self.notice = Some(reason.to_owned());
                    OverlayEvent::None
                }
            };
        }
        if keys.matches(Action::ToolsExpand, key) {
            self.expanded = !self.expanded;
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectUp, key) {
            self.scroll_by(-1);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectDown, key) {
            self.scroll_by(1);
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectPageUp, key) {
            self.scroll_by(-self.page());
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectPageDown, key) {
            self.scroll_by(self.page());
            return OverlayEvent::None;
        }
        // Home/End jump only while there is no draft; with text they move the editor cursor.
        if empty && key.modifiers.is_empty() && matches!(key.code, KeyCode::Home | KeyCode::End) {
            if key.code == KeyCode::Home {
                self.scroll.to_top();
            } else {
                self.scroll.to_bottom();
            }
            return OverlayEvent::None;
        }
        if keys.matches(Action::SubagentsHub, key) {
            return OverlayEvent::Action("subagent-hub".to_owned());
        }
        // Left twice on an empty draft is the gesture form of the hub key, as in the composer.
        if key.code == KeyCode::Left && key.modifiers.is_empty() && empty {
            if self
                .left_at
                .is_some_and(|at| now.duration_since(at) <= LEFT_TWICE)
            {
                self.left_at = None;
                return OverlayEvent::Action("subagent-hub".to_owned());
            }
            self.left_at = Some(now);
            return OverlayEvent::None;
        }

        self.draft.key_with(*key, keys);
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let name = self
            .agent
            .as_ref()
            .map(|agent| agent.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.id.clone());
        let extra = self.agent.as_ref().map_or("loading".to_owned(), |agent| {
            agent.status.label().to_owned()
        });
        panel(
            frame,
            ctx.theme,
            Section::Sandbox,
            &name,
            &extra,
            |width, height| self.compose(usize::from(width), usize::from(height), ctx),
        );
    }

    fn compose(&mut self, width: usize, height: usize, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let theme = ctx.theme;
        let footer = self.footer(width, ctx);
        let mut lines = vec![self.header(width, ctx), self.task(width, theme)];
        let body_h = height.saturating_sub(lines.len() + footer.len());

        let key_width = width.min(u16::MAX as usize) as u16;
        self.ensure_rows(key_width, ctx);
        let (total, anchors_ready) = match &self.cache {
            Some(cache) => (cache.rows.len(), !cache.anchors.is_empty()),
            None => (0, false),
        };
        if anchors_ready
            && let (Some(entry), Some(cache)) = (self.pending_entry, self.cache.as_ref())
            && let Some(row) = entry_row(&self.conversation.entries, &cache.anchors, entry)
        {
            self.scroll = Scroll {
                top: row,
                follow: false,
            };
            self.pending_entry = None;
        }
        self.scroll.clamp(total, body_h);
        self.geometry = (total, body_h);

        let body = match &self.cache {
            Some(cache) if total > 0 => {
                let top = self.scroll.top.min(total);
                let end = (top + body_h).min(total);
                cache.rows[top..end].to_vec()
            }
            _ => Vec::new(),
        };
        if body.is_empty() {
            lines.push(Line::from(Span::styled(
                self.empty_body_text(),
                theme.fg(Tok::Muted),
            )));
        } else {
            lines.extend(body);
        }
        while lines.len() < height.saturating_sub(footer.len()) {
            lines.push(Line::default());
        }
        lines.extend(footer);
        lines
    }

    fn empty_body_text(&self) -> String {
        match &self.agent {
            None => "Loading worker…".to_owned(),
            Some(agent) if agent.transcript.is_none() => {
                "This worker keeps no transcript file".to_owned()
            }
            Some(_) => "Waiting for the first message…".to_owned(),
        }
    }

    fn ensure_rows(&mut self, width: u16, ctx: &DrawCtx) {
        let running = self
            .agent
            .as_ref()
            .is_some_and(|agent| !agent.status.is_finished());
        let tick = if running { ctx.status.tick } else { 0 };
        let key = (width, self.expanded, self.conversation.revision(), tick);
        if self.cache.as_ref().is_some_and(|cache| cache.key == key) {
            return;
        }
        let transcript_ctx = TranscriptCtx {
            theme: ctx.theme,
            ui: &self.ui,
            tick: ctx.status.tick,
            tools_expanded: self.expanded,
            expand_key: &ctx.keys.label(Action::ToolsExpand),
        };
        let (mut rows, anchors) = self
            .conversation
            .transcript
            .document(width, &transcript_ctx);
        rows.extend(
            self.conversation
                .streaming
                .document(width, &transcript_ctx)
                .0,
        );
        for tool in &self.conversation.live {
            let state = if tool.running { "running" } else { "finished" };
            rows.push(Line::from(Span::styled(
                format!("▸ {} · {state}", tool.name),
                ctx.theme.fg(Tok::Muted),
            )));
        }
        self.cache = Some(Cache { key, rows, anchors });
    }

    fn header(&self, width: usize, ctx: &DrawCtx) -> Line<'static> {
        let theme = ctx.theme;
        let Some(agent) = &self.agent else {
            return filled(
                vec![Span::styled(
                    format!(" {}  loading…", self.id),
                    theme.fg(Tok::Muted),
                )],
                width,
                Style::default(),
            );
        };
        let (glyph, glyph_style) = match agent.status {
            AgentStatus::Running => (
                theme.spinner(ctx.status.tick).to_owned(),
                fx::pulse(theme, ctx.status.tick),
            ),
            // A worker blocked on the operator must not look busy.
            AgentStatus::Waiting => (theme.sym(Sym::Warn).to_owned(), theme.fg(Tok::Warning)),
            AgentStatus::Done => (theme.sym(Sym::Check).to_owned(), theme.fg(Tok::Success)),
            AgentStatus::Failed => (theme.sym(Sym::Cross2).to_owned(), theme.fg(Tok::Error)),
            AgentStatus::Cancelled => (theme.sym(Sym::Cross2).to_owned(), theme.fg(Tok::Muted)),
        };
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(
                agent.name.clone(),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            ),
        ];
        // The panel title already names the status; repeat it here only when it asks for action.
        let mut facts = vec![agent.agent.clone()];
        if agent.status == AgentStatus::Waiting {
            facts.push("needs your input".to_owned());
        }
        if !agent.model.is_empty() {
            facts.push(agent.model.clone());
        }
        if let Some(ms) = agent.elapsed_ms {
            facts.push(format_elapsed(ms));
        }
        if let Some(cost) = agent.cost_usd {
            facts.push(format!("${cost:.3}"));
        }
        for fact in facts.into_iter().filter(|fact| !fact.is_empty()) {
            spans.push(Span::styled(
                format!(" {} ", theme.sym(Sym::Dot)),
                theme.fg(Tok::Dim),
            ));
            spans.push(Span::styled(fact, theme.fg(Tok::Muted)));
        }
        filled(spans, width, Style::default())
    }

    fn task(&self, width: usize, theme: &Theme) -> Line<'static> {
        let task = self
            .agent
            .as_ref()
            .map(|agent| text::one_line(&agent.task))
            .unwrap_or_default();
        let clipped = text::ellipsize(&task, width.saturating_sub(4), theme.ascii());
        filled(
            vec![
                Span::styled("   ", theme.fg(Tok::Dim)),
                Span::styled(clipped, theme.fg(Tok::Muted)),
            ],
            width,
            Style::default(),
        )
    }

    /// Composer row, optional notice, and hint row, bottom of the panel.
    fn footer(&self, width: usize, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let theme = ctx.theme;
        let mut rows = Vec::new();
        if let Some(notice) = &self.notice {
            rows.push(filled(
                vec![Span::styled(
                    format!(" {}", text::clip(notice, width.saturating_sub(2))),
                    theme.fg(Tok::Warning),
                )],
                width,
                Style::default(),
            ));
        }
        let route = route(self.agent.as_ref());
        let mut spans = vec![Span::styled(
            format!("{} ", theme.sym(Sym::Prompt)),
            theme.fg(Tok::Accent),
        )];
        if self.draft.text.is_empty() {
            let placeholder = match route {
                Route::Steer => "Message worker — delivered at its next step".to_owned(),
                Route::Resume => "Continue this worker".to_owned(),
                Route::Unavailable(reason) => reason.to_owned(),
            };
            spans.push(Span::styled(
                text::clip(&placeholder, width.saturating_sub(2)),
                theme.fg(Tok::Dim),
            ));
        } else {
            spans.extend(editor_spans(
                theme,
                &self.draft,
                width.saturating_sub(2),
                false,
            ));
        }
        rows.push(filled(spans, width, Style::default()));

        let esc = if self.from_hub {
            "back · draft kept"
        } else {
            "main · draft kept"
        };
        let mut segments = Vec::new();
        // Scroll keys type into the draft once it has text, so they are offered only without one.
        if self.draft.text.is_empty() {
            segments.push(format!(
                "{} scroll",
                super::settings::arrows(theme, super::settings::Axis::Vertical)
            ));
            segments.push("PgUp/PgDn page".to_owned());
        }
        segments.push(hint(
            ctx.keys,
            Action::ToolsExpand,
            if self.expanded {
                "collapse tools"
            } else {
                "expand tools"
            },
        ));
        let verb = match route {
            Route::Steer => Some("send"),
            Route::Resume => Some("continue"),
            Route::Unavailable(_) => None,
        };
        if let Some(verb) = verb.filter(|_| !self.draft.text.is_empty()) {
            segments.push(hint(ctx.keys, Action::Submit, verb));
        }
        if self.agent.as_ref().is_some_and(|agent| agent.cancellable) {
            segments.push("Ctrl+C abort worker".to_owned());
        }
        segments.push(hint(ctx.keys, Action::SubagentsHub, "all workers"));
        segments.push(hint(ctx.keys, Action::SelectCancel, esc));
        segments.push("/model · /thinking · /back".to_owned());
        rows.push(hint_line(theme, &segments, width));
        rows
    }
}

impl Drop for SubagentViewer {
    fn drop(&mut self) {
        if let Some(editors) = &self.editors {
            let mut editors = editors.0.lock().unwrap_or_else(|error| error.into_inner());
            if !self.draft.text.is_empty() {
                editors.insert(
                    std::mem::take(&mut self.editor_key),
                    std::mem::take(&mut self.draft),
                );
            }
        }
    }
}

/// `12s`, `3m 04s`, `1h 02m`.
pub fn format_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests;
