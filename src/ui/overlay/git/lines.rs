//! Line-level staging in the diff. `shift+up/down` extends a range of rows, `m` marks the lines of the
//! cursor row, and while any line is marked `space`, `s`/`u` stage (or unstage) only those lines and `x`
//! discards them from the worktree. `esc` clears the marks. A selection belongs to one hunk of one file
//! and lapses as soon as that hunk changes, leaves the screen, or the view changes.

use super::{GitOverlay, model::*};
use crate::ui::theme::{Theme, Tok};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{style::Modifier, text::Span};
use std::collections::BTreeSet;

/// Marked body lines of one hunk. Body lines are numbered as `line_patch` numbers them: every line of the
/// hunk except `\` markers, in order.
#[derive(Clone, Debug)]
pub(super) struct LineSel {
    change: Change,
    hunk: usize,
    view: View,
    /// The hunk as it was when the selection started. A different hunk text lapses the selection.
    snapshot: Hunk,
    /// Shift-range start, as a row offset from the hunk's first row so it survives row rebuilds.
    anchor: Option<usize>,
    marked: BTreeSet<usize>,
}

/// Changed line of a hunk: (removed side?, line number on that side, body index).
type ChangedKey = (bool, u32, usize);

impl LineSel {
    fn new(change: &Change, hunk: usize, snapshot: &Hunk, view: View) -> Self {
        Self {
            change: change.clone(),
            hunk,
            view,
            snapshot: snapshot.clone(),
            anchor: None,
            marked: BTreeSet::new(),
        }
    }

    /// Marks the given body lines, or unmarks them when all of them are already marked.
    fn toggle(&mut self, bodies: &[usize]) {
        if bodies.is_empty() {
            return;
        }
        if bodies.iter().all(|body| self.marked.contains(body)) {
            for body in bodies {
                self.marked.remove(body);
            }
        } else {
            self.marked.extend(bodies.iter().copied());
        }
    }

    /// Marked lines together with the lines of an open shift-range.
    fn effective(&self, range: &[usize]) -> BTreeSet<usize> {
        let mut out = self.marked.clone();
        out.extend(range.iter().copied());
        out
    }
}

/// Body index of every removed and added line of a hunk, with the number it carries on its side.
fn changed_keys(hunk: &Hunk) -> Vec<ChangedKey> {
    let (mut old, mut new) = hunk_start(&hunk.header);
    let mut body = 0;
    let mut out = Vec::new();
    for line in &hunk.lines {
        match line.as_bytes().first() {
            Some(b'\\') => continue,
            Some(b'-') => {
                out.push((true, old, body));
                old += 1;
            }
            Some(b'+') => {
                out.push((false, new, body));
                new += 1;
            }
            _ => {
                old += 1;
                new += 1;
            }
        }
        body += 1;
    }
    out
}

/// Body indices of the changed lines shown by one row.
fn row_bodies(row: &Row, keys: &[ChangedKey]) -> Vec<usize> {
    let find = |removed: bool, num: u32| {
        keys.iter()
            .find(|key| key.0 == removed && key.1 == num)
            .map(|key| key.2)
    };
    match &row.kind {
        RowKind::Line {
            side: Side::Removed,
            old: Some(num),
            ..
        } => find(true, *num).into_iter().collect(),
        RowKind::Line {
            side: Side::Added,
            new: Some(num),
            ..
        } => find(false, *num).into_iter().collect(),
        RowKind::Pair { left, right } => left
            .iter()
            .filter(|col| col.side == Side::Removed)
            .filter_map(|col| find(true, col.num))
            .chain(
                right
                    .iter()
                    .filter(|col| col.side == Side::Added)
                    .filter_map(|col| find(false, col.num)),
            )
            .collect(),
        _ => Vec::new(),
    }
}

/// Body indices of the changed lines in rows `from..=to` that belong to `hunk`.
fn span_bodies(
    rows: &[Row],
    hunk: usize,
    from: usize,
    to: usize,
    keys: &[ChangedKey],
) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(index, row)| (from..=to).contains(index) && row.hunk == Some(hunk))
        .flat_map(|(_, row)| row_bodies(row, keys))
        .collect()
}

