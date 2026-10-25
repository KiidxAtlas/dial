use std::sync::Arc;

use parking_lot::Mutex;

use ratatui::text::Span;

use super::*;

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);
impl Write for Shared {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An `InlineTerminal` wired to a vt100 emulator: assertions look at what a user would see.
struct Rig {
    term: InlineTerminal,
    bytes: Shared,
    parser: vt100::Parser,
    raw: Vec<u8>,
    history: Vec<String>,
    seen: usize,
}

fn lines(rows: &[&str]) -> Vec<Line<'static>> {
    rows.iter().map(|r| Line::from(r.to_string())).collect()
}

impl Rig {
    fn new(cols: u16, rows: u16) -> Self {
        let bytes = Shared::default();
        Self {
            term: with_writer(Box::new(bytes.clone()), (cols, rows)),
            bytes,
            parser: vt100::Parser::new(rows, cols, 100_000),
            raw: Vec::new(),
            history: Vec::new(),
            seen: 0,
        }
    }

    fn sync(&mut self) {
        let data = std::mem::take(&mut *self.bytes.0.lock());
        self.feed(&data);
        self.raw.extend(data);
    }

    fn commit(&mut self, rows: &[&str]) {
        self.term.commit(lines(rows));
    }

    fn paint(&mut self, rows: &[&str], cursor: Option<(u16, u16)>) {
        self.paint_lines(&lines(rows), cursor);
    }

