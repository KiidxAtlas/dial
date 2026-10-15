//! The `/git [revision]` panel: a diff viewer with a minimap and four view modes, a sidebar of
//! staged, unstaged and untracked files with a commit composer, and a read-only view of one commit.
//! Staging, hunk edits, discards and commits run git in the project root.

mod help;
mod image;
mod lines;
mod model;
mod ops;
mod render;
mod tree;
mod view;

use super::{DrawCtx, OverlayEvent, settings::pressed};
use crate::{
    git_panel::{AiSummary, Generated, Repo, Watch},
    ui::{
        chrome::{self, Section},
        keys::{Action, Keymap},
        text,
    },
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use model::*;
use ratatui::{Frame, widgets::Paragraph};
use render::plain_char;
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    sync::mpsc::Receiver,
    time::Instant,
};
use tree::Entry;

pub struct GitOverlay {
    repo: Repo,
    commit: Option<Commit>,
    branch: Option<String>,
    /// Short sha and subject of HEAD.
    head: Option<(String, String)>,
    changes: Vec<Change>,
    cursor: usize,
    /// Folded sidebar directories, keyed by `tree::dir_key`.
    collapsed: HashSet<String>,
    shown: Option<Shown>,
    rows: Vec<Row>,
    cursor_row: usize,
    /// Hunk under the diff cursor (or the hunk shown in the hunk view).
    hunk: usize,
    diff_top: Cell<usize>,
    side_top: Cell<usize>,
    body_h: Cell<usize>,
    /// Image placements of the last compose, relative to the panel body. Rebuilt on every draw.
    placements: RefCell<Vec<crate::ui::images::Placement>>,
    hscroll: usize,
    focus: Focus,
    view: View,
    whitespace: view::Whitespace,
    wrap: bool,
    message: String,
    amend: bool,
    editing: bool,
    /// A discard waiting for a second press of `delete`.
    confirm: Option<Change>,
    status: Option<(String, bool, Instant)>,
    synced: Instant,
    /// Marked lines of the diff; see `lines`.
    line_sel: Option<lines::LineSel>,
    /// The `?` keyboard sheet replaces the panel body while set.
    help: bool,
    /// Working-tree fs watch; `None` for a pinned revision or when the watch failed.
    watch: Option<Watch>,
    /// Why the watch is unavailable; the panel then polls every `REFRESH`.
    watch_error: Option<String>,
    /// Inner panel size from the last draw, for mouse hit tests.
    area: Cell<(usize, usize)>,
    /// Result of a running commit-message generation, until it is applied.
    generating: Option<Receiver<Result<Generated, String>>>,
    /// Result of a running AI staging pass, until it is applied.
    ai_job: Option<Receiver<Result<AiSummary, String>>>,
    /// The instruction typed for AI staging (`A`); `None` when the prompt is closed.
    ai_prompt: Option<String>,
}

impl GitOverlay {
    /// Opens the panel on the working tree, or on `revision` (read-only) when one is given.
    pub(crate) fn open(repo: Repo, revision: Option<&str>) -> Result<Self, String> {
        let (commit, mut changes) = match revision {
            None => (None, repo.status()?),
            Some(rev) => {
                let sha = repo.resolve(rev)?;
                let commit = repo.commit_info(&sha)?;
                let files = repo.commit_files(&sha)?;
                (Some(commit), files)
            }
        };
        sort_changes(&mut changes);
        let (watch, watch_error) = if commit.is_none() {
            match repo.watch() {
                Ok(watch) => (Some(watch), None),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, None)
        };
        let mut overlay = Self {
            repo,
            commit,
            branch: None,
            head: None,
            changes,
            cursor: 0,
            collapsed: HashSet::new(),
            shown: None,
            rows: Vec::new(),
            cursor_row: 0,
            hunk: 0,
            diff_top: Cell::new(0),
            side_top: Cell::new(0),
            body_h: Cell::new(1),
            placements: RefCell::new(Vec::new()),
            hscroll: 0,
            focus: Focus::Sidebar,
            view: View::Inline,
            whitespace: view::Whitespace::Off,
            wrap: false,
            message: String::new(),
            amend: false,
            editing: false,
            confirm: None,
            status: None,
            synced: Instant::now(),
            line_sel: None,
            help: false,
            watch,
            watch_error,
            area: Cell::new((0, 0)),
            generating: None,
            ai_job: None,
            ai_prompt: None,
        };
        overlay.refresh_meta();
        overlay.cursor = overlay.first_file_row();
        overlay.load_shown();
        if let Some(error) = overlay.watch_error.clone() {
            overlay.flash(Self::watch_notice(&error), true);
        }
        Ok(overlay)
    }

