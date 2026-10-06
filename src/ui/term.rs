//! Inline (scrollback-native) terminal driver.
//!
//! Finalized rows are written once to the *normal* screen buffer, so the terminal scrolls them into
//! its own scrollback. Only the live bottom region (the viewport: composer, menus, streaming text)
//! is repainted in place. Every paint is one write wrapped in synchronized output (`?2026`):
//!
//! 1. hide the cursor, disable autowrap;
//! 2. move up to the top of the previous viewport (relative movement only: the terminal may have
//!    scrolled since, so absolute rows are never trusted);
//! 3. write pending history rows (each followed by `\r\n`, so they scroll up naturally), then the
//!    viewport rows (no newline after the last one), erasing rows left over from a taller viewport;
//! 4. restore autowrap and park the real cursor at the requested cell.
//!
//! Full-screen panels (settings, model selector) run on the alternate screen through
//! [`InlineTerminal::with_overlay`]; the normal buffer and cursor are restored by the terminal.
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Once,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use crossterm::{
    Command,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport as RatatuiViewport,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::{Color, Modifier, Style},
    text::Line,
};
use unicode_segmentation::UnicodeSegmentation;

use super::text;

/// Only one real terminal may own the screen at a time.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Facts the panic hook needs (it has no access to the `InlineTerminal`).
static UP_ROWS: AtomicU16 = AtomicU16::new(0);
static KEYBOARD_FLAGS: AtomicBool = AtomicBool::new(false);
static IN_ALT_SCREEN: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";
const WRAP_OFF: &str = "\x1b[?7l";
const WRAP_ON: &str = "\x1b[?7h";
const ALT_ENTER: &str = "\x1b[?1049h";
const ALT_LEAVE: &str = "\x1b[?1049l";
const RESET: &str = "\x1b[0m";

/// The live region: rows to repaint, where the hardware cursor goes, and an optional window title.
pub struct Viewport<'a> {
    pub rows: &'a [Line<'static>],
    /// `(row in rows, column in cells)`. `None` leaves the cursor hidden.
    pub cursor: Option<(u16, u16)>,
    pub title: Option<&'a str>,
}