    fn paint_lines(&mut self, rows: &[Line<'static>], cursor: Option<(u16, u16)>) {
        self.term
            .paint(&Viewport {
                rows,
                cursor,
                title: None,
                signals: None,
            })
            .unwrap();
        self.sync();
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.term.set_size_for_test((cols, rows));
        self.parser.set_size(rows, cols);
        self.term.on_resize();
    }

    /// Visible screen rows, right-trimmed.
    fn screen(&self) -> Vec<String> {
        let s = self.parser.screen();
        s.rows(0, s.size().1)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    /// Everything the user can scroll through: scrollback followed by the screen, with trailing
    /// blank rows removed.
    fn transcript(&self) -> Vec<String> {
        let mut all = self.history.clone();
        all.extend(self.screen());
        while all.last().is_some_and(|r| r.is_empty()) {
            all.pop();
        }
        all
    }

    /// Rows that have scrolled off the top of the screen so far.
    fn scrollback_len(&self) -> usize {
        self.history.len()
    }

    /// vt100 0.15 cannot scroll back further than one screen at a time, so feed bytes one by
    /// one and record each row the instant it enters scrollback.
    fn feed(&mut self, data: &[u8]) {
        for byte in data {
            self.parser.process(std::slice::from_ref(byte));
            self.parser.set_scrollback(usize::MAX);
            let len = self.parser.screen().scrollback();
            self.parser.set_scrollback(0);
            while self.seen < len {
                self.parser.set_scrollback(len - self.seen);
                let cols = self.parser.screen().size().1;
                let row = self.parser.screen().rows(0, cols).next().unwrap();
                self.parser.set_scrollback(0);
                self.history.push(row.trim_end().to_string());
                self.seen += 1;
            }
        }
    }
}

fn numbered(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("history {i}")).collect()
}

fn commit_numbered(rig: &mut Rig, n: usize) {
    let rows = numbered(n);
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    rig.commit(&refs);
}

#[test]
fn unchanged_frames_are_silent_but_cursor_resize_and_history_are_not() {
    let mut rig = Rig::new(24, 8);
    rig.paint(&["hello"], Some((0, 1)));
    let n = rig.raw.len();
    rig.paint(&["hello"], Some((0, 1)));
    assert_eq!(rig.raw.len(), n);
    rig.paint(&["hello"], Some((0, 2)));
    assert!(rig.raw.len() > n);
    let n = rig.raw.len();
    rig.resize(30, 8);
    rig.paint(&["hello"], Some((0, 2)));
    assert!(rig.raw.len() > n);
    rig.commit(&["history"]);
    rig.paint(&["hello"], Some((0, 2)));
    assert_eq!(rig.term.history_rows_written(), 1);
    assert!(rig.transcript().iter().any(|line| line == "history"));
}

#[test]
fn history_enters_scrollback_once_in_order_above_the_viewport() {
    let mut rig = Rig::new(24, 8);
    commit_numbered(&mut rig, 15);
    rig.paint(&["╭─ box ─╮", "│ hi │", "╰───────╯"], Some((1, 4)));

    let mut expected = numbered(15);
    expected.extend(["╭─ box ─╮", "│ hi │", "╰───────╯"].map(String::from));
    assert_eq!(rig.transcript(), expected);
    assert_eq!(
        rig.scrollback_len(),
        expected.len() - 8,
        "everything above the screen is scrollback, nothing duplicated"
    );
    assert_eq!(&rig.screen()[5..], ["╭─ box ─╮", "│ hi │", "╰───────╯"]);
    assert_eq!(rig.term.history_rows_written(), 15);
}

#[test]
fn repainting_the_same_viewport_leaves_no_duplicates_or_residue() {
    let mut rig = Rig::new(20, 6);
    commit_numbered(&mut rig, 7);
    rig.paint(&["alpha", "beta"], Some((0, 2)));
    let first = rig.transcript();
    for _ in 0..50 {
        rig.paint(&["alpha", "beta"], Some((0, 2)));
    }
    assert_eq!(rig.transcript(), first);
    assert_eq!(rig.term.history_rows_written(), 7);
}

#[test]
fn history_committed_between_repaints_is_appended_without_disturbing_the_viewport() {
    let mut rig = Rig::new(20, 6);
    rig.paint(&["input"], Some((0, 5)));
    let mut expected = Vec::new();
    for i in 0..10 {
        let row = format!("line {i}");
        expected.push(row.clone());
        rig.commit(&[&row]);
        rig.paint(&["input"], Some((0, 5)));
    }
    expected.push("input".to_string());
    assert_eq!(rig.transcript(), expected);
    assert_eq!(rig.screen().last().unwrap(), "input");
}

#[test]
fn viewport_growing_then_shrinking_leaves_no_rows_behind() {
    let mut rig = Rig::new(20, 10);
    commit_numbered(&mut rig, 3);
    rig.paint(&["composer"], Some((0, 8)));
    rig.paint(
        &["composer", "menu 1", "menu 2", "menu 3", "menu 4"],
        Some((0, 8)),
    );
    assert_eq!(
        rig.screen()[3..8],
        ["composer", "menu 1", "menu 2", "menu 3", "menu 4"]
    );
    rig.paint(&["composer", "menu 1"], Some((0, 8)));
    rig.paint(&["composer"], Some((0, 8)));
    let mut expected = numbered(3);
    expected.push("composer".to_string());
    assert_eq!(rig.transcript(), expected);
    assert!(rig.screen()[4..].iter().all(|r| r.is_empty()));
}

#[test]
fn shrinking_to_an_empty_viewport_and_back() {
    let mut rig = Rig::new(20, 6);
    commit_numbered(&mut rig, 2);
    rig.paint(&["a", "b", "c"], None);
    rig.paint(&[], None);
    let mut expected = numbered(2);
    assert_eq!(rig.transcript(), expected);
    rig.paint(&["x"], Some((0, 1)));
    expected.push("x".to_string());
    assert_eq!(rig.transcript(), expected);
}

#[test]
fn history_with_an_empty_viewport_still_scrolls_naturally() {
    let mut rig = Rig::new(20, 4);
    commit_numbered(&mut rig, 9);
    rig.paint(&[], None);
    assert_eq!(rig.transcript(), numbered(9));
    assert_eq!(rig.parser.screen().cursor_position().1, 0);
}

#[test]
fn growth_at_the_bottom_scrolls_history_instead_of_overwriting_it() {
    let mut rig = Rig::new(20, 6);
    commit_numbered(&mut rig, 10);
    rig.paint(&["input"], Some((0, 5)));
    rig.paint(&["input", "m1", "m2", "m3"], Some((0, 5)));
    rig.paint(&["input", "m1", "m2", "m3", "m4", "m5"], Some((0, 5)));
    let mut expected = numbered(10);
    expected.extend(["input", "m1", "m2", "m3", "m4", "m5"].map(String::from));
    assert_eq!(rig.transcript(), expected);
    assert_eq!(rig.screen(), ["input", "m1", "m2", "m3", "m4", "m5"]);
    rig.paint(&["input"], Some((0, 5)));
    let mut expected = numbered(10);
    expected.push("input".to_string());
    assert_eq!(rig.transcript(), expected);
}

#[test]
fn viewport_taller_than_the_terminal_keeps_its_bottom_rows() {
    let mut rig = Rig::new(20, 4);
    commit_numbered(&mut rig, 3);
    let rows = ["r1", "r2", "r3", "r4", "r5", "r6"];
    rig.paint(&rows, Some((5, 1)));
    assert_eq!(rig.screen(), ["r3", "r4", "r5", "r6"]);
    assert_eq!(rig.parser.screen().cursor_position(), (3, 1));
    // A cursor in a dropped row is clamped into view rather than panicking.
    rig.paint(&rows, Some((0, 0)));
    assert_eq!(rig.screen(), ["r3", "r4", "r5", "r6"]);
    let transcript = rig.transcript();
    assert_eq!(&transcript[..3], numbered(3), "history survives");
}

#[test]
fn height_one_terminal_keeps_history_and_the_single_row() {
    let mut rig = Rig::new(14, 1);
    commit_numbered(&mut rig, 4);
    rig.paint(&["prompt", "dropped above"], Some((0, 3)));
    assert_eq!(rig.screen(), ["dropped above"]);
    rig.paint(&["prompt", "kept"], Some((1, 2)));
    rig.commit(&["more"]);
    rig.paint(&["prompt", "kept"], Some((1, 2)));
    let mut expected = numbered(4);
    expected.extend(["more", "kept"].map(String::from));
    assert_eq!(rig.transcript(), expected);
}

#[test]
fn width_one_terminal_never_wraps_or_panics() {
    let mut rig = Rig::new(1, 5);
    commit_numbered(&mut rig, 2);
    rig.paint(&["abc", "日本", "x"], Some((0, 9)));
    assert_eq!(rig.screen()[2..], ["a", "", "x"]);
    let t = rig.transcript();
    assert_eq!(&t[..2], ["h", "h"], "history rows are clipped, not wrapped");
    assert_eq!(rig.parser.screen().cursor_position().1, 0);
}

#[test]
fn wide_graphemes_are_clipped_by_cells() {
    let mut rig = Rig::new(5, 4);
    rig.paint(
        &[
            "日本語",
            "ab日本",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
        ],
        None,
    );
    assert_eq!(
        rig.screen()[..3],
        ["日本", "ab日", "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}"]
    );
    assert_eq!(rows_to_ansi(&lines(&["日本語"]), 5)[0], "日本\x1b[0m");
    assert_eq!(
        rows_to_ansi(&lines(&["日本語"]), 3)[0],
        "日\x1b[0m",
        "a wide cell that does not fit is dropped whole"
    );
}

#[test]
fn colors_and_modifiers_reach_the_screen() {
    let mut rig = Rig::new(40, 4);
    let row = Line::from(vec![
        Span::styled(
            "rgb",
            Style::default()
                .fg(Color::Rgb(10, 20, 30))
                .bg(Color::Indexed(200)),
        ),
        Span::styled(
            "bold",
            Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        ),
        Span::styled(
            "rev",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::UNDERLINED),
        ),
        Span::styled("red", Style::default().fg(Color::Red)),
        Span::raw("plain"),
    ]);
    rig.paint_lines(&[row], None);
    let screen = rig.parser.screen();
    let cell = |col: u16| screen.cell(0, col).unwrap();
    assert_eq!(cell(0).fgcolor(), vt100::Color::Rgb(10, 20, 30));
    assert_eq!(cell(0).bgcolor(), vt100::Color::Idx(200));
    assert!(!cell(0).bold());
    assert!(cell(3).bold() && cell(3).italic());
    assert_eq!(
        cell(3).fgcolor(),
        vt100::Color::Default,
        "attributes do not inherit the previous span's colors"
    );
    assert!(cell(7).inverse() && cell(7).underline());
    assert_eq!(cell(10).fgcolor(), vt100::Color::Idx(1));
    assert!(!cell(10).inverse());
    assert_eq!(cell(13).fgcolor(), vt100::Color::Default);
    assert_eq!(cell(13).bgcolor(), vt100::Color::Default);
}

