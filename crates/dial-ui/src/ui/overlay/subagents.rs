//! Subagent hub panel: a roster of workers with an inspector for the selected one, plus an
//! activity feed across all of them. The controller ([`crate::ui::subagent_hub`]) pushes
//! snapshots from [`crate::ui::subagent_model`]; this panel reads no files.
use std::cell::Cell;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{
    DrawCtx, OverlayEvent,
    settings::{filled, hint, hint_line, pair_label, panel},
};
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    subagent_model::{
        ActivityEvent, ActivityFilter, ActivityKind, ActivityScope, AgentEntry, AgentStatus,
        HubSnapshot, Totals, TreeRow, epoch_ms, matches_filter, tree_rows,
    },
    text,
    theme::{Sym, Theme, Tok},
};

/// Panels at least this wide show the roster and inspector side by side.
const SPLIT_AT: usize = 96;
/// A second `x` within this window confirms a cancel.
const CANCEL_CONFIRM: Duration = Duration::from_secs(2);
/// Inspector recent-activity lines.
const RECENT_ACTIVITY: usize = 8;
/// Lines per roster card: header, task, detail.
const CARD_LINES: usize = 3;
/// Two clicks on the same row inside this window open it.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Agents,
    Activity,
}

#[derive(Default)]
pub struct SubagentsOverlay {
    agents: Vec<AgentEntry>,
    activity: Vec<ActivityEvent>,
    totals: Totals,
    routes: Vec<String>,
    tab: Tab,
    /// Id of the worker under the cursor; survives refreshes.
    selected: Option<String>,
    /// Tree view with rails (ignored while a filter is applied).
    tree: bool,
    agent_query: String,
    /// Narrow layouts: the inspector full width instead of the roster.
    inspector: bool,
    inspector_scroll: usize,
    /// Draft of the active tab's query while `/` is open; the query applies as it is typed.
    search: Option<String>,
    compose: Option<String>,
    message: Option<(String, bool)>,
    armed: Option<(String, Instant)>,
    activity_filter: ActivityFilter,
    activity_scope: ActivityScope,
    activity_query: String,
    /// Index into the filtered feed; `usize::MAX` means the newest event.
    activity_cursor: usize,
    follow: bool,
    /// Set while drawing so the key handler knows which layout is on screen.
    wide: Cell<bool>,
    /// Absolute position of the body's first cell, set while drawing.
    origin: Cell<(u16, u16)>,
    /// Rows above the body, and the body's height, from the last draw.
    body_top: Cell<u16>,
    body_h: Cell<u16>,
    /// Columns of the roster pane (0 when no roster is drawn); the inspector starts after it.
    roster_w: Cell<u16>,
    /// First roster card and first activity row drawn, so a click maps to its row.
    roster_start: Cell<usize>,
    activity_start: Cell<usize>,
    /// Last click: when it happened and which row it hit, for double-click detection.
    last_click: Option<(std::time::Instant, String)>,
}

#[derive(Clone, Copy)]
struct Row {
    index: usize,
    depth: usize,
    last: bool,
}

impl From<TreeRow> for Row {
    fn from(row: TreeRow) -> Self {
        Self {
            index: row.index,
            depth: row.depth,
            last: row.last,
        }
    }
}

impl SubagentsOverlay {
    pub fn new() -> Self {
        Self {
            follow: true,
            activity_cursor: usize::MAX,
            ..Self::default()
        }
    }

    /// Opens on the Activity tab.
    pub fn show_activity(&mut self) {
        self.tab = Tab::Activity;
    }

    /// Replaces the roster, feed and totals. Keeps the roster order and the selection.
    pub fn set_snapshot(&mut self, snapshot: HubSnapshot) {
        self.agents = self.order_agents(snapshot.agents);
        self.activity = snapshot.activity;
        self.totals = snapshot.totals;
        self.sync_selection();
    }

    /// Existing workers keep their position; new ones append in snapshot order.
    fn order_agents(&self, mut incoming: Vec<AgentEntry>) -> Vec<AgentEntry> {
        let mut out = Vec::with_capacity(incoming.len());
        for id in self.agents.iter().map(|a| a.id.as_str()) {
            if let Some(pos) = incoming.iter().position(|a| a.id == id) {
                out.push(incoming.remove(pos));
            }
        }
        out.extend(incoming);
        out
    }

    pub fn set_routes(&mut self, routes: Vec<String>) {
        self.routes = routes;
    }

    pub fn banner(&mut self, message: String, is_error: bool) {
        self.message = Some((message, is_error));
    }

    #[cfg(test)]
    pub fn banner_text(&self) -> Option<String> {
        self.message.as_ref().map(|(text, _)| text.clone())
    }