    /// Refreshes when the fs watch reports a debounced change. Without a watch (it failed to
    /// start) the panel re-reads every `REFRESH` and keeps the fallback notice in the banner.
    pub fn tick(&mut self) {
        self.poll_jobs();
        let due = match self.watch.as_mut() {
            Some(watch) => watch.take_ready(Instant::now()),
            None => self.watch_error.is_some() && self.synced.elapsed() >= REFRESH,
        };
        if !due {
            return;
        }
        self.sync();
        if let Some(error) = self.watch_error.clone() {
            self.flash(Self::watch_notice(&error), true);
        }
    }

    fn watch_notice(error: &str) -> String {
        format!(
            "fs watch unavailable, polling every {}s: {error}",
            REFRESH.as_secs()
        )
    }

    /// Mouse input. Geometry mirrors `compose`: the border takes row 0 and column 0, compose's
    /// rows start at row 1 (the diff header), and the sidebar starts after the diff column and a
    /// one-cell separator.
    fn mouse(&mut self, event: MouseEvent) {
        let (width, _) = self.area.get();
        let (Some(x), Some(y)) = (
            usize::from(event.column).checked_sub(1),
            usize::from(event.row).checked_sub(1),
        ) else {
            return;
        };
        if y >= self.body_h.get() {
            return;
        }
        let side_w = if width >= view::NARROW_COLS {
            (width * 3 / 10).clamp(26, 48)
        } else {
            0
        };
        let diff_w = width - side_w - usize::from(side_w > 0);
        let in_sidebar = side_w > 0 && x > diff_w;
        let in_diff = x < diff_w;
        match event.kind {
            MouseEventKind::ScrollUp if in_sidebar => self.select_file(-1),
            MouseEventKind::ScrollDown if in_sidebar => self.select_file(1),
            MouseEventKind::ScrollUp if in_diff => self.scroll_diff(-3),
            MouseEventKind::ScrollDown if in_diff => self.scroll_diff(3),
            MouseEventKind::Down(MouseButton::Left) if in_sidebar => self.focus = Focus::Sidebar,
            MouseEventKind::Down(MouseButton::Left) if in_diff => {
                self.focus = Focus::Diff;
                if diff_w >= 40 && x + 2 >= diff_w {
                    self.seek(y);
                } else {
                    self.cursor_to(self.diff_top.get() + y);
                }
            }
            _ => {}
        }
    }

    /// Wheel over the diff: moves the viewport, and the cursor when it would leave the view.
    fn scroll_diff(&mut self, delta: isize) {
        let body = self.body_h.get().max(1);
        let max_top = self.rows.len().saturating_sub(body) as isize;
        let top = (self.diff_top.get() as isize + delta).clamp(0, max_top) as usize;
        self.diff_top.set(top);
        let row = self.cursor_row.clamp(top, top + body - 1);
        if row != self.cursor_row {
            self.cursor_to(row);
        }
    }