#[test]
fn background_never_bleeds_into_the_next_row_or_the_rest_of_the_line() {
    let mut rig = Rig::new(10, 3);
    let tinted = Line::from(Span::styled("ab", Style::default().bg(Color::Rgb(1, 2, 3))));
    rig.paint_lines(&[tinted, Line::from("cd")], None);
    let screen = rig.parser.screen();
    assert_eq!(screen.cell(0, 5).unwrap().bgcolor(), vt100::Color::Default);
    assert_eq!(screen.cell(1, 0).unwrap().bgcolor(), vt100::Color::Default);
}

#[test]
fn cursor_lands_on_the_requested_cell_and_is_visible() {
    let mut rig = Rig::new(20, 8);
    rig.paint(&["one", "two", "three"], Some((1, 2)));
    // Rows are painted from the cursor's starting line; the cursor sits on the second row.
    let (row, col) = rig.parser.screen().cursor_position();
    assert_eq!(rig.screen()[row as usize], "two");
    assert_eq!(col, 2);
    assert!(!rig.parser.screen().hide_cursor());
    // Moving the cursor within an unchanged viewport.
    rig.paint(&["one", "two", "three"], Some((2, 4)));
    let (row, col) = rig.parser.screen().cursor_position();
    assert_eq!((rig.screen()[row as usize].as_str(), col), ("three", 4));
    // Column beyond the width is clamped.
    rig.paint(&["one", "two", "three"], Some((0, 500)));
    assert_eq!(rig.parser.screen().cursor_position().1, 19);
}