impl GitOverlay {
    /// Line selection applies to a plain text diff in the working tree, not the commit view or whole-file view.
    fn line_ops_allowed(&self) -> bool {
        self.commit.is_none() && self.view != View::File && self.hunk_ops_allowed()
    }

    fn cursor_hunk(&self) -> Option<usize> {
        self.rows.get(self.cursor_row).and_then(|row| row.hunk)
    }

    fn first_row(&self, hunk: usize) -> Option<usize> {
        self.rows.iter().position(|row| row.hunk == Some(hunk))
    }

    /// The selection, while it still matches the shown file, view and hunk text.
    fn live_sel(&self) -> Option<&LineSel> {
        let sel = self.line_sel.as_ref()?;
        let shown = self.shown.as_ref()?;
        let hunk = shown.doc.hunks.get(sel.hunk)?;
        let same = sel.change == shown.change
            && sel.view == self.view
            && self.view != View::File
            && hunk.header == sel.snapshot.header
            && hunk.lines == sel.snapshot.lines;
        same.then_some(sel)
    }

    /// The live selection, when the cursor is on its hunk. Only this selection is shown and acted on.
    fn active_sel(&self) -> Option<&LineSel> {
        let sel = self.live_sel()?;
        (self.cursor_hunk() == Some(sel.hunk)).then_some(sel)
    }

    /// Whether the cursor's hunk has marks or an open range.
    fn selecting(&self) -> bool {
        self.active_sel()
            .is_some_and(|sel| sel.anchor.is_some() || !sel.marked.is_empty())
    }

    /// Makes sure a live selection exists for `hunk`, starting an empty one when the current one belongs
    /// to another hunk or has lapsed. Returns false when the hunk is not in the shown file.
    fn ensure_sel(&mut self, hunk: usize) -> bool {
        if self.live_sel().is_some_and(|sel| sel.hunk == hunk) {
            return true;
        }
        let Some(shown) = self.shown.as_ref() else {
            return false;
        };
        let Some(snapshot) = shown.doc.hunks.get(hunk) else {
            return false;
        };
        self.line_sel = Some(LineSel::new(&shown.change, hunk, snapshot, self.view));
        true
    }

    /// Body indices of the open shift-range, if any.
    fn range_bodies(&self, sel: &LineSel) -> Vec<usize> {
        let (Some(anchor), Some(shown), Some(first)) =
            (sel.anchor, self.shown.as_ref(), self.first_row(sel.hunk))
        else {
            return Vec::new();
        };
        let Some(hunk) = shown.doc.hunks.get(sel.hunk) else {
            return Vec::new();
        };
        let start = first + anchor;
        let keys = changed_keys(hunk);
        span_bodies(
            &self.rows,
            sel.hunk,
            start.min(self.cursor_row),
            start.max(self.cursor_row),
            &keys,
        )
    }

    /// Every body line the selection covers: the marks plus the open range.
    fn selection_bodies(&self, sel: &LineSel) -> BTreeSet<usize> {
        sel.effective(&self.range_bodies(sel))
    }

    /// Body indices of the changed lines on the cursor row.
    fn cursor_bodies(&self, hunk: usize) -> Vec<usize> {
        let (Some(row), Some(shown)) = (self.rows.get(self.cursor_row), self.shown.as_ref()) else {
            return Vec::new();
        };
        match shown.doc.hunks.get(hunk) {
            Some(hunk) => row_bodies(row, &changed_keys(hunk)),
            None => Vec::new(),
        }
    }

    /// Key handling for line selection. Runs before the hunk keys; returns true when it used the key.
    pub(super) fn line_key(&mut self, key: &KeyEvent, ch: Option<char>) -> bool {
        if self.commit.is_some() {
            return false;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let allowed = self.line_ops_allowed();
        match (key.code, ch) {
            (KeyCode::Up | KeyCode::Down, _) if shift && allowed => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.extend_lines(delta);
                true
            }
            (_, Some('m')) if allowed => {
                self.mark_line();
                true
            }
            (KeyCode::Char(' '), _) | (_, Some('s' | 'u')) if allowed && self.selecting() => {
                self.apply_lines(false);
                true
            }
            (_, Some('x')) if allowed && self.selecting() => {
                self.apply_lines(true);
                true
            }
            _ => {
                // Any other key without shift ends a shift-range; the marks stay.
                if !shift && let Some(sel) = self.line_sel.as_mut() {
                    sel.anchor = None;
                }
                false
            }
        }
    }