    /// Workers in display order.
    pub fn agents(&self) -> &[AgentEntry] {
        &self.agents
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if self.compose.is_some() {
            return self.handle_compose(key, keys);
        }
        if self.search.is_some() {
            self.handle_search(key);
            return OverlayEvent::None;
        }
        self.message = None;
        if keys.matches(Action::SelectCancel, key) {
            return self.escape();
        }
        if keys.matches(Action::TabNext, key) || keys.matches(Action::TabPrev, key) {
            self.tab = match self.tab {
                Tab::Agents => Tab::Activity,
                Tab::Activity => Tab::Agents,
            };
            return OverlayEvent::None;
        }
        match key.code {
            KeyCode::Char('1') if plain(key) => {
                self.tab = Tab::Agents;
                return OverlayEvent::None;
            }
            KeyCode::Char('2') if plain(key) => {
                self.tab = Tab::Activity;
                return OverlayEvent::None;
            }
            KeyCode::Tab => {
                if self.tab == Tab::Agents && !self.wide.get() {
                    self.inspector = !self.inspector;
                } else {
                    self.tab = match self.tab {
                        Tab::Agents => Tab::Activity,
                        Tab::Activity => Tab::Agents,
                    };
                }
                return OverlayEvent::None;
            }
            _ => {}
        }
        match self.tab {
            Tab::Agents => self.handle_agents(key, keys),
            Tab::Activity => self.handle_activity(key, keys),
        }
    }

    /// Esc: clear the filter, then leave the narrow inspector, then close.
    fn escape(&mut self) -> OverlayEvent {
        match self.tab {
            Tab::Activity if !self.activity_query.is_empty() => self.activity_query.clear(),
            Tab::Agents if !self.agent_query.is_empty() => {
                self.agent_query.clear();
                self.sync_selection();
            }
            _ if self.inspector => self.inspector = false,
            _ => return OverlayEvent::Close,
        }
        OverlayEvent::None
    }