#[test]
fn cursor_is_hidden_when_the_viewport_has_none() {
    let mut rig = Rig::new(20, 4);
    rig.paint(&["x"], None);
    assert!(rig.parser.screen().hide_cursor());
    rig.paint(&["x"], Some((0, 0)));
    assert!(!rig.parser.screen().hide_cursor());
}

#[test]
fn frame_diff_writes_only_changed_rows_and_repaints_style_changes() {
    let mut rig = Rig::new(30, 8);
    rig.paint(&["stable row", "old row"], Some((1, 2)));
    let before = rig.raw.len();
    rig.paint(&["stable row", "new row"], Some((1, 2)));
    let update = String::from_utf8_lossy(&rig.raw[before..]);
    assert!(!update.contains("stable row"));
    assert!(update.contains("new row"));
    assert!(rig.screen().iter().any(|row| row == "stable row"));
    assert!(rig.screen().iter().any(|row| row == "new row"));
    let before = rig.raw.len();
    rig.paint_lines(
        &[
            Line::from("stable row"),
            Line::styled("new row", Style::default().fg(Color::Red)),
        ],
        Some((1, 2)),
    );
    assert!(String::from_utf8_lossy(&rig.raw[before..]).contains("new row"));
    assert!(!String::from_utf8_lossy(&rig.raw[before..]).contains("stable row"));
}

#[test]
fn each_paint_is_one_synchronized_write() {
    let mut rig = Rig::new(20, 4);
    rig.paint(&["x"], Some((0, 0)));
    let text = String::from_utf8(rig.raw.clone()).unwrap();
    assert!(text.starts_with("\x1b[?2026h") && text.ends_with("\x1b[?2026l"));
    assert_eq!(text.matches("\x1b[?2026h").count(), 1);
}

#[test]
fn resize_between_paints_keeps_history_intact() {
    let mut rig = Rig::new(30, 8);
    commit_numbered(&mut rig, 6);
    rig.paint(&["composer", "menu"], Some((0, 3)));
    rig.resize(30, 12);
    rig.paint(&["composer", "menu"], Some((0, 3)));
    let t = rig.transcript();
    assert_eq!(&t[t.len() - 2..], ["composer", "menu"]);
    assert_eq!(
        t.iter().filter(|r| r.as_str() == "composer").count(),
        1,
        "no stale viewport copy"
    );
    assert_eq!(
        t.iter().filter(|r| r.starts_with("history")).count(),
        6,
        "history neither lost nor duplicated"
    );
}

#[test]
fn shrinking_the_terminal_height_does_not_leave_a_stale_viewport() {
    let mut rig = Rig::new(30, 10);
    commit_numbered(&mut rig, 3);
    rig.paint(&["composer", "menu 1", "menu 2"], Some((0, 1)));
    rig.resize(30, 5);
    rig.paint(&["composer", "menu 1", "menu 2"], Some((0, 1)));
    let t = rig.transcript();
    assert_eq!(t.iter().filter(|r| r.as_str() == "composer").count(), 1);
    assert_eq!(t.iter().filter(|r| r.as_str() == "menu 2").count(), 1);
    assert_eq!(t.last().unwrap(), "menu 2");
}

#[test]
fn growing_the_width_repaints_cleanly() {
    let mut rig = Rig::new(10, 6);
    rig.paint(&["abcdefghij", "second"], Some((1, 2)));
    rig.resize(20, 6);
    rig.paint(&["abcdefghijklmnopqrst", "second"], Some((1, 2)));
    assert_eq!(rig.transcript(), ["abcdefghijklmnopqrst", "second"]);
}

#[test]
fn narrowing_the_width_repaints_without_a_stale_copy() {
    // vt100 does not reflow on resize, so this proves the clamped upward move and erase on a
    // width change; the reflow arithmetic (`ceil(width / new_width)`) is not emulated.
    let mut rig = Rig::new(20, 8);
    rig.paint(&["12345678901234567890", "tail"], Some((1, 0)));
    rig.resize(10, 8);
    rig.paint(&["1234567890", "tail"], Some((1, 0)));
    let t = rig.transcript();
    assert_eq!(t.iter().filter(|r| r.as_str() == "tail").count(), 1);
    assert_eq!(t.last().unwrap(), "tail");
}