    /// Minimap press on body row `y`: seeks proportionally through the diff.
    fn seek(&mut self, y: usize) {
        let body = self.body_h.get().max(1);
        self.cursor_to((2 * y + 1) * self.rows.len() / (2 * body));
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        match event {
            Event::Paste(value) if self.editing => {
                self.message
                    .push_str(&value.replace("\r\n", "\n").replace('\r', "\n"));
                OverlayEvent::None
            }
            Event::Key(key) if pressed(key) => self.key(key, keys),
            Event::Mouse(mouse) if !self.help => {
                self.mouse(*mouse);
                OverlayEvent::None
            }
            _ => OverlayEvent::None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        self.placements.borrow_mut().clear();
        let area = frame.area();
        if self.help {
            let block =
                chrome::panel_block_titled(ctx.theme, Section::Tools, "Git", "keyboard shortcuts");
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(Paragraph::new(help::lines(ctx.theme)), inner);
            return;
        }
        let extra = match &self.commit {
            Some(commit) => format!("commit {}", commit.short),
            None => self.branch.clone().unwrap_or_else(|| "detached".to_owned()),
        };
        let block = chrome::panel_block_titled(ctx.theme, Section::Tools, "Git", &extra);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.area
            .set((usize::from(inner.width), usize::from(inner.height)));
        let lines = self.compose(inner.width, inner.height, ctx);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Native image placements for the frame just drawn, in frame cell coordinates. Compose
    /// positions them relative to its own rows; the border and the one-cell inset add the offset.
    pub fn native_placements(&self) -> Vec<crate::ui::images::Placement> {
        self.placements
            .borrow()
            .iter()
            .map(|placement| crate::ui::images::Placement {
                x: placement.x + 1,
                y: placement.y + 1,
                image: placement.image.clone(),
            })
            .collect()
    }

    fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        if self.help {
            if keys.matches(Action::SelectCancel, key) || plain_char(key) == Some('?') {
                self.help = false;
            }
            return OverlayEvent::None;
        }
        if self.editing {
            return self.edit_key(key, keys);
        }
        if self.ai_prompt.is_some() {
            return self.ai_prompt_key(key, keys);
        }
        let pending = self.confirm.take();
        if key.code == KeyCode::Delete {
            self.discard_request(pending);
            return OverlayEvent::None;
        }
        let ch = plain_char(key);
        if self.focus == Focus::Diff
            && keys.matches(Action::SelectCancel, key)
            && self.clear_line_selection()
        {
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) || ch == Some('q') {
            return OverlayEvent::Close;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Up => self.jump_hunk(-1),
                KeyCode::Down => self.jump_hunk(1),
                _ => {}
            }
            return OverlayEvent::None;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_focus();
            return OverlayEvent::None;
        }
        match ch {
            Some('?') => self.help = true,
            Some(']') => self.select_file(1),
            Some('[') => self.select_file(-1),
            Some('v') => {
                let next = match self.view {
                    View::File => View::Split,
                    View::Split => View::Inline,
                    View::Inline => View::Hunk,
                    View::Hunk => View::File,
                };
                self.set_view(next);
            }
            Some('1') => self.set_view(View::File),
            Some('2') => self.set_view(View::Split),
            Some('3') => self.set_view(View::Inline),
            Some('4') => self.set_view(View::Hunk),
            Some('b') => {
                self.whitespace = self.whitespace.next();
                let label = self.whitespace.label();
                self.flash(label, false);
                self.load_shown();
            }
            Some('w') => {
                self.wrap = !self.wrap;
                self.hscroll = 0;
            }
            Some('r') => {
                self.sync();
                self.flash("Refreshed", false);
            }
            Some('c') if self.commit.is_none() => {
                self.editing = true;
                self.focus = Focus::Sidebar;
            }
            _ => match self.focus {
                Focus::Diff => self.diff_key(key, ch),
                Focus::Sidebar => self.sidebar_key(key, ch),
            },
        }
        OverlayEvent::None
    }