    /// `esc` in the diff: clears the marks and open range. Returns false when nothing was selected.
    pub(super) fn clear_line_selection(&mut self) -> bool {
        let had = self
            .live_sel()
            .is_some_and(|sel| sel.anchor.is_some() || !sel.marked.is_empty());
        self.line_sel = None;
        had
    }

    /// `shift+up/down`: starts a range at the cursor row when none is open, then moves the cursor.
    fn extend_lines(&mut self, delta: isize) {
        if let Some(hunk) = self.cursor_hunk()
            && self.ensure_sel(hunk)
        {
            let anchor = self
                .cursor_row
                .saturating_sub(self.first_row(hunk).unwrap_or(0));
            if let Some(sel) = self.line_sel.as_mut()
                && sel.anchor.is_none()
            {
                sel.anchor = Some(anchor);
            }
        }
        self.move_cursor(delta);
    }

    /// `m`: marks or unmarks the changed lines of the cursor row. An open range is folded into the marks first.
    fn mark_line(&mut self) {
        let Some(hunk) = self.cursor_hunk() else {
            return;
        };
        if !self.ensure_sel(hunk) {
            return;
        }
        let bodies = self.cursor_bodies(hunk);
        let range = match self.line_sel.as_ref() {
            Some(sel) => self.range_bodies(sel),
            None => return,
        };
        let Some(sel) = self.line_sel.as_mut() else {
            return;
        };
        if sel.anchor.take().is_some() {
            sel.marked.extend(range);
        }
        sel.toggle(&bodies);
    }

    /// `space`, `s`, `u` (stage or unstage) and `x` (discard) on the marked lines. Stage and unstage act on
    /// the index, discard on the worktree.
    fn apply_lines(&mut self, discard: bool) {
        let Some(sel) = self.active_sel() else {
            return;
        };
        let hunk = sel.hunk;
        let picks: Vec<usize> = self.selection_bodies(sel).into_iter().collect();
        if picks.is_empty() {
            return self.flash("Selection contains no changes", true);
        }
        if !self.hunk_ops_allowed() {
            return self.flash("Line staging needs a plain text diff", true);
        }
        let Some(shown) = self.shown.as_ref() else {
            return;
        };
        let (area, kind) = (shown.change.area, shown.change.kind);
        let (reverse, cached, verb) = match (discard, area) {
            (true, Area::Unstaged) if !matches!(kind, Kind::Untracked | Kind::Conflicted) => {
                (true, false, "Discarded")
            }
            (true, _) => return self.flash("Only unstaged text lines can be discarded", true),
            (false, Area::Unstaged) => (false, true, "Staged"),
            (false, Area::Staged) => (true, true, "Unstaged"),
            (false, Area::Commit) => return,
        };
        let Some(result) = self
            .repo
            .apply_lines(&shown.doc, hunk, &picks, reverse, cached)
        else {
            return self.flash("Selection contains no changes", true);
        };
        let plural = if picks.len() == 1 { "" } else { "s" };
        match result {
            Ok(()) => {
                self.line_sel = None;
                self.flash(format!("{verb} {} line{plural}", picks.len()), false);
                self.sync();
            }
            Err(error) => self.flash(error, true),
        }
    }

    /// Hint-line entries for the line keys, shown in the diff.
    pub(super) fn line_hints(&self) -> Vec<String> {
        if !self.line_ops_allowed() {
            return Vec::new();
        }
        if self.selecting() {
            vec![
                "space stage lines".to_owned(),
                "x discard lines".to_owned(),
                "esc clear lines".to_owned(),
            ]
        } else {
            vec!["shift+↑/↓ select".to_owned(), "m mark line".to_owned()]
        }
    }