#[test]
fn leave_erases_the_live_region_and_parks_the_cursor_below_the_history() {
    let mut rig = Rig::new(20, 8);
    commit_numbered(&mut rig, 3);
    rig.paint(&["╭─ box ─╮", "│ typed │", "╰───────╯"], Some((1, 3)));
    rig.term.leave();
    rig.sync();
    assert_eq!(rig.transcript(), numbered(3));
    assert_eq!(rig.parser.screen().cursor_position(), (3, 0));
    assert!(!rig.parser.screen().hide_cursor());
    // Idempotent: a second leave (and Drop) writes nothing more.
    rig.term.leave();
    rig.sync();
    assert_eq!(rig.parser.screen().cursor_position(), (3, 0));
}

#[test]
fn leave_flushes_rows_committed_but_not_yet_painted() {
    let mut rig = Rig::new(20, 6);
    rig.paint(&["input"], Some((0, 0)));
    rig.commit(&["last words"]);
    rig.term.leave();
    rig.sync();
    assert_eq!(rig.transcript(), ["last words"]);
    assert_eq!(rig.parser.screen().cursor_position(), (1, 0));
}

#[test]
fn leave_from_the_bottom_row_keeps_the_cursor_on_screen() {
    let mut rig = Rig::new(20, 4);
    commit_numbered(&mut rig, 6);
    rig.paint(&["a", "b"], Some((1, 0)));
    rig.term.leave();
    rig.sync();
    assert_eq!(rig.transcript(), numbered(6));
    let (row, col) = rig.parser.screen().cursor_position();
    assert_eq!(col, 0);
    assert!(rig.screen()[row as usize].is_empty());
}

#[test]
fn clear_screen_resets_the_display_and_restarts_tracking() {
    let mut rig = Rig::new(20, 4);
    commit_numbered(&mut rig, 9);
    rig.paint(&["input", "menu"], Some((0, 0)));
    rig.commit(&["discarded"]);
    rig.term.clear_screen().unwrap();
    rig.sync();
    // (vt100 does not implement `3J`, so scrollback erasure itself is checked on the bytes.)
    assert!(
        String::from_utf8(rig.raw.clone())
            .unwrap()
            .contains("\x1b[2J\x1b[3J\x1b[H")
    );
    assert!(rig.screen().iter().all(|r| r.is_empty()));
    assert_eq!(rig.parser.screen().cursor_position(), (0, 0));
    rig.commit(&["fresh"]);
    rig.paint(&["input"], Some((0, 0)));
    assert_eq!(
        rig.screen(),
        ["fresh", "input", "", ""],
        "tracking restarted at the top; the discarded row never appears"
    );
}

#[test]
fn overlay_round_trip_restores_the_inline_view() {
    let mut rig = Rig::new(30, 8);
    commit_numbered(&mut rig, 4);
    rig.paint(&["╭ composer ╮", "╰──────────╯"], Some((0, 3)));
    let before = rig.transcript();

    rig.term.open_overlay().unwrap();
    assert!(rig.term.overlay_open());
    assert_eq!(rig.term.overlay_size(), (30, 8));
    for label in ["SETTINGS PANEL", "SETTINGS PANEL 2"] {
        rig.term
            .draw_overlay(|frame| {
                frame.render_widget(ratatui::widgets::Paragraph::new(label), frame.area());
            })
            .unwrap();
    }
    rig.term.close_overlay().unwrap();
    assert!(!rig.term.overlay_open());
    rig.term.close_overlay().unwrap();
    rig.sync();
    let text = String::from_utf8(rig.raw.clone()).unwrap();
    let enter = text
        .find("\x1b[?1049h")
        .expect("enters the alternate screen");
    let leave = text
        .find("\x1b[?1049l")
        .expect("leaves the alternate screen");
    assert!(enter < leave);
    assert!(
        text[enter..leave].contains("\x1b[?2004h"),
        "paste mode enabled in the panel"
    );
    assert!(
        text[leave..].contains("\x1b[?2004h"),
        "paste mode re-armed after the panel"
    );
    assert!(text[enter..leave].contains("SETTINGS"));
    assert!(!rig.parser.screen().alternate_screen());
    assert_eq!(
        rig.transcript(),
        before,
        "the normal buffer is untouched by the overlay"
    );

    // Next paint redraws the viewport in full at the saved position, no duplicates.
    let before_len = rig.raw.len();
    rig.paint(&["╭ composer ╮", "╰──────────╯"], Some((0, 3)));
    assert!(
        rig.raw[before_len..].windows(3).any(|w| w == b"\x1b[J"),
        "the redraw erases from the viewport top"
    );
    assert_eq!(rig.transcript(), before);
}