    fn handle_agents(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        if keys.matches(Action::SelectUp, key)
            || matches!(key.code, KeyCode::Char('k') if plain(key))
        {
            self.move_agent(-1);
        } else if keys.matches(Action::SelectDown, key)
            || matches!(key.code, KeyCode::Char('j') if plain(key))
        {
            self.move_agent(1);
        } else if keys.matches(Action::SelectPageUp, key) {
            self.inspector_scroll = self.inspector_scroll.saturating_sub(5);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.inspector_scroll += 5;
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(agent) = self.selected_agent() {
                return OverlayEvent::Action(format!("subagent-open:{}", agent.id));
            }
        } else if let KeyCode::Char(c) = key.code {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return OverlayEvent::None;
            }
            match c {
                'x' => return self.cancel_request(),
                'm' => self.start_message(),
                'r' => return OverlayEvent::Action("open:roles".into()),
                't' => {
                    self.tree = !self.tree;
                    self.sync_selection();
                }
                '/' => self.search = Some(self.agent_query.clone()),
                _ => {}
            }
        }
        OverlayEvent::None
    }

    fn handle_activity(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let rows = self.activity_rows();
        let pos = self.activity_pos(rows.len());
        if keys.matches(Action::SelectUp, key)
            || matches!(key.code, KeyCode::Char('k') if plain(key))
        {
            self.follow = false;
            self.activity_cursor = pos.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key)
            || matches!(key.code, KeyCode::Char('j') if plain(key))
        {
            self.follow = false;
            self.activity_cursor = (pos + 1).min(rows.len().saturating_sub(1));
        } else if keys.matches(Action::SelectPageUp, key) {
            self.follow = false;
            self.activity_cursor = pos.saturating_sub(10);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.follow = false;
            self.activity_cursor = (pos + 10).min(rows.len().saturating_sub(1));
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(&index) = rows.get(pos) {
                let event = &self.activity[index];
                return OverlayEvent::Action(match event.entry {
                    Some(entry) => format!("subagent-open:{}#{entry}", event.agent_id),
                    None => format!("subagent-open:{}", event.agent_id),
                });
            }
        } else if let KeyCode::Char(c) = key.code {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return OverlayEvent::None;
            }
            match c {
                'f' => {
                    self.activity_filter = self.activity_filter.next();
                    self.activity_cursor = usize::MAX;
                }
                's' => {
                    self.activity_scope = self.activity_scope.next(self.selected.as_deref());
                    self.activity_cursor = usize::MAX;
                }
                ' ' => {
                    self.follow = !self.follow;
                    if self.follow {
                        self.activity_cursor = usize::MAX;
                    }
                }
                '/' => self.search = Some(self.activity_query.clone()),
                _ => {}
            }
        }
        OverlayEvent::None
    }

    fn cancel_request(&mut self) -> OverlayEvent {
        let Some(agent) = self.selected_agent() else {
            return OverlayEvent::None;
        };
        let (id, name) = (agent.id.clone(), agent.name.clone());
        if !agent.cancellable {
            let why = if agent.status.is_finished() {
                "it already finished"
            } else {
                "it cannot be stopped from here"
            };
            self.message = Some((format!("{name}: {why}."), true));
            return OverlayEvent::None;
        }
        // Take the arm so a stale one never confirms a later cancel.
        let armed = self.armed.take();
        if let Some((prev, at)) = armed
            && prev == id
            && at.elapsed() <= CANCEL_CONFIRM
        {
            return OverlayEvent::Action(format!("subagent-cancel:{id}"));
        }
        self.armed = Some((id, Instant::now()));
        self.message = Some((format!("Press x again within 2 s to cancel {name}."), false));
        OverlayEvent::None
    }

    fn start_message(&mut self) {
        let Some(agent) = self.selected_agent() else {
            return;
        };
        if agent.steerable || agent.resumable {
            self.compose = Some(String::new());
            return;
        }
        let why = if agent.status.is_finished() {
            "cannot be resumed (no retained transcript)"
        } else {
            "cannot take messages right now"
        };
        self.message = Some((format!("{} {why}.", agent.name), true));
    }

    fn handle_compose(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        if keys.matches(Action::SelectCancel, key) {
            self.compose = None;
            return OverlayEvent::None;
        }
        match key.code {
            KeyCode::Enter => {
                let body = self.compose.take().unwrap_or_default().trim().to_owned();
                if body.is_empty() {
                    return OverlayEvent::None;
                }
                let Some(id) = self.selected.clone() else {
                    return OverlayEvent::None;
                };
                return OverlayEvent::Action(format!("subagent-message:{id}\n{body}"));
            }
            KeyCode::Backspace => {
                if let Some(draft) = self.compose.as_mut() {
                    draft.pop();
                }
            }
            KeyCode::Char(c) if plain(key) => {
                self.compose.get_or_insert_with(String::new).push(c);
            }
            _ => {}
        }
        OverlayEvent::None
    }

    fn handle_search(&mut self, key: &KeyEvent) {
        let Some(mut draft) = self.search.take() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                // Keep the query; close the editor.
                self.set_query(draft);
                return;
            }
            KeyCode::Esc => {
                self.set_query(String::new());
                return;
            }
            KeyCode::Backspace => {
                draft.pop();
            }
            KeyCode::Char(c) if plain(key) => draft.push(c),
            _ => {}
        }
        self.set_query(draft.clone());
        self.search = Some(draft);
    }

    fn set_query(&mut self, query: String) {
        match self.tab {
            Tab::Agents => {
                self.agent_query = query.trim().to_owned();
                self.sync_selection();
            }
            Tab::Activity => {
                self.activity_query = query.trim().to_owned();
                self.activity_cursor = usize::MAX;
            }
        }
    }

    /// Roster rows in display order: flat by default, tree rails with `t` (no filter).
    fn rows(&self) -> Vec<Row> {
        let query = self.agent_query.trim();
        if self.tree && query.is_empty() {
            return tree_rows(&self.agents).into_iter().map(Row::from).collect();
        }
        (0..self.agents.len())
            .filter(|&index| query.is_empty() || matches_filter(&self.agents[index], query))
            .map(|index| Row {
                index,
                depth: 0,
                last: true,
            })
            .collect()
    }

    fn sync_selection(&mut self) {
        let rows = self.rows();
        let kept = self
            .selected
            .as_ref()
            .is_some_and(|id| rows.iter().any(|row| self.agents[row.index].id == *id));
        if !kept {
            let first = rows.first().map(|row| self.agents[row.index].id.clone());
            if first != self.selected {
                self.inspector_scroll = 0;
            }
            self.selected = first;
        }
    }

    /// Maps an absolute cell to (line within the body, column within the panel); `None` outside
    /// the body rows (tabs, routes, prompt, footer) or before the first draw.
    fn body_point(&self, col: u16, row: u16) -> Option<(usize, u16)> {
        let (ox, oy) = self.origin.get();
        let line = row.checked_sub(oy)?.checked_sub(self.body_top.get())?;
        if line >= self.body_h.get() {
            return None;
        }
        Some((usize::from(line), col.checked_sub(ox)?))
    }

    /// Wheel over the inspector pane scrolls it. Returns true when the pointer was over it.
    pub fn wheel_over_inspector(&mut self, col: u16, row: u16, down: bool) -> bool {
        let Some((_, x)) = self.body_point(col, row) else {
            return false;
        };
        let over = match self.tab {
            Tab::Agents if self.wide.get() => x > self.roster_w.get(),
            Tab::Agents => self.inspector,
            Tab::Activity => false,
        };
        if over {
            self.inspector_scroll = if down {
                self.inspector_scroll + 3
            } else {
                self.inspector_scroll.saturating_sub(3)
            };
        }
        over
    }

    /// Left click on a drawn row selects it; a second click on the same row within
    /// `DOUBLE_CLICK` opens its transcript, as Enter does.
    pub fn click(&mut self, col: u16, row: u16, now: std::time::Instant) -> OverlayEvent {
        let Some((line, x)) = self.body_point(col, row) else {
            return OverlayEvent::None;
        };
        match self.tab {
            Tab::Activity => {
                let Some(k) = line.checked_sub(2).map(|n| self.activity_start.get() + n) else {
                    return OverlayEvent::None;
                };
                let rows = self.activity_rows();
                let Some(&index) = rows.get(k) else {
                    return OverlayEvent::None;
                };
                self.follow = false;
                self.activity_cursor = k;
                if !self.double_clicked(format!("activity:{k}"), now) {
                    return OverlayEvent::None;
                }
                let event = &self.activity[index];
                OverlayEvent::Action(match event.entry {
                    Some(entry) => format!("subagent-open:{}#{entry}", event.agent_id),
                    None => format!("subagent-open:{}", event.agent_id),
                })
            }
            Tab::Agents if x < self.roster_w.get() => {
                let rows = self.rows();
                let Some(row) = rows
                    .get(self.roster_start.get() + line / CARD_LINES)
                    .copied()
                else {
                    return OverlayEvent::None;
                };
                let id = self.agents[row.index].id.clone();
                if self.selected.as_deref() != Some(id.as_str()) {
                    self.inspector_scroll = 0;
                }
                self.selected = Some(id.clone());
                if self.double_clicked(format!("agent:{id}"), now) {
                    return OverlayEvent::Action(format!("subagent-open:{id}"));
                }
                OverlayEvent::None
            }
            Tab::Agents => OverlayEvent::None,
        }
    }

    /// True when `key` was also clicked less than `DOUBLE_CLICK` ago. Clears the record on a hit.
    fn double_clicked(&mut self, key: String, now: std::time::Instant) -> bool {
        let hit = matches!(
            &self.last_click,
            Some((at, last)) if *last == key && now.duration_since(*at) <= DOUBLE_CLICK
        );
        self.last_click = if hit { None } else { Some((now, key)) };
        hit
    }

    fn move_agent(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let pos = rows
            .iter()
            .position(|row| self.is_selected(row.index))
            .unwrap_or(0);
        let next = (pos as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        let id = self.agents[rows[next].index].id.clone();
        if self.selected.as_deref() != Some(id.as_str()) {
            self.inspector_scroll = 0;
        }
        self.selected = Some(id);
    }

    fn is_selected(&self, index: usize) -> bool {
        self.selected.as_deref() == Some(self.agents[index].id.as_str())
    }

    fn selected_agent(&self) -> Option<&AgentEntry> {
        let id = self.selected.as_ref()?;
        self.agents.iter().find(|agent| &agent.id == id)
    }

    fn name_of(&self, id: &str) -> String {
        self.agents
            .iter()
            .find(|agent| agent.id == id)
            .map_or_else(|| id.to_owned(), |agent| agent.name.clone())
    }

    /// Activity indices (into `self.activity`) that pass the filter, scope, and search.
    fn activity_rows(&self) -> Vec<usize> {
        let query = self.activity_query.trim().to_lowercase();
        self.activity
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                self.activity_filter.admits(event.kind)
                    && self.activity_scope.admits(event, &self.agents)
                    && (query.is_empty()
                        || format!("{} {} {}", event.agent_id, event.title, event.summary)
                            .to_lowercase()
                            .contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn activity_pos(&self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            self.activity_cursor.min(len - 1)
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let running = self.totals.running;
        let extra = match (self.agents.len(), running) {
            (0, _) => "none yet".to_owned(),
            (n, 0) => format!("{n} total"),
            (n, r) => format!("{n} total · {r} running"),
        };
        // The panel's inner area starts one cell inside the border, unless the box is too small.
        let area = frame.area();
        let inset = u16::from(area.width >= 5 && area.height >= 5);
        self.origin.set((area.x + inset, area.y + inset));
        panel(
            frame,
            ctx.theme,
            Section::Sandbox,
            "Workers",
            &extra,
            |w, h| self.compose(w, h, ctx),
        );
    }

    fn compose(&self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let wide = w >= SPLIT_AT;
        self.wide.set(wide);

        let mut out = vec![self.tabs_line(w, theme)];
        // Tabs, prompt line, footer hint; the body gets what remains.
        let mut body_h = h.saturating_sub(3);
        if !self.routes.is_empty() && body_h > 8 {
            out.push(self.routes_line(w, theme));
            body_h -= 1;
        }
        self.body_top.set(out.len() as u16);
        self.body_h.set(body_h as u16);
        self.roster_w.set(0);
        let body = match self.tab {
            Tab::Activity => self.activity_lines(w, body_h, theme),
            Tab::Agents if wide => self.split_lines(w, body_h, theme),
            Tab::Agents if self.inspector => self.inspector_lines(w, body_h, theme),
            Tab::Agents => {
                self.roster_w.set(w as u16);
                self.roster_lines(w, body_h, theme)
            }
        };
        out.extend(body.into_iter().take(body_h));
        while out.len() < h.saturating_sub(2) {
            out.push(Line::default());
        }
        out.push(self.prompt_line(w, theme));
        out.push(hint_line(theme, &self.footer(ctx), w));
        out
    }

    fn tabs_line(&self, w: usize, theme: &Theme) -> Line<'static> {
        let tab = |label: &str, active: bool| {
            Span::styled(format!(" {label} "), chrome::tab_style(theme, active))
        };
        let t = &self.totals;
        let tokens = t.tokens_in + t.tokens_out;
        let mut parts = vec![
            format!("{} workers", t.total),
            format!("{} running", t.running),
        ];
        if t.cost_usd > 0.0 {
            parts.push(money(t.cost_usd));
        }
        if tokens > 0 {
            parts.push(format!("{} tok", compact(tokens)));
        }
        if t.requests > 0 {
            parts.push(format!("{} req", t.requests));
        }
        if t.tool_calls > 0 {
            parts.push(format!("{} tools", t.tool_calls));
        }
        if t.agent_ms > 0 {
            parts.push(duration(t.agent_ms));
        }
        let totals = text::clip(&parts.join(" · "), w.saturating_sub(24));
        filled(
            vec![
                tab("1 Agents", self.tab == Tab::Agents),
                tab("2 Activity", self.tab == Tab::Activity),
                Span::styled(format!("  {totals}"), theme.fg(Tok::Dim)),
            ],
            w,
            Style::default(),
        )
    }

    fn routes_line(&self, w: usize, theme: &Theme) -> Line<'static> {
        let joined = self
            .routes
            .iter()
            .map(|route| text::one_line(route).trim().to_owned())
            .collect::<Vec<_>>()
            .join(" · ");
        filled(
            vec![Span::styled(
                format!(" roles: {}", text::clip(&joined, w.saturating_sub(8))),
                theme.fg(Tok::Dim),
            )],
            w,
            Style::default(),
        )
    }

    fn prompt_line(&self, w: usize, theme: &Theme) -> Line<'static> {
        let (text_, tok) = match (&self.compose, &self.search, &self.message) {
            (Some(draft), _, _) => (format!(" message> {draft}"), Tok::Accent),
            (None, Some(draft), _) => (format!(" / {draft}"), Tok::Accent),
            (None, None, Some((message, true))) => (format!(" {message}"), Tok::Error),
            (None, None, Some((message, false))) => (format!(" {message}"), Tok::Muted),
            (None, None, None) => {
                let query = match self.tab {
                    Tab::Agents => &self.agent_query,
                    Tab::Activity => &self.activity_query,
                };
                if query.is_empty() {
                    return Line::default();
                }
                (format!(" filter: {query}  · Esc clear search"), Tok::Dim)
            }
        };
        filled(
            vec![Span::styled(text::clip(&text_, w), theme.fg(tok))],
            w,
            Style::default(),
        )
    }

    fn footer(&self, ctx: &DrawCtx) -> Vec<String> {
        let mut segments: Vec<String> = match self.tab {
            Tab::Agents => {
                let mut s = vec![
                    "↑↓ select".to_owned(),
                    "Enter open".to_owned(),
                    "x cancel".to_owned(),
                    "m message".to_owned(),
                    "t tree".to_owned(),
                    "/ search".to_owned(),
                    "r roles".to_owned(),
                ];
                if !self.wide.get() {
                    s.push("Tab inspector".to_owned());
                }
                s
            }
            Tab::Activity => vec![
                "↑↓ select".to_owned(),
                "Enter open".to_owned(),
                "f filter".to_owned(),
                "s scope".to_owned(),
                "Space follow".to_owned(),
                "/ search".to_owned(),
            ],
        };
        segments.push(format!(
            "1/2 {} tabs",
            pair_label(ctx.keys, Action::TabPrev, Action::TabNext)
        ));
        let searching = match self.tab {
            Tab::Agents => !self.agent_query.is_empty(),
            Tab::Activity => !self.activity_query.is_empty(),
        };
        let esc = if searching {
            "clear search"
        } else if self.inspector {
            "back"
        } else {
            "close"
        };
        segments.push(hint(ctx.keys, Action::SelectCancel, esc));
        segments
    }

    fn split_lines(&self, w: usize, h: usize, theme: &Theme) -> Vec<Line<'static>> {
        let left_w = (w * 45 / 100).clamp(30, w.saturating_sub(31));
        let right_w = w.saturating_sub(left_w + 1);
        self.roster_w.set(left_w as u16);
        let left = self.roster_lines(left_w, h, theme);
        let right = self.inspector_lines(right_w, h, theme);
        (0..h)
            .map(|i| {
                let mut spans = match left.get(i) {
                    Some(line) => line.spans.clone(),
                    None => filled(Vec::new(), left_w, Style::default()).spans,
                };
                spans.push(Span::styled("│", theme.fg(Tok::BorderMuted)));
                match right.get(i) {
                    Some(line) => spans.extend(line.spans.clone()),
                    None => spans.extend(filled(Vec::new(), right_w, Style::default()).spans),
                }
                Line::from(spans)
            })
            .collect()
    }

    fn roster_lines(&self, w: usize, h: usize, theme: &Theme) -> Vec<Line<'static>> {
        let rows = self.rows();
        if rows.is_empty() {
            let message = if self.agents.is_empty() {
                "No workers yet. Background jobs and task workers appear here.".to_owned()
            } else {
                format!("No workers match \"{}\".", self.agent_query)
            };
            return text::wrap_words(&message, w.saturating_sub(2))
                .into_iter()
                .map(|line| {
                    filled(
                        vec![Span::styled(format!(" {line}"), theme.fg(Tok::Dim))],
                        w,
                        Style::default(),
                    )
                })
                .collect();
        }
        let page = (h / CARD_LINES).max(1);
        let sel = rows
            .iter()
            .position(|row| self.is_selected(row.index))
            .unwrap_or(0);
        let start = if sel >= page { sel + 1 - page } else { 0 };
        self.roster_start.set(start);
        let mut out = Vec::new();
        for row in rows.iter().skip(start).take(page) {
            out.extend(self.roster_card(*row, w, theme));
        }
        out
    }

    fn roster_card(&self, row: Row, w: usize, theme: &Theme) -> Vec<Line<'static>> {
        let agent = &self.agents[row.index];
        let selected = self.is_selected(row.index);
        let (glyph, glyph_tok) = status_glyph(agent.status, theme);
        let pointer = if selected {
            theme.sym(Sym::Pointer).to_string()
        } else {
            " ".to_owned()
        };
        let rail = if self.tree && row.depth > 0 {
            format!(
                "{}{}",
                "  ".repeat(row.depth - 1),
                if row.last { "└─ " } else { "├─ " }
            )
        } else {
            String::new()
        };
        let name_tok = if selected { Tok::Accent } else { Tok::Text };
        let mut left = vec![
            Span::styled(format!("{pointer} "), theme.fg(Tok::Accent)),
            Span::styled(format!("{glyph} "), theme.fg(glyph_tok)),
            Span::styled(rail, theme.fg(Tok::BorderMuted)),
            Span::styled(
                text::clip(&agent.name, (w / 2).max(8)),
                theme.fg(name_tok).add_modifier(Modifier::BOLD),
            ),
        ];
        if !self.tree
            && let Some(parent) = &agent.parent
        {
            left.push(Span::styled(
                format!("  ↳ {}", text::clip(&self.name_of(parent), 16)),
                theme.fg(Tok::Dim),
            ));
        }
        if agent.unread > 0 {
            left.push(Span::styled(
                format!("  ✉ {}", agent.unread),
                theme.fg(Tok::Accent),
            ));
        }
        let badge = badge_of(agent);
        let left_w: usize = left.iter().map(|s| text::width(&s.content)).sum();
        let badge = text::clip(&badge, w.saturating_sub(left_w + 2));
        let gap = w.saturating_sub(left_w + text::width(&badge) + 1);
        left.push(Span::raw(" ".repeat(gap)));
        left.push(Span::styled(badge, theme.fg(Tok::Dim)));
        let header = filled(left, w, Style::default());

        let task = if agent.task.trim().is_empty() {
            agent.agent.clone()
        } else {
            text::one_line(&agent.task)
        };
        let task = filled(
            vec![Span::styled(
                format!("   {}", text::clip(&task, w.saturating_sub(3))),
                theme.fg(Tok::Muted),
            )],
            w,
            Style::default(),
        );

        let detail = match (&agent.status, &agent.current, &agent.retry) {
            (AgentStatus::Running, Some(current), retry) => {
                let mut d = format!("▸ {current}");
                if let Some((n, max)) = retry {
                    d.push_str(&format!(" · retry {n}/{max}"));
                }
                d
            }
            _ => metrics(agent, epoch_ms()).join(" · "),
        };
        let detail = filled(
            vec![Span::styled(
                format!("   {}", text::clip(&detail, w.saturating_sub(3))),
                theme.fg(Tok::Dim),
            )],
            w,
            Style::default(),
        );
        vec![header, task, detail]
    }

    fn inspector_lines(&self, w: usize, h: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(agent) = self.selected_agent() else {
            return vec![filled(
                vec![Span::styled(
                    " Select a worker to inspect it.",
                    theme.fg(Tok::Dim),
                )],
                w,
                Style::default(),
            )];
        };
        let now = epoch_ms();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let (glyph, glyph_tok) = status_glyph(agent.status, theme);
        lines.push(filled(
            vec![
                Span::styled(
                    format!(" {}", text::clip(&agent.name, w.saturating_sub(12))),
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {}", agent.id), theme.fg(Tok::Dim)),
            ],
            w,
            Style::default(),
        ));
        lines.push(filled(
            vec![
                Span::styled(format!(" {glyph} "), theme.fg(glyph_tok)),
                Span::styled(agent.status.label().to_owned(), theme.fg(glyph_tok)),
            ],
            w,
            Style::default(),
        ));
        lines.push(filled(
            vec![Span::styled(
                format!(" {}", text::clip(&badge_of(agent), w.saturating_sub(1))),
                theme.fg(Tok::Muted),
            )],
            w,
            Style::default(),
        ));

        lines.push(heading("Task", w, theme));
        let task = text::one_line(&agent.task);
        if task.trim().is_empty() {
            lines.push(field(" —", w, theme));
        } else {
            for line in text::wrap_words(&task, w.saturating_sub(2).max(1)) {
                lines.push(field(&format!(" {line}"), w, theme));
            }
        }

        lines.push(heading("Current", w, theme));
        lines.extend(kv_wrapped(
            "tool",
            agent.current.as_deref().unwrap_or("idle"),
            w,
            theme,
        ));
        if let Some((n, max)) = agent.retry {
            lines.push(kv("retry", &format!("attempt {n}/{max}"), w, theme));
        }

        lines.push(heading("Usage", w, theme));
        lines.push(kv("steps", &agent.steps.to_string(), w, theme));
        lines.push(kv("requests", &agent.requests.to_string(), w, theme));
        lines.push(kv("tool calls", &agent.tool_calls.to_string(), w, theme));
        let tokens = match (agent.tokens_in, agent.tokens_out) {
            (None, None) => "unknown".to_owned(),
            (i, o) => format!(
                "in {} · out {}",
                i.map_or("?".into(), compact),
                o.map_or("?".into(), compact)
            ),
        };
        lines.push(kv("tokens", &tokens, w, theme));
        lines.push(kv(
            "cost",
            &agent.cost_usd.map_or("unknown".into(), money),
            w,
            theme,
        ));
        lines.push(kv(
            "elapsed",
            &agent.elapsed_ms.map_or("unknown".into(), duration),
            w,
            theme,
        ));
        if let Some((used, window)) = agent.context.filter(|(_, window)| *window > 0) {
            lines.push(kv("context", &gauge(used, window, w), w, theme));
        }

        lines.push(heading("Lineage", w, theme));
        let parent = agent
            .parent
            .as_ref()
            .map_or("main".to_owned(), |id| self.name_of(id));
        lines.push(kv("parent", &parent, w, theme));
        lines.push(kv(
            "transcript",
            &agent
                .transcript
                .as_ref()
                .and_then(|p| p.file_name())
                .map_or("none".into(), |f| f.to_string_lossy().into_owned()),
            w,
            theme,
        ));

        lines.push(heading("Changes & output", w, theme));
        if agent.artifacts.is_empty() {
            lines.push(kv("artifacts", "none yet", w, theme));
        }
        for (label, path) in &agent.artifacts {
            lines.push(kv(label, path, w, theme));
        }
        lines.push(kv(
            "isolation",
            agent.isolation.as_deref().unwrap_or("none"),
            w,
            theme,
        ));

        lines.push(heading("Recent activity", w, theme));
        let recent: Vec<&ActivityEvent> = self
            .activity
            .iter()
            .filter(|event| event.agent_id == agent.id)
            .collect();
        if recent.is_empty() {
            lines.push(field(" nothing recorded yet", w, theme));
        }
        for event in recent.iter().rev().take(RECENT_ACTIVITY).rev() {
            let line = format!(
                " {:>4} {} {}",
                format!("{} ago", ago(event.at_ms, now)),
                kind_glyph(event.kind, theme),
                event.title
            );
            lines.push(field(&line, w, theme));
        }

        let max = lines.len().saturating_sub(h);
        let start = self.inspector_scroll.min(max);
        lines.into_iter().skip(start).collect()
    }

    fn activity_lines(&self, w: usize, h: usize, theme: &Theme) -> Vec<Line<'static>> {
        let rows = self.activity_rows();
        let pos = self.activity_pos(rows.len());
        let status = format!(
            " filter {} · scope {} · follow {} · {} events",
            self.activity_filter.label(),
            self.activity_scope.label(),
            if self.follow { "on" } else { "off" },
            rows.len()
        );
        let mut out = vec![filled(
            vec![Span::styled(text::clip(&status, w), theme.fg(Tok::Muted))],
            w,
            Style::default(),
        )];
        if rows.is_empty() {
            out.extend(
                text::wrap_words(
                    "No activity matches. Responses, tool calls, errors, and messages appear here.",
                    w.saturating_sub(2),
                )
                .into_iter()
                .map(|line| {
                    filled(
                        vec![Span::styled(format!(" {line}"), theme.fg(Tok::Dim))],
                        w,
                        Style::default(),
                    )
                }),
            );
            return out;
        }
        let now = epoch_ms();
        let agent_w = 12;
        let title_w = 20;
        let summary_w = w.saturating_sub(2 + 8 + agent_w + 2 + title_w + 1);
        out.push(filled(
            vec![Span::styled(
                format!(
                    "  {}{}{}  {}",
                    cell("time", 8),
                    cell("agent", agent_w),
                    cell("", 2),
                    cell("event", title_w + summary_w)
                ),
                theme.fg(Tok::Dim),
            )],
            w,
            Style::default(),
        ));
        let page = h.saturating_sub(2).max(1);
        let start = if pos >= page { pos + 1 - page } else { 0 };
        self.activity_start.set(start);
        for (k, &index) in rows.iter().enumerate().skip(start).take(page) {
            let event = &self.activity[index];
            let selected = k == pos;
            let pointer = if selected {
                theme.sym(Sym::Pointer).to_string()
            } else {
                " ".to_owned()
            };
            out.push(filled(
                vec![
                    Span::styled(format!("{pointer} "), theme.fg(Tok::Accent)),
                    Span::styled(cell(&ago(event.at_ms, now), 8), theme.fg(Tok::Dim)),
                    Span::styled(
                        cell(&self.name_of(&event.agent_id), agent_w),
                        theme.fg(Tok::Text),
                    ),
                    Span::styled(
                        format!("{} ", kind_glyph(event.kind, theme)),
                        theme.fg(kind_tok(event.kind)),
                    ),
                    Span::styled(
                        cell(&event.title, title_w),
                        theme.fg(kind_tok(event.kind)).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(cell(&event.summary, summary_w), theme.fg(Tok::Muted)),
                ],
                w,
                Style::default(),
            ));
        }
        out
    }
}