pub struct InlineTerminal {
    out: Box<dyn Write>,
    /// Talks to the real TTY (raw mode, size query, panic hook). `false` for `with_writer`.
    real: bool,
    /// Size reported by `size()` when not `real`.
    fake_size: (u16, u16),
    keyboard_flags: bool,
    left: bool,
    /// Finalized rows waiting for the next paint.
    pending: Vec<Line<'static>>,
    history_written: usize,
    /// Rows the previous paint left on screen, their cell widths, and the row (relative to the
    /// viewport top) the hardware cursor rests on.
    painted_rows: u16,
    painted_widths: Vec<usize>,
    cursor_row: u16,
    last_size: (u16, u16),
    /// Erase from the viewport top down before painting (resize, overlay return).
    force_erase: bool,
    title: Option<String>,
    last_frame: Option<(Vec<Line<'static>>, Option<(u16, u16)>)>,
}

impl InlineTerminal {
    pub fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("an interactive terminal is required (stdin and stdout must be TTYs)");
        }
        if ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("another terminal session is already active");
        }
        match Self::enter_active() {
            Ok(term) => Ok(term),
            Err(e) => {
                ACTIVE.store(false, Ordering::SeqCst);
                let _ = disable_raw_mode();
                Err(e)
            }
        }
    }

    fn enter_active() -> Result<Self> {
        enable_raw_mode().context("enabling raw mode")?;
        // Clear the terminal so dial starts at the top (like omp).
        let _ = io::stdout().write_all(b"\x1b[2J\x1b[3J\x1b[H");
        let _ = io::stdout().flush();
        let keyboard_flags = supports_keyboard_enhancement().unwrap_or(false);
        let mut out: Box<dyn Write> = Box::new(io::stdout());
        let mut setup = String::new();
        if keyboard_flags {
            push_command(
                &mut setup,
                &PushKeyboardEnhancementFlags(KEYBOARD_FLAGS_WANTED),
            );
        }
        push_command(&mut setup, &EnableBracketedPaste);
        out.write_all(setup.as_bytes())?;
        out.flush()?;
        KEYBOARD_FLAGS.store(keyboard_flags, Ordering::SeqCst);
        UP_ROWS.store(0, Ordering::SeqCst);
        IN_ALT_SCREEN.store(false, Ordering::SeqCst);
        install_panic_hook();
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        Ok(Self::build(out, size, true, keyboard_flags))
    }

    fn build(out: Box<dyn Write>, size: (u16, u16), real: bool, keyboard_flags: bool) -> Self {
        Self {
            out,
            real,
            fake_size: size,
            keyboard_flags,
            left: false,
            pending: Vec::new(),
            history_written: 0,
            painted_rows: 0,
            painted_widths: Vec::new(),
            cursor_row: 0,
            last_size: (size.0.max(1), size.1.max(1)),
            force_erase: false,
            title: None,
            last_frame: None,
        }
    }

    /// `(columns, rows)`, re-queried from the terminal on every call.
    pub fn size(&self) -> (u16, u16) {
        let (cols, rows) = if self.real {
            crossterm::terminal::size().unwrap_or(self.last_size)
        } else {
            self.fake_size
        };
        (cols.max(1), rows.max(1))
    }

    /// Queue finalized rows; they are written to scrollback exactly once on the next paint.
    pub fn commit(&mut self, rows: Vec<Line<'static>>) {
        self.pending.extend(rows);
    }

    /// Call on `Event::Resize`: the next paint erases conservatively from the recorded viewport top.
    pub fn on_resize(&mut self) {
        self.force_erase = true;
    }

    /// Number of history rows flushed to the terminal so far.
    pub fn history_rows_written(&self) -> usize {
        self.history_written
    }

    /// Number of committed rows still waiting for a paint.
    pub fn history_rows_pending(&self) -> usize {
        self.pending.len()
    }

    /// Test seam: pretend the terminal was resized (only meaningful for `with_writer` terminals).
    pub fn set_size_for_test(&mut self, size: (u16, u16)) {
        self.fake_size = size;
    }

    pub fn paint(&mut self, vp: &Viewport) -> Result<()> {
        let (cols, rows) = self.size();
        let width = cols as usize;
        // Taller than the terminal: keep the bottom rows (the composer).
        let dropped = vp.rows.len().saturating_sub(rows as usize);
        let visible = &vp.rows[dropped..];
        if !self.force_erase
            && self.pending.is_empty()
            && (cols, rows) == self.last_size
            && vp
                .title
                .is_none_or(|title| self.title.as_deref() == Some(title))
            && self
                .last_frame
                .as_ref()
                .is_some_and(|(lines, cursor)| lines == visible && *cursor == vp.cursor)
        {
            return Ok(());
        }
        let rendered: Vec<(String, usize)> = visible.iter().map(|l| render_row(l, width)).collect();
        let history: Vec<(String, usize)> =
            self.pending.iter().map(|l| render_row(l, width)).collect();

        let width_changed = cols != self.last_size.0;
        let erase_first = self.force_erase || (cols, rows) != self.last_size;
        // After a narrowing, the terminal may have re-wrapped old rows into more physical rows.
        let up = if width_changed {
            self.painted_widths
                .iter()
                .take(self.cursor_row as usize)
                .map(|&w| w.div_ceil(width).max(1))
                .sum::<usize>()
        } else {
            self.cursor_row as usize
        }
        .min(rows as usize - 1);

        let mut buf =
            String::with_capacity(256 + rendered.iter().map(|r| r.0.len() + 2).sum::<usize>());
        buf.push_str(SYNC_BEGIN);
        buf.push_str(CURSOR_HIDE);
        buf.push_str(WRAP_OFF);
        if let Some(title) = vp.title {
            let clean = text::one_line(title);
            if self.title.as_deref() != Some(clean.as_str()) {
                buf.push_str(&format!("\x1b]0;{clean}\x07"));
                self.title = Some(clean);
            }
        }
        if up > 0 {
            buf.push_str(&format!("\x1b[{up}A"));
        }
        buf.push('\r');
        if erase_first {
            buf.push_str("\x1b[J");
        }
        for (row, _) in &history {
            buf.push_str("\x1b[2K");
            buf.push_str(row);
            buf.push_str("\r\n");
        }
        for (i, (row, _)) in rendered.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str("\x1b[2K");
            buf.push_str(row);
        }
        if rendered.is_empty() {
            // The (empty) viewport still owns the line the cursor sits on.
            buf.push_str("\x1b[2K");
        }
        // Rows of a taller previous viewport that this paint did not overwrite. They are below the
        // cursor, so there is room for a relative step down (no scrolling).
        let covered = history.len() + rendered.len().max(1);
        let leftover = if erase_first {
            0
        } else {
            (self.painted_rows as usize).saturating_sub(covered)
        };
        if leftover > 0 {
            buf.push_str("\x1b[B\r\x1b[J\x1b[A");
        }

        let n = rendered.len();
        let bottom = n.saturating_sub(1);
        let target = vp.cursor.map(|(r, c)| {
            (
                (r as usize).saturating_sub(dropped).min(bottom),
                (c as usize).min(width - 1),
            )
        });
        let rest_row = target.map_or(bottom, |(r, _)| r);
        if bottom > rest_row {
            buf.push_str(&format!("\x1b[{}A", bottom - rest_row));
        }
        buf.push_str(WRAP_ON);
        if let Some((_, c)) = target {
            buf.push_str(&format!("\x1b[{}G", c + 1));
            buf.push_str(CURSOR_SHOW);
        }
        buf.push_str(SYNC_END);

        self.out.write_all(buf.as_bytes())?;
        self.out.flush()?;

        self.history_written += history.len();
        self.pending.clear();
        self.painted_rows = n as u16;
        self.painted_widths = rendered.iter().map(|r| r.1).collect();
        self.cursor_row = rest_row as u16;
        self.last_size = (cols, rows);
        self.force_erase = false;
        self.last_frame = Some((visible.to_vec(), vp.cursor));
        UP_ROWS.store(self.cursor_row, Ordering::SeqCst);
        Ok(())
    }

    /// Display reset: erase screen and scrollback. Uncommitted rows are discarded; the controller
    /// re-commits whatever it wants to show again.
    pub fn clear_screen(&mut self) -> Result<()> {
        self.out.write_all(b"\x1b[2J\x1b[3J\x1b[H")?;
        self.out.flush()?;
        self.pending.clear();
        self.painted_rows = 0;
        self.painted_widths.clear();
        self.last_frame = None;
        self.cursor_row = 0;
        self.force_erase = false;
        self.last_size = self.size();
        UP_ROWS.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Run a full-screen panel on the alternate screen, then bring the inline view back exactly:
    /// the terminal restores the normal buffer and cursor; the next paint redraws the viewport.
    pub fn with_overlay<R>(
        &mut self,
        f: impl FnOnce(&mut OverlayTerminal<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut enter = String::from(ALT_ENTER);
        if self.real {
            // Kitty keyboard flags are per screen: the alternate screen starts with none.
            if self.keyboard_flags {
                push_command(
                    &mut enter,
                    &PushKeyboardEnhancementFlags(KEYBOARD_FLAGS_WANTED),
                );
            }
            IN_ALT_SCREEN.store(true, Ordering::SeqCst);
        }
        self.out.write_all(enter.as_bytes())?;
        self.out.flush()?;

        let size_override = (!self.real).then_some(self.fake_size);
        let result = run_overlay(&mut *self.out, size_override, f);

        let mut leave = String::new();
        if self.real && self.keyboard_flags {
            push_command(&mut leave, &PopKeyboardEnhancementFlags);
        }
        leave.push_str(CURSOR_SHOW);
        leave.push_str(ALT_LEAVE);
        let restored = self
            .out
            .write_all(leave.as_bytes())
            .and_then(|()| self.out.flush());
        if self.real {
            IN_ALT_SCREEN.store(false, Ordering::SeqCst);
        }
        self.force_erase = true;
        let value = result?;
        restored?;
        Ok(value)
    }

    /// Erase the live region and park the cursor at column 0 where it began, so the shell prompt
    /// continues below the conversation. Idempotent; also runs from `Drop` and the panic hook.
    pub fn leave(&mut self) {
        if self.left {
            return;
        }
        self.left = true;
        if self.real && !ACTIVE.swap(false, Ordering::SeqCst) {
            return; // the panic hook already restored the terminal
        }
        let (cols, _) = self.size();
        let mut s = String::new();
        if self.cursor_row > 0 {
            s.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        s.push_str("\r\x1b[J");
        for line in &self.pending {
            s.push_str(&render_row(line, cols as usize).0);
            s.push_str("\r\n");
        }
        self.history_written += self.pending.len();
        self.pending.clear();
        s.push_str(WRAP_ON);
        s.push_str(CURSOR_SHOW);
        if self.real {
            if self.keyboard_flags {
                push_command(&mut s, &PopKeyboardEnhancementFlags);
            }
            push_command(&mut s, &DisableBracketedPaste);
        }
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
        self.painted_rows = 0;
        self.cursor_row = 0;
        UP_ROWS.store(0, Ordering::SeqCst);
        if self.real {
            let _ = disable_raw_mode();
        }
    }

    /// Hand the real terminal to an external full-screen program (`$EDITOR`): erase the live
    /// viewport, restore cooked mode, run `f`, then re-enter raw mode and let the next paint redraw
    /// the viewport below wherever the cursor ended up. Committed rows (pending or already
    /// written) are neither dropped nor repeated. The terminal is re-armed even if `f` panics.
    pub fn suspend<R>(&mut self, f: impl FnOnce() -> R) -> Result<R> {
        let mut s = String::new();
        if self.cursor_row > 0 {
            s.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        s.push_str("\r\x1b[J");
        s.push_str(WRAP_ON);
        s.push_str(CURSOR_SHOW);
        if self.keyboard_flags {
            push_command(&mut s, &PopKeyboardEnhancementFlags);
        }
        push_command(&mut s, &DisableBracketedPaste);
        self.out.write_all(s.as_bytes())?;
        self.out.flush()?;
        // The viewport no longer exists; the cursor now rests where it began.
        self.painted_rows = 0;
        self.painted_widths.clear();
        self.cursor_row = 0;
        UP_ROWS.store(0, Ordering::SeqCst);
        if self.real {
            disable_raw_mode().context("leaving raw mode")?;
        }

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        let rearm = (|| -> Result<()> {
            if self.real {
                enable_raw_mode().context("re-enabling raw mode")?;
            }
            let mut s = String::new();
            if self.keyboard_flags {
                push_command(&mut s, &PushKeyboardEnhancementFlags(KEYBOARD_FLAGS_WANTED));
            }
            push_command(&mut s, &EnableBracketedPaste);
            self.out.write_all(s.as_bytes())?;
            self.out.flush()?;
            Ok(())
        })();
        // The program may have scrolled or cleared the screen: repaint from the cursor, erasing below.
        self.force_erase = true;
        self.last_size = self.size();
        match outcome {
            Ok(value) => rearm.map(|()| value),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Test constructor: no TTY, no raw mode, no panic hook; everything is written to `out`.
pub fn with_writer(out: Box<dyn Write>, size: (u16, u16)) -> InlineTerminal {
    InlineTerminal::build(out, size, false, false)
}

const KEYBOARD_FLAGS_WANTED: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

fn push_command(into: &mut String, command: &impl Command) {
    // Writing into a String cannot fail.
    let _ = command.write_ansi(into);
}

fn run_overlay<R>(
    out: &mut dyn Write,
    size_override: Option<(u16, u16)>,
    f: impl FnOnce(&mut OverlayTerminal<'_>) -> Result<R>,
) -> Result<R> {
    let backend = OverlayBackend {
        inner: CrosstermBackend::new(out),
        size_override,
    };
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: RatatuiViewport::Fullscreen,
        },
    )?;
    let mut overlay = OverlayTerminal { terminal };
    f(&mut overlay)
}

/// Best-effort restore from a panic: the `InlineTerminal` is not reachable, so statics carry the facts.
fn emergency_restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut s = String::new();
    let keyboard = KEYBOARD_FLAGS.load(Ordering::SeqCst);
    if IN_ALT_SCREEN.swap(false, Ordering::SeqCst) {
        if keyboard {
            push_command(&mut s, &PopKeyboardEnhancementFlags);
        }
        s.push_str(ALT_LEAVE);
    }
    let up = UP_ROWS.load(Ordering::SeqCst);
    if up > 0 {
        s.push_str(&format!("\x1b[{up}A"));
    }
    s.push_str("\r\x1b[J");
    s.push_str(WRAP_ON);
    s.push_str(CURSOR_SHOW);
    if keyboard {
        push_command(&mut s, &PopKeyboardEnhancementFlags);
    }
    push_command(&mut s, &DisableBracketedPaste);
    let mut stdout = io::stdout();
    let _ = stdout.write_all(s.as_bytes());
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

fn install_panic_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            previous(info);
        }));
    });
}