#[test]
fn suspend_disables_bracketed_paste_before_f_and_reenables_after() {
    let mut rig = Rig::new(20, 8);
    rig.paint(&["│ typed │"], Some((0, 3)));
    let bytes = rig.bytes.clone();
    let mut at_call = Vec::new();
    let value = rig
        .term
        .suspend(|| {
            at_call = bytes.0.lock().clone();
            42
        })
        .unwrap();
    assert_eq!(value, 42);
    let before_f = String::from_utf8(at_call).unwrap();
    assert!(
        before_f.contains("\x1b[?2004l"),
        "bracketed paste disabled before f"
    );
    assert!(before_f.contains("\x1b[?25h"), "cursor shown before f");
    assert!(
        !before_f.contains("\x1b[?2004h"),
        "nothing re-armed before f ran"
    );
    rig.sync();
    let all = String::from_utf8(rig.raw.clone()).unwrap();
    assert!(
        all.rfind("\x1b[?2004l").unwrap() < all.rfind("\x1b[?2004h").unwrap(),
        "re-enabled after f"
    );
}

#[test]
fn suspend_erases_the_viewport_and_the_next_paint_redraws_without_touching_history() {
    let mut rig = Rig::new(20, 8);
    commit_numbered(&mut rig, 5);
    rig.paint(&["╭─ box ─╮", "│ typed │", "╰───────╯"], Some((1, 3)));
    let bytes = rig.bytes.clone();
    let mut probe = vt100::Parser::new(8, 20, 0);
    probe.process(&rig.raw);
    let mut seen = Vec::new();
    let mut cursor = (0, 0);
    rig.term
        .suspend(|| {
            probe.process(&bytes.0.lock());
            seen = probe
                .screen()
                .rows(0, 20)
                .map(|r| r.trim_end().to_string())
                .collect::<Vec<_>>();
            cursor = probe.screen().cursor_position();
        })
        .unwrap();
    assert!(
        !seen
            .iter()
            .any(|r| r.contains("box") || r.contains("typed")),
        "viewport gone while the editor runs: {seen:?}"
    );
    assert_eq!(cursor.1, 0, "cursor at column 0");
    rig.sync();
    // The "editor" prints something, then the viewport comes back below it.
    rig.parser.process(b"editor output\r\n");
    rig.paint(&["╭─ box ─╮", "│ typed │", "╰───────╯"], Some((1, 3)));
    let t = rig.transcript();
    let history: Vec<_> = t
        .iter()
        .filter(|r| r.starts_with("history"))
        .cloned()
        .collect();
    assert_eq!(history, numbered(5), "history exactly once, in order");
    assert_eq!(
        t.iter().filter(|r| r.contains("box")).count(),
        1,
        "no stale viewport copy: {t:?}"
    );
    assert_eq!(t.last().unwrap(), "╰───────╯");
}

#[test]
fn rows_committed_before_a_suspend_are_written_once_after_it() {
    let mut rig = Rig::new(20, 8);
    rig.paint(&["input"], Some((0, 0)));
    rig.commit(&["pending 1", "pending 2"]);
    rig.term.suspend(|| ()).unwrap();
    assert_eq!(rig.term.history_rows_pending(), 2);
    rig.sync();
    rig.paint(&["input"], Some((0, 0)));
    assert_eq!(rig.transcript(), ["pending 1", "pending 2", "input"]);
    assert_eq!(rig.term.history_rows_written(), 2);
}

#[test]
fn suspend_does_not_rearm_after_the_program_panics() {
    let mut rig = Rig::new(20, 6);
    rig.paint(&["input"], Some((0, 0)));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = rig.term.suspend(|| panic!("editor crashed"));
    }));
    assert!(
        caught.is_err(),
        "the panic propagates; the panic hook already restored the terminal"
    );
    rig.sync();
    let all = String::from_utf8(rig.raw.clone()).unwrap();
    assert!(
        all.rfind("\x1b[?2004h")
            .is_none_or(|on| on < all.rfind("\x1b[?2004l").unwrap())
    );
    rig.paint(&["input"], Some((0, 0)));
    assert_eq!(rig.transcript(), ["input"]);
}