    fn edit_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        if keys.matches(Action::SelectCancel, key) {
            self.editing = false;
            return OverlayEvent::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => self.commit(),
                KeyCode::Char('t') => self.amend = !self.amend,
                KeyCode::Char('g') => self.generate_message(),
                _ => {}
            }
            return OverlayEvent::None;
        }
        match key.code {
            KeyCode::Enter => self.message.push('\n'),
            KeyCode::Backspace => {
                self.message.pop();
            }
            KeyCode::Tab => {
                self.editing = false;
                self.focus = Focus::Diff;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => self.message.push(c),
            _ => {}
        }
        OverlayEvent::None
    }

    fn ai_prompt_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        if keys.matches(Action::SelectCancel, key) {
            self.ai_prompt = None;
            return OverlayEvent::None;
        }
        match key.code {
            KeyCode::Enter => {
                let instruction = self.ai_prompt.take().unwrap_or_default();
                let instruction = instruction.trim().to_owned();
                if instruction.is_empty() {
                    self.flash("Describe what to stage", true);
                } else {
                    self.start_ai_stage(instruction);
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.ai_prompt.as_mut() {
                    prompt.pop();
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(prompt) = self.ai_prompt.as_mut() {
                    prompt.push(c);
                }
            }
            _ => {}
        }
        let shown = self
            .ai_prompt
            .as_ref()
            .map(|prompt| format!("Stage what? {prompt}_"));
        if let Some(shown) = shown {
            self.flash(shown, false);
        }
        OverlayEvent::None
    }

    fn diff_key(&mut self, key: &KeyEvent, ch: Option<char>) {
        if self.line_key(key, ch) {
            return;
        }
        let page = self.body_h.get().saturating_sub(2).max(1) as isize;
        match (key.code, ch) {
            (KeyCode::Up, _) | (_, Some('k')) => self.move_cursor(-1),
            (KeyCode::Down, _) | (_, Some('j')) => self.move_cursor(1),
            (KeyCode::PageUp, _) => self.move_cursor(-page),
            (KeyCode::PageDown, _) => self.move_cursor(page),
            (KeyCode::Home, _) | (_, Some('g')) => self.cursor_to(0),
            (KeyCode::End, _) | (_, Some('G')) => self.cursor_to(usize::MAX),
            (KeyCode::Left, _) | (_, Some('h')) if !self.wrap => {
                self.hscroll = self.hscroll.saturating_sub(8);
            }
            (KeyCode::Right, _) | (_, Some('l')) if !self.wrap => self.hscroll += 8,
            (KeyCode::Enter, _) => self.jump_hunk(1),
            (KeyCode::Char(' '), _) | (_, Some('s')) | (_, Some('u')) => self.stage_current(),
            (_, Some('x')) => self.discard_hunk(),
            _ => {}
        }
    }

    fn sidebar_key(&mut self, key: &KeyEvent, ch: Option<char>) {
        match (key.code, ch) {
            (KeyCode::Up, _) | (_, Some('k')) => self.move_side(-1),
            (KeyCode::Down, _) | (_, Some('j')) => self.move_side(1),
            (KeyCode::Enter, _) => self.side_enter(),
            (KeyCode::Left, _) | (_, Some('h')) => self.side_left(),
            (KeyCode::Right, _) | (_, Some('l')) => self.side_right(),
            (KeyCode::Char(' '), _) => self.toggle_file(),
            (_, Some('S')) => self.stage_all(),
            (_, Some('U')) => self.unstage_all(),
            (_, Some('A')) => self.open_ai_prompt(),
            _ => {}
        }
    }

    fn selected(&self) -> Option<&Change> {
        match self.entries().get(self.cursor) {
            Some(Entry::File { index, .. }) => self.changes.get(*index),
            _ => None,
        }
    }

    fn flash(&mut self, message: impl Into<String>, error: bool) {
        let message: String = message.into();
        self.status = Some((text::one_line(&message), error, Instant::now()));
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Diff => Focus::Sidebar,
            Focus::Sidebar => Focus::Diff,
        };
    }

    /// Reloads status and the selected diff. Keeps the selection on the same path when possible.
    fn sync(&mut self) {
        self.synced = Instant::now();
        if self.commit.is_none() {
            let mut next = match self.repo.status() {
                Ok(next) => next,
                Err(error) => {
                    self.flash(error, true);
                    return;
                }
            };
            sort_changes(&mut next);
            if next != self.changes {
                let key = self.selected().cloned();
                self.changes = next;
                let entries = self.entries();
                self.cursor = key
                    .and_then(|k| {
                        entries.iter().position(
                            |entry| matches!(entry, Entry::File { index, .. } if self.changes[*index] == k),
                        )
                    })
                    .unwrap_or_else(|| self.cursor.min(entries.len().saturating_sub(1)));
            }
            self.refresh_meta();
        }
        self.load_shown();
    }

    fn refresh_meta(&mut self) {
        if self.commit.is_some() {
            return;
        }
        let (branch, head) = self.repo.branch_and_head();
        self.branch = branch;
        self.head = head;
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.load_shown();
    }

    fn hunk_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row.kind, RowKind::Hunk(_)))
            .map(|(index, _)| index)
    }

    fn load_shown(&mut self) {
        self.shown = match self.selected().cloned() {
            Some(change) => match self.load_doc(&change) {
                Ok(shown) => Some(shown),
                Err(error) => {
                    self.flash(error, true);
                    None
                }
            },
            None => None,
        };
        self.rebuild_rows();
    }

    fn load_doc(&self, change: &Change) -> Result<Shown, String> {
        let doc = self.git_diff(change, CONTEXT)?;
        let binary = if view::is_binary_doc(&doc) {
            let rev = self.commit.as_ref().map(|commit| commit.sha.as_str());
            let protocol = crate::ui::images::ImageProtocol::detect();
            Some(view::load_binary(&self.repo, change, rev, protocol)?)
        } else {
            None
        };
        let full = if self.view == View::File && binary.is_none() {
            Some(self.git_diff(change, FULL_CONTEXT)?)
        } else {
            None
        };
        Ok(Shown {
            change: change.clone(),
            doc,
            full,
            binary,
        })
    }

    fn git_diff(&self, change: &Change, context: &str) -> Result<Doc, String> {
        if change.kind == Kind::Untracked {
            return self.repo.untracked_doc(&change.path);
        }
        let mut args: Vec<&str> = match &self.commit {
            Some(commit) => vec![
                "show",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--format=",
                context,
                commit.sha.as_str(),
            ],
            None if change.area == Area::Staged => vec![
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                context,
            ],
            None => vec![
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                context,
            ],
        };
        args.extend(self.whitespace.git_args().iter().copied());
        args.push("--");
        args.push(change.path.as_str());
        self.repo.diff(&args)
    }

    fn rebuild_rows(&mut self) {
        let Some(shown) = self.shown.as_ref() else {
            self.rows.clear();
            self.cursor_row = 0;
            return;
        };
        self.hunk = self.hunk.min(shown.doc.hunks.len().saturating_sub(1));
        self.rows = match (&shown.binary, self.view) {
            (Some(binary), _) => view::binary_rows(binary),
            (None, View::File) => layout(
                shown.full.as_ref().unwrap_or(&shown.doc),
                View::Inline,
                None,
            ),
            (None, View::Inline) => layout(&shown.doc, View::Inline, None),
            (None, View::Split) => layout(&shown.doc, View::Split, None),
            (None, View::Hunk) => layout(&shown.doc, View::Inline, Some(self.hunk)),
        };
        self.cursor_row = self.cursor_row.min(self.rows.len().saturating_sub(1));
        self.sync_hunk();
    }

    fn sync_hunk(&mut self) {
        if let Some(hunk) = self.rows.get(self.cursor_row).and_then(|row| row.hunk) {
            self.hunk = hunk;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        self.cursor_row = (self.cursor_row as isize + delta).clamp(0, last) as usize;
        self.sync_hunk();
    }

    fn cursor_to(&mut self, row: usize) {
        self.cursor_row = row.min(self.rows.len().saturating_sub(1));
        self.sync_hunk();
    }

    /// `alt+up/down` and `enter`: next or previous hunk, crossing into the neighbouring file.
    fn jump_hunk(&mut self, delta: isize) {
        if self.view == View::Hunk {
            let count = self.shown.as_ref().map_or(0, |s| s.doc.hunks.len()) as isize;
            let next = self.hunk as isize + delta;
            if (0..count).contains(&next) {
                self.hunk = next as usize;
                self.rebuild_rows();
                self.cursor_row = 0;
            } else {
                self.select_file(delta);
            }
            return;
        }
        let target = if delta > 0 {
            self.rows
                .iter()
                .enumerate()
                .skip(self.cursor_row + 1)
                .find(|(_, row)| matches!(row.kind, RowKind::Hunk(_)))
                .map(|(index, _)| index)
        } else {
            let end = self.cursor_row.min(self.rows.len());
            self.rows[..end]
                .iter()
                .rposition(|row| matches!(row.kind, RowKind::Hunk(_)))
        };
        match target {
            Some(row) => {
                self.cursor_row = row;
                self.sync_hunk();
            }
            None => self.select_file(delta),
        }
    }

    fn hunk_ops_allowed(&self) -> bool {
        self.whitespace.allows_patch()
            && self.shown.as_ref().is_some_and(|shown| {
                !shown.doc.head.iter().any(|line| {
                    line.starts_with("diff --cc") || line.starts_with("diff --combined")
                })
            })
    }
}