fn plain(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn status_glyph(status: AgentStatus, theme: &Theme) -> (String, Tok) {
    match status {
        AgentStatus::Running => (theme.sym(Sym::Working).to_string(), Tok::Accent),
        AgentStatus::Waiting => (theme.sym(Sym::Warn).to_string(), Tok::Warning),
        AgentStatus::Done => (theme.sym(Sym::Check).to_string(), Tok::Success),
        AgentStatus::Failed => (theme.sym(Sym::Cross2).to_string(), Tok::Error),
        AgentStatus::Cancelled => (theme.sym(Sym::Dot).to_string(), Tok::Dim),
    }
}

fn kind_glyph(kind: ActivityKind, theme: &Theme) -> String {
    let sym = match kind {
        ActivityKind::Response => Sym::Bullet,
        ActivityKind::Tool => Sym::Delta,
        ActivityKind::Error => Sym::Cross2,
        ActivityKind::Message => Sym::Quote,
        ActivityKind::Lifecycle => Sym::Dot,
    };
    theme.sym(sym).to_string()
}

fn kind_tok(kind: ActivityKind) -> Tok {
    match kind {
        ActivityKind::Response => Tok::Text,
        ActivityKind::Tool => Tok::Accent,
        ActivityKind::Error => Tok::Error,
        ActivityKind::Message => Tok::Warning,
        ActivityKind::Lifecycle => Tok::Dim,
    }
}

fn badge_of(agent: &AgentEntry) -> String {
    let role = if agent.role.is_empty() || agent.role == "unknown" {
        None
    } else {
        Some(agent.role.as_str())
    };
    let model = if agent.model.is_empty() || agent.model == "unknown" {
        None
    } else {
        Some(agent.model.as_str())
    };
    match (role, model) {
        (Some(role), Some(model)) => format!("{role} · {model}"),
        (Some(role), None) => role.to_owned(),
        (None, Some(model)) => model.to_owned(),
        (None, None) => agent.agent.clone(),
    }
}

/// Muted metrics line: cost, time, requests, tools, tokens, context, and age.
fn metrics(agent: &AgentEntry, now: u64) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(cost) = agent.cost_usd {
        parts.push(money(cost));
    }
    if let Some(ms) = agent.elapsed_ms {
        parts.push(duration(ms));
    }
    if agent.requests > 0 {
        parts.push(format!("{} req", agent.requests));
    }
    if agent.tool_calls > 0 {
        parts.push(format!("{} tools", agent.tool_calls));
    }
    match (agent.tokens_in, agent.tokens_out) {
        (None, None) => {}
        (i, o) => parts.push(format!("{} tok", compact(i.unwrap_or(0) + o.unwrap_or(0)))),
    }
    if let Some((used, window)) = agent.context.filter(|(_, window)| *window > 0) {
        parts.push(format!("ctx {}%", used * 100 / window));
    }
    if let Some(last) = agent.last_activity_ms {
        parts.push(format!("active {} ago", ago(last, now)));
    }
    if parts.is_empty() {
        parts.push(agent.status.label().to_owned());
    }
    parts
}