#[test]
fn panel_stays_open_between_frames_and_history_committed_meanwhile_lands_once() {
    let mut rig = Rig::new(30, 8);
    rig.paint(&["input"], Some((0, 0)));
    rig.term.open_overlay().unwrap();
    rig.term
        .draw_overlay(|frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new("PANEL"), frame.area());
        })
        .unwrap();
    rig.sync();
    assert!(rig.parser.screen().alternate_screen());
    // The agent keeps producing output while the panel is up.
    rig.commit(&["streamed 1", "streamed 2"]);
    rig.term
        .draw_overlay(|frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new("PANEL"), frame.area());
        })
        .unwrap();
    let before = rig.raw.len();
    rig.sync();
    assert_eq!(rig.raw.len(), before, "an unchanged frame writes nothing");
    rig.term.close_overlay().unwrap();
    rig.sync();
    assert!(!rig.parser.screen().alternate_screen());
    rig.paint(&["input"], Some((0, 0)));
    assert_eq!(rig.transcript(), ["streamed 1", "streamed 2", "input"]);
}

#[test]
fn mouse_reporting_is_only_on_while_a_panel_is_open() {
    let mut rig = Rig::new(30, 8);
    rig.term.set_overlay_mouse(true);
    rig.paint(&["input"], Some((0, 0)));
    rig.term.open_overlay().unwrap();
    rig.term.close_overlay().unwrap();
    rig.sync();
    let text = String::from_utf8(rig.raw.clone()).unwrap();
    let on = text
        .find(MOUSE_ON)
        .expect("mouse reporting enabled in the panel");
    let off = text
        .rfind(MOUSE_OFF)
        .expect("mouse reporting disabled on close");
    assert!(text.find("\x1b[?1049h").unwrap() < on && on < off);
    assert!(off < text.rfind("\x1b[?1049l").unwrap());
}

#[test]
fn suspend_and_leave_never_leave_a_panel_covering_the_terminal() {
    let mut rig = Rig::new(30, 8);
    rig.paint(&["input"], Some((0, 0)));
    rig.term.open_overlay().unwrap();
    let bytes = rig.bytes.clone();
    let mut alternate_during = true;
    let mut probe = vt100::Parser::new(8, 30, 0);
    rig.term
        .suspend(|| {
            probe.process(&bytes.0.lock());
            alternate_during = probe.screen().alternate_screen();
        })
        .unwrap();
    assert!(
        !alternate_during,
        "the external program runs on the normal screen"
    );
    assert!(rig.term.overlay_open(), "the panel comes back afterwards");
    rig.term.leave();
    rig.sync();
    assert!(!rig.parser.screen().alternate_screen());
}

#[test]
fn title_is_set_once_per_change_and_sanitized() {
    let mut rig = Rig::new(20, 4);
    let rows = lines(&["x"]);
    for title in ["dial", "dial", "dial\x1b[2Jevil"] {
        rig.term
            .paint(&Viewport {
                rows: &rows,
                cursor: None,
                title: Some(title),
                signals: None,
            })
            .unwrap();
    }
    rig.sync();
    let text = String::from_utf8(rig.raw.clone()).unwrap();
    assert_eq!(text.matches("\x1b]0;").count(), 2);
    assert!(!text.contains("\x1b[2Jevil"));
}

// ---- rows_to_ansi -------------------------------------------------------------------

#[test]
fn rows_to_ansi_encodes_colors_and_modifiers() {
    let row = Line::from(vec![
        Span::styled(
            "a",
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Indexed(9)),
        ),
        Span::styled(
            "b",
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD | Modifier::DIM),
        ),
        Span::styled(
            "c",
            Style::default()
                .fg(Color::Gray)
                .bg(Color::Black)
                .add_modifier(
                    Modifier::ITALIC
                        | Modifier::UNDERLINED
                        | Modifier::REVERSED
                        | Modifier::CROSSED_OUT,
                ),
        ),
        Span::raw("d"),
    ]);
    assert_eq!(
        rows_to_ansi(&[row], 10)[0],
        "\x1b[0;38;2;1;2;3;48;5;9ma\x1b[0;1;2;94mb\x1b[0;3;4;7;9;37;40mc\x1b[0md\x1b[0m"
    );
}

#[test]
fn rows_to_ansi_resets_every_row_and_clips_in_cells() {
    let styled = Line::from(Span::styled("hello world", Style::default().fg(Color::Red)));
    let out = rows_to_ansi(&[styled, Line::from(""), Line::from("ok")], 5);
    assert_eq!(out, ["\x1b[0;31mhello\x1b[0m", "\x1b[0m", "ok\x1b[0m"]);
    assert_eq!(rows_to_ansi(&lines(&["abc"]), 0), ["\x1b[0m"]);
}