// ---------------------------------------------------------------------------------------------
// Overlay (alternate screen)
// ---------------------------------------------------------------------------------------------

/// A ratatui terminal over the alternate screen, valid for the duration of `with_overlay`.
pub struct OverlayTerminal<'a> {
    terminal: Terminal<OverlayBackend<'a>>,
}

impl OverlayTerminal<'_> {
    pub fn draw(&mut self, f: impl FnOnce(&mut ratatui::Frame)) -> Result<()> {
        self.terminal.draw(f)?;
        Ok(())
    }

    /// `(columns, rows)` of the alternate screen.
    pub fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .map_or((80, 24), |s| (s.width, s.height))
    }
}

/// `CrosstermBackend` over a borrowed writer, with an optional fixed size so overlays can run
/// against a captured writer (tests) where no TTY can be queried.
struct OverlayBackend<'a> {
    inner: CrosstermBackend<&'a mut dyn Write>,
    size_override: Option<(u16, u16)>,
}

impl Backend for OverlayBackend<'_> {
    fn draw<'b, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'b Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if self.size_override.is_some() {
            Ok(Position::new(0, 0))
        } else {
            self.inner.get_cursor_position()
        }
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        match self.size_override {
            Some((width, height)) => Ok(Size::new(width, height)),
            None => self.inner.size(),
        }
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        match self.size_override {
            Some((columns, rows)) => Ok(WindowSize {
                columns_rows: Size::new(columns, rows),
                pixels: Size::new(0, 0),
            }),
            None => self.inner.window_size(),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

// ---------------------------------------------------------------------------------------------
// Rows -> ANSI
// ---------------------------------------------------------------------------------------------

/// SGR-encode each row, clipped to `width` cells (never bisecting a grapheme), control characters
/// removed, with a style reset at the end of every row.
pub fn rows_to_ansi(rows: &[Line<'static>], width: u16) -> Vec<String> {
    rows.iter()
        .map(|line| render_row(line, width as usize).0)
        .collect()
}

/// The encoded row and the number of cells it occupies.
fn render_row(line: &Line<'_>, width: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    let mut current = Style::default();
    'spans: for span in &line.spans {
        let style = line.style.patch(span.style);
        for grapheme in span.content.graphemes(true) {
            if grapheme.chars().any(char::is_control) {
                continue;
            }
            let w = text::width(grapheme);
            if used + w > width {
                break 'spans;
            }
            if style != current {
                if style == Style::default() {
                    out.push_str(RESET);
                } else {
                    push_sgr(&mut out, style);
                }
                current = style;
            }
            out.push_str(grapheme);
            used += w;
        }
    }
    out.push_str(RESET);
    (out, used)
}

/// Absolute SGR for `style` (starts with a reset so stale attributes never leak between spans).
fn push_sgr(out: &mut String, style: Style) {
    out.push_str("\x1b[0");
    let modifiers = style.add_modifier - style.sub_modifier;
    for (flag, code) in [
        (Modifier::BOLD, 1),
        (Modifier::DIM, 2),
        (Modifier::ITALIC, 3),
        (Modifier::UNDERLINED, 4),
        (Modifier::SLOW_BLINK, 5),
        (Modifier::RAPID_BLINK, 6),
        (Modifier::REVERSED, 7),
        (Modifier::HIDDEN, 8),
        (Modifier::CROSSED_OUT, 9),
    ] {
        if modifiers.contains(flag) {
            out.push_str(&format!(";{code}"));
        }
    }
    if let Some(fg) = style.fg {
        push_color(out, fg, false);
    }
    if let Some(bg) = style.bg {
        push_color(out, bg, true);
    }
    out.push('m');
}

fn push_color(out: &mut String, color: Color, background: bool) {
    let base = if background { 40 } else { 30 };
    let named = |offset: u8| format!(";{}", base + offset);
    let bright = |offset: u8| format!(";{}", base + 60 + offset);
    let code = match color {
        Color::Reset => format!(";{}", base + 9),
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => bright(0),
        Color::LightRed => bright(1),
        Color::LightGreen => bright(2),
        Color::LightYellow => bright(3),
        Color::LightBlue => bright(4),
        Color::LightMagenta => bright(5),
        Color::LightCyan => bright(6),
        Color::White => bright(7),
        Color::Rgb(r, g, b) => format!(";{};2;{r};{g};{b}", base + 8),
        Color::Indexed(i) => format!(";{};5;{i}", base + 8),
    };
    out.push_str(&code);
}

#[cfg(test)]
mod tests {
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

        let seen = rig
            .term
            .with_overlay(|overlay| {
                assert_eq!(overlay.size(), (30, 8));
                overlay.draw(|frame| {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("SETTINGS PANEL"),
                        frame.area(),
                    );
                })?;
                Ok(7)
            })
            .unwrap();
        assert_eq!(seen, 7);
        rig.sync();
        let text = String::from_utf8(rig.raw.clone()).unwrap();
        let enter = text
            .find("\x1b[?1049h")
            .expect("enters the alternate screen");
        let leave = text
            .find("\x1b[?1049l")
            .expect("leaves the alternate screen");
        assert!(enter < leave);
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
    fn suspend_rearms_when_the_program_panics() {
        let mut rig = Rig::new(20, 6);
        rig.paint(&["input"], Some((0, 0)));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = rig.term.suspend(|| panic!("editor crashed"));
        }));
        assert!(
            caught.is_err(),
            "the panic propagates after the terminal is re-armed"
        );
        rig.sync();
        let all = String::from_utf8(rig.raw.clone()).unwrap();
        assert!(all.rfind("\x1b[?2004h").unwrap() > all.rfind("\x1b[?2004l").unwrap());
        rig.paint(&["input"], Some((0, 0)));
        assert_eq!(rig.transcript(), ["input"]);
    }

    #[test]
    fn overlay_error_still_leaves_the_alternate_screen() {
        let mut rig = Rig::new(30, 8);
        rig.paint(&["input"], Some((0, 0)));
        let err = rig
            .term
            .with_overlay::<()>(|_| anyhow::bail!("boom"))
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        rig.sync();
        assert!(!rig.parser.screen().alternate_screen());
        rig.paint(&["input"], Some((0, 0)));
        assert_eq!(rig.transcript(), ["input"]);
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
        let row = Line::from(vec![Span::raw("a\x1b[31mb\tc\nd")])
            .style(Style::default().fg(Color::Green));
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
}