    /// Marker prefix for a diff row that belongs to the active line selection.
    pub(super) fn with_line_marker(
        &self,
        index: usize,
        content: Vec<Span<'static>>,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let Some(sel) = self.active_sel() else {
            return content;
        };
        let (Some(row), Some(shown)) = (self.rows.get(index), self.shown.as_ref()) else {
            return content;
        };
        let Some(hunk) = shown.doc.hunks.get(sel.hunk) else {
            return content;
        };
        let picked = self.selection_bodies(sel);
        let marked = row_bodies(row, &changed_keys(hunk))
            .iter()
            .any(|body| picked.contains(body));
        if !marked {
            return content;
        }
        let mut out = vec![Span::styled(
            "▌",
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )];
        out.extend(content);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GitOverlay, model::*};
    use super::{LineSel, changed_keys, span_bodies};
    use crate::git_panel::parse_diff;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::Path;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// One hunk with two changed pairs: body lines are 0 ` one`, 1 `-two`, 2 `+TWO`, 3 ` three`,
    /// 4 `-four`, 5 `+FOUR`, 6 ` five`.
    fn sample() -> Doc {
        parse_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,5 +1,5 @@\n one\n-two\n+TWO\n three\n-four\n+FOUR\n five\n",
        )
    }

    fn sample_change() -> Change {
        Change {
            path: "f".to_owned(),
            orig: None,
            kind: Kind::Modified,
            area: Area::Unstaged,
        }
    }

    #[test]
    fn crlf_hunk_round_trips_cr_and_displays_without_it() {
        let text =
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n one\r\n-two\r\n+TWO\r\n";
        let doc = parse_diff(text);
        let lines: Vec<&str> = doc.hunks[0].lines.iter().map(String::as_str).collect();
        assert_eq!(lines, vec![" one\r", "-two\r", "+TWO\r"]);
        assert_eq!(crate::git_panel::hunk_patch(&doc, 0).as_deref(), Some(text));
        for view in [View::Inline, View::Split] {
            for row in layout(&doc, view, Some(0)) {
                let shown: Vec<&str> = match &row.kind {
                    RowKind::Line { text, .. } => vec![text.as_str()],
                    RowKind::Pair { left, right } => left
                        .iter()
                        .chain(right.iter())
                        .map(|col| col.text.as_str())
                        .collect(),
                    _ => Vec::new(),
                };
                assert!(shown.iter().all(|text| !text.contains('\r')));
            }
        }
    }

    #[test]
    fn toggle_marks_then_unmarks_body_lines() {
        let doc = sample();
        let mut sel = LineSel::new(&sample_change(), 0, &doc.hunks[0], View::Inline);
        sel.toggle(&[2]);
        sel.toggle(&[1, 2]);
        assert_eq!(sel.marked.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        sel.toggle(&[1, 2]);
        assert!(sel.marked.is_empty());
        sel.toggle(&[]);
        assert!(sel.marked.is_empty());
    }

    #[test]
    fn range_extends_over_changed_lines_in_row_order() {
        let doc = sample();
        let rows = layout(&doc, View::Inline, Some(0));
        let keys = changed_keys(&doc.hunks[0]);
        let row_of = |text: &str| {
            rows.iter()
                .position(|row| matches!(&row.kind, RowKind::Line { text: t, .. } if t == text))
                .expect("row exists")
        };
        let (from, to) = (row_of("two"), row_of("FOUR"));
        assert_eq!(span_bodies(&rows, 0, from, to, &keys), vec![1, 2, 4, 5]);
        assert_eq!(span_bodies(&rows, 0, to, to, &keys), vec![5]);
        assert_eq!(
            span_bodies(&rows, 0, row_of("one"), row_of("one"), &keys),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn selection_stages_only_the_marked_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").expect("write");
        git(root, &["add", "f.txt"]);
        git(
            root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
        );
        std::fs::write(root.join("f.txt"), "one\nTWO\nthree\nFOUR\nfive\n").expect("write");

        let mut overlay = GitOverlay::open(
            crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
            None,
        )
        .expect("overlay opens");
        overlay.focus = Focus::Diff;
        overlay.cursor_row = overlay
            .rows
            .iter()
            .position(|row| {
                matches!(&row.kind, RowKind::Line { side: Side::Added, text, .. } if text == "TWO")
            })
            .expect("TWO row");

        let mark = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(overlay.line_key(&mark, Some('m')));
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(overlay.line_key(&space, Some(' ')));

        assert_eq!(
            git(root, &["show", ":f.txt"]),
            "one\ntwo\nTWO\nthree\nfour\nfive\n"
        );
    }
}