#[test]
fn rows_to_ansi_clips_across_span_boundaries() {
    let row = Line::from(vec![
        Span::raw("ab"),
        Span::styled("cdef", Style::default().add_modifier(Modifier::BOLD)),
    ]);
    assert_eq!(rows_to_ansi(&[row], 4)[0], "ab\x1b[0;1mcd\x1b[0m");
}

#[test]
fn rows_to_ansi_strips_control_characters_and_applies_line_style() {
    let row =
        Line::from(vec![Span::raw("a\x1b[31mb\tc\nd")]).style(Style::default().fg(Color::Green));
    let out = rows_to_ansi(&[row], 20)[0].clone();
    assert!(!out.contains('\n') && !out.contains('\t'));
    assert_eq!(
        out.matches('\x1b').count(),
        2,
        "only our own SGR + reset; the injected ESC is gone: {out:?}"
    );
    assert!(out.starts_with("\x1b[0;32m"));
}

#[test]
fn explicit_default_style_after_a_styled_span_resets() {
    let row = Line::from(vec![
        Span::styled("x", Style::default().fg(Color::Red)),
        Span::raw("y"),
    ]);
    assert_eq!(rows_to_ansi(&[row], 5)[0], "\x1b[0;31mx\x1b[0my\x1b[0m");
}

fn every_integration() -> crate::ui::termcaps::Integration {
    crate::ui::termcaps::Integration {
        hyperlinks: true,
        line_fragments: true,
        cwd: false,
        tab_color: true,
        attention: true,
        user_vars: true,
        progress: true,
        tmux: false,
    }
}

#[test]
fn scrollback_links_existing_paths_and_urls_but_not_the_live_region() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut rig = Rig::new(80, 6);
    rig.term.set_integration_for_test(every_integration());
    let signals = crate::ui::term_state::Signals {
        cwd: Some(root.path().to_path_buf()),
        ..Default::default()
    };
    rig.commit(&["see main.rs:12, https://example.com/a and missing.rs"]);
    rig.term
        .paint(&Viewport {
            rows: &lines(&["live main.rs"]),
            cursor: None,
            title: None,
            signals: Some(&signals),
        })
        .unwrap();
    rig.sync();
    let text = String::from_utf8(rig.raw.clone()).unwrap();
    let file = url::Url::from_file_path(root.path().join("main.rs")).unwrap();
    assert!(
        text.contains(&format!("\x1b]8;;{file}#12\x1b\\main.rs:12\x1b]8;;\x1b\\")),
        "{text:?}"
    );
    assert!(
        text.contains("\x1b]8;;https://example.com/a\x1b\\https://example.com/a\x1b]8;;\x1b\\")
    );
    assert_eq!(text.matches("\x1b]8;;\x1b\\").count(), 2, "{text:?}");
    // The text a user sees is unchanged.
    assert!(
        rig.transcript()
            .iter()
            .any(|row| row == "see main.rs:12, https://example.com/a and missing.rs"),
        "{:?}",
        rig.transcript()
    );
}

#[test]
fn signals_are_written_on_change_and_restored_on_leave() {
    use crate::ui::term_state::{Activity, Progress, Signals};
    let mut rig = Rig::new(40, 4);
    rig.term.set_integration_for_test(every_integration());
    let rows = lines(&["x"]);
    let working = Signals {
        activity: Activity::Working,
        progress: Progress::Percent(40),
        vars: vec![("dialState", "working".into())],
        ..Default::default()
    };
    for _ in 0..2 {
        rig.term
            .paint(&Viewport {
                rows: &rows,
                cursor: None,
                title: None,
                signals: Some(&working),
            })
            .unwrap();
    }
    rig.sync();
    let first = String::from_utf8(std::mem::take(&mut rig.raw)).unwrap();
    assert_eq!(first.matches("\x1b]9;4;1;40\x07").count(), 1, "{first:?}");
    assert_eq!(first.matches("\x1b]6;1;bg;red;brightness;").count(), 1);
    assert_eq!(
        first.matches("SetUserVar=dialState=d29ya2luZw==").count(),
        1
    );
    rig.term.leave();
    rig.sync();
    let last = String::from_utf8(rig.raw.clone()).unwrap();
    assert!(last.contains("\x1b]9;4;0\x07"), "{last:?}");
    assert!(last.contains("\x1b]6;1;bg;*;default\x07"));
    assert!(last.contains("SetUserVar=dialState=\x07"));
}