fn heading(title: &str, w: usize, theme: &Theme) -> Line<'static> {
    let label = format!(" {title} ");
    let fill = w.saturating_sub(text::width(&label) + 1);
    filled(
        vec![
            Span::styled(label, theme.fg(Tok::Muted).add_modifier(Modifier::BOLD)),
            Span::styled(
                theme.sym(Sym::BoxH).repeat(fill),
                theme.fg(Tok::BorderMuted),
            ),
        ],
        w,
        Style::default(),
    )
}

fn field(value: &str, w: usize, theme: &Theme) -> Line<'static> {
    filled(
        vec![Span::styled(text::clip(value, w), theme.fg(Tok::Text))],
        w,
        Style::default(),
    )
}

fn kv(label: &str, value: &str, w: usize, theme: &Theme) -> Line<'static> {
    let label_w = 11;
    let value = text::clip(&text::one_line(value), w.saturating_sub(label_w + 1));
    filled(
        vec![
            Span::styled(cell(&format!(" {label}"), label_w), theme.fg(Tok::Dim)),
            Span::styled(value, theme.fg(Tok::Text)),
        ],
        w,
        Style::default(),
    )
}

/// Like `kv`, but long values wrap onto continuation rows aligned under the value column.
fn kv_wrapped(label: &str, value: &str, w: usize, theme: &Theme) -> Vec<Line<'static>> {
    let label_w = 11;
    let rows = text::wrap_words(&text::one_line(value), w.saturating_sub(label_w + 1).max(1));
    if rows.len() <= 1 {
        return vec![kv(label, value, w, theme)];
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            if i == 0 {
                kv(label, row, w, theme)
            } else {
                filled(
                    vec![Span::styled(
                        format!("{}{row}", " ".repeat(label_w)),
                        theme.fg(Tok::Text),
                    )],
                    w,
                    Style::default(),
                )
            }
        })
        .collect()
}

fn gauge(used: u64, window: u64, w: usize) -> String {
    let pct = (used * 100 / window.max(1)).min(100);
    let cells = 12usize.min(w.saturating_sub(24).max(4));
    let filled_cells = (pct as usize * cells / 100).min(cells);
    format!(
        "{}{} {pct}% {}/{}",
        "█".repeat(filled_cells),
        "░".repeat(cells - filled_cells),
        compact(used),
        compact(window)
    )
}

/// Fixed-width table cell: clipped and padded to `n` columns.
fn cell(value: &str, n: usize) -> String {
    let clipped = text::clip(value, n);
    let pad = n.saturating_sub(text::width(&clipped));
    format!("{clipped}{}", " ".repeat(pad))
}

fn ago(at_ms: u64, now: u64) -> String {
    let secs = now.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn money(usd: f64) -> String {
    if usd >= 0.01 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests;
