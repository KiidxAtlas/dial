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
//! [`InlineTerminal::open_overlay`]; they stay open while the caller keeps working and the normal
//! buffer and cursor are restored by the terminal on [`InlineTerminal::close_overlay`].
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
static UI_THREAD: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";
const WRAP_OFF: &str = "\x1b[?7l";
const WRAP_ON: &str = "\x1b[?7h";
const ALT_ENTER: &str = "\x1b[?1049h";
const ALT_LEAVE: &str = "\x1b[?1049l";
/// Button presses and the wheel (1000) in SGR encoding (1006); only while a panel is open.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";
const RESET: &str = "\x1b[0m";

/// The live region: rows to repaint, where the hardware cursor goes, and an optional window title.
pub struct Viewport<'a> {
    pub rows: &'a [Line<'static>],
    /// `(row in rows, column in cells)`. `None` leaves the cursor hidden.
    pub cursor: Option<(u16, u16)>,
    pub title: Option<&'a str>,
}

/// A painted frame: its rows and the cursor position, if any.
type Frame = (Vec<Line<'static>>, Option<(u16, u16)>);

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
    pending_images: Vec<(
        usize,
        super::images::NativeImage,
        super::images::ImageProtocol,
    )>,
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
    last_frame: Option<Frame>,
    last_rendered: Vec<(String, usize)>,
    /// The open alternate-screen panel, drawn frame by frame (see [`InlineTerminal::open_overlay`]).
    overlay: Option<OverlaySession>,
    /// Report mouse buttons and the wheel while a panel is open (never on the inline screen,
    /// where it would take selection and scrollback away from the terminal).
    overlay_mouse: bool,
}

/// Borrowed drawing surface for a terminal overlay session.
pub struct OverlayTerminal<'a> {
    terminal: &'a mut InlineTerminal,
}

impl OverlayTerminal<'_> {
    /// Draw a frame on the alternate screen; the closure returns the native image placements for it.
    pub fn draw(
        &mut self,
        frame: impl FnOnce(&mut ratatui::Frame) -> Vec<super::images::Placement>,
    ) -> Result<()> {
        self.terminal.draw_overlay_placed(frame)
    }
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
        // Clear the terminal so dial starts at the top.
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
        *UI_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
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
            pending_images: Vec::new(),
            history_written: 0,
            painted_rows: 0,
            painted_widths: Vec::new(),
            cursor_row: 0,
            last_size: (size.0.max(1), size.1.max(1)),
            force_erase: false,
            title: None,
            last_frame: None,
            last_rendered: Vec::new(),
            overlay: None,
            overlay_mouse: false,
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

    pub fn image_protocol(&self) -> Option<super::images::ImageProtocol> {
        self.real
            .then(super::images::ImageProtocol::detect)
            .flatten()
    }

    pub fn commit_with_images(
        &mut self,
        rows: Vec<Line<'static>>,
        images: Vec<(usize, super::images::NativeImage)>,
        protocol: super::images::ImageProtocol,
    ) {
        let offset = self.pending.len();
        self.pending_images.extend(
            images
                .into_iter()
                .filter(|(row, image)| *row < rows.len() && image.rows() <= row + 1)
                .map(|(row, image)| (offset + row, image, protocol)),
        );
        self.commit(rows);
    }

    /// Call on `Event::Resize`: the next paint erases conservatively from the recorded viewport top.
    pub fn on_resize(&mut self) {
        self.force_erase = true;
    }

    /// Number of history rows flushed to the terminal so far.
    #[cfg(test)]
    pub fn history_rows_written(&self) -> usize {
        self.history_written
    }

    /// Number of committed rows still waiting for a paint.
    #[cfg(test)]
    pub fn history_rows_pending(&self) -> usize {
        self.pending.len()
    }

    /// Test seam: pretend the terminal was resized (only meaningful for `with_writer` terminals).
    #[cfg(test)]
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
        let diff = !self.force_erase
            && self.pending.is_empty()
            && (cols, rows) == self.last_size
            && self
                .last_frame
                .as_ref()
                .is_some_and(|(lines, _)| lines.len() == visible.len());
        // Each changed rich row passes through the ANSI primitive once. Stable rows reuse their
        // frame cells and never allocate a new encoded buffer just because a spinner changed.
        let rendered: Vec<Option<(String, usize)>> = visible
            .iter()
            .enumerate()
            .map(|(i, line)| {
                if diff
                    && self
                        .last_frame
                        .as_ref()
                        .is_some_and(|(lines, _)| lines[i] == *line)
                {
                    None
                } else {
                    Some(render_row(line, width))
                }
            })
            .collect();
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

        let mut buf = String::with_capacity(
            256 + rendered
                .iter()
                .flatten()
                .map(|r| r.0.len() + 2)
                .sum::<usize>(),
        );
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
        for (index, (row, _)) in history.iter().enumerate() {
            buf.push_str("\x1b[2K");
            buf.push_str(row);
            for (_, image, protocol) in self
                .pending_images
                .iter()
                .filter(|(at, image, _)| *at == index && image.fits(cols, rows))
            {
                image.write_ansi(&mut buf, *protocol);
            }
            buf.push_str("\r\n");
        }
        for (i, row) in rendered.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            if let Some((row, _)) = row {
                buf.push_str("\x1b[2K");
                buf.push_str(row);
            }
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
        self.pending_images.clear();
        self.painted_rows = n as u16;
        self.last_rendered
            .resize_with(rendered.len(), || (String::new(), 0));
        for (i, row) in rendered.into_iter().enumerate() {
            if let Some(row) = row {
                self.last_rendered[i] = row;
            }
        }
        self.painted_widths = self.last_rendered.iter().map(|r| r.1).collect();
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
        self.pending_images.clear();
        self.painted_widths.clear();
        self.last_frame = None;
        self.cursor_row = 0;
        self.force_erase = false;
        self.last_size = self.size();
        UP_ROWS.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Show a full-screen panel on the alternate screen. The panel stays open across calls, so the
    /// caller can keep running (an agent turn keeps streaming) and draw it with
    /// [`InlineTerminal::draw_overlay`] whenever it likes; [`InlineTerminal::close_overlay`] brings the
    /// inline view back exactly (the terminal restores the normal buffer and cursor; the next paint
    /// redraws the viewport and flushes history committed meanwhile). Idempotent.
    pub fn open_overlay(&mut self) -> Result<()> {
        if self.overlay.is_some() {
            return Ok(());
        }
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
        push_command(&mut enter, &EnableBracketedPaste);
        if self.overlay_mouse {
            enter.push_str(MOUSE_ON);
        }
        self.out.write_all(enter.as_bytes())?;
        self.out.flush()?;
        let buffer = SharedBuffer::default();
        let backend = OverlayBackend {
            inner: CrosstermBackend::new(buffer.clone()),
            size_override: (!self.real).then_some(self.fake_size),
            cursor_hidden: None,
        };
        match Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: RatatuiViewport::Fullscreen,
            },
        ) {
            Ok(terminal) => {
                self.overlay = Some(OverlaySession { terminal, buffer });
                Ok(())
            }
            Err(error) => {
                let _ = self.restore_inline_screen();
                Err(error.into())
            }
        }
    }

    /// Whether a panel currently owns the (alternate) screen.
    #[cfg(test)]
    pub fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// Draw one frame of the open panel as a single synchronized write (unchanged cells are skipped).
    #[cfg(test)]
    pub fn draw_overlay(&mut self, f: impl FnOnce(&mut ratatui::Frame)) -> Result<()> {
        self.draw_overlay_placed(|frame| {
            f(frame);
            Vec::new()
        })
    }

    /// Like [`Self::draw_overlay`], then draws the native image placements the frame returns. Placements
    /// are written only when the terminal speaks a graphics protocol.
    pub fn draw_overlay_placed(
        &mut self,
        f: impl FnOnce(&mut ratatui::Frame) -> Vec<super::images::Placement>,
    ) -> Result<()> {
        let protocol = self.image_protocol();
        let Some(session) = &mut self.overlay else {
            bail!("no panel is open");
        };
        let mut placements = Vec::new();
        session.terminal.draw(|frame| placements = f(frame))?;
        let bytes = std::mem::take(&mut *session.buffer.0.borrow_mut());
        if bytes.is_empty() {
            return Ok(());
        }
        let mut frame = Vec::with_capacity(bytes.len() + SYNC_BEGIN.len() + SYNC_END.len());
        frame.extend_from_slice(SYNC_BEGIN.as_bytes());
        frame.extend_from_slice(&bytes);
        if let Some(protocol) = protocol {
            let mut images = String::new();
            super::images::write_placements(&mut images, protocol, &placements);
            frame.extend_from_slice(images.as_bytes());
        }
        frame.extend_from_slice(SYNC_END.as_bytes());
        self.out.write_all(&frame)?;
        self.out.flush()?;
        Ok(())
    }

    /// `(columns, rows)` of the open panel (the terminal size when none is open).
    #[cfg(test)]
    pub fn overlay_size(&self) -> (u16, u16) {
        match &self.overlay {
            Some(session) => session
                .terminal
                .size()
                .map_or(self.size(), |s| (s.width, s.height)),
            None => self.size(),
        }
    }

    /// Open an overlay for the duration of `f`, restoring the inline screen on success or error.
    pub fn with_overlay<R>(
        &mut self,
        f: impl FnOnce(&mut OverlayTerminal<'_>) -> Result<R>,
    ) -> Result<R> {
        self.open_overlay()?;
        let result = {
            let mut panel = OverlayTerminal { terminal: self };
            f(&mut panel)
        };
        let close = self.close_overlay();
        match (result, close) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Leave the alternate screen; the next paint redraws the inline viewport. Idempotent.
    pub fn close_overlay(&mut self) -> Result<()> {
        if self.overlay.take().is_none() {
            return Ok(());
        }
        if self.image_protocol() == Some(super::images::ImageProtocol::Kitty) {
            self.out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
            self.out.flush()?;
        }
        self.restore_inline_screen()?;
        Ok(())
    }

    /// Ask for mouse reports (wheel and buttons) while a panel is open. Takes effect at the next open.
    pub fn set_overlay_mouse(&mut self, enabled: bool) {
        self.overlay_mouse = enabled;
    }

    fn restore_inline_screen(&mut self) -> io::Result<()> {
        // Harmless when reporting was never enabled; never leaves it on for the inline screen.
        let mut leave = String::from(MOUSE_OFF);
        if self.real && self.keyboard_flags {
            push_command(&mut leave, &PopKeyboardEnhancementFlags);
        }
        leave.push_str(CURSOR_SHOW);
        leave.push_str(ALT_LEAVE);
        push_command(&mut leave, &EnableBracketedPaste);
        let restored = self
            .out
            .write_all(leave.as_bytes())
            .and_then(|()| self.out.flush());
        if self.real {
            IN_ALT_SCREEN.store(false, Ordering::SeqCst);
        }
        self.force_erase = true;
        restored
    }

    /// Erase the live region and park the cursor at column 0 where it began, so the shell prompt
    /// continues below the conversation. Idempotent; also runs from `Drop` and the panic hook.
    pub fn leave(&mut self) {
        if self.left {
            return;
        }
        if self.overlay.take().is_some() {
            let _ = self.restore_inline_screen();
        }
        self.left = true;
        if self.real && !ACTIVE.swap(false, Ordering::SeqCst) {
            return; // the panic hook already restored the terminal
        }
        let (cols, terminal_rows) = self.size();
        let mut s = String::new();
        if self.cursor_row > 0 {
            s.push_str(&format!("\x1b[{}A", self.cursor_row));
        }
        s.push_str("\r\x1b[J");
        for (index, line) in self.pending.iter().enumerate() {
            s.push_str(&render_row(line, cols as usize).0);
            for (_, image, protocol) in self
                .pending_images
                .iter()
                .filter(|(at, image, _)| *at == index && image.fits(cols, terminal_rows))
            {
                image.write_ansi(&mut s, *protocol);
            }
            s.push_str("\r\n");
        }
        self.history_written += self.pending.len();
        self.pending.clear();
        self.pending_images.clear();
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
        // A panel never covers an external program; it comes back afterwards.
        let reopen = self.overlay.is_some();
        self.close_overlay()?;
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
        // A failed raw-mode exit must not return early: the screen and keyboard flags were already
        // handed back, so the program is skipped and the terminal is re-armed below regardless.
        let left = if self.real {
            disable_raw_mode().context("leaving raw mode")
        } else {
            Ok(())
        };
        let value = match left {
            Err(error) => Err(error),
            Ok(()) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(value) => Ok(value),
                Err(panic) => {
                    // The panic hook already restored the terminal (`emergency_restore`). Re-arming here
                    // would leave raw mode, keyboard flags and bracketed paste on once the process unwinds.
                    // The viewport is gone either way, so the next paint must repaint from scratch.
                    self.force_erase = true;
                    self.last_size = self.size();
                    std::panic::resume_unwind(panic)
                }
            },
        };

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
        let rearmed = rearm.is_ok();
        // The program may have scrolled or cleared the screen: repaint from the cursor, erasing below.
        self.force_erase = true;
        self.last_size = self.size();
        let outcome = rearm.and(value);
        if reopen && rearmed {
            self.open_overlay()?;
        }
        outcome
    }

    /// Job-control suspend (Ctrl+Z): restore the terminal, stop dial's process group with SIGTSTP
    /// and, once the shell resumes it (`fg`), re-arm raw mode so the next paint redraws everything.
    /// Without job control (an orphaned process group) the kernel discards the signal and this
    /// returns at once.
    #[cfg(unix)]
    pub fn suspend_process(&mut self) -> Result<()> {
        // SIGTSTP rather than SIGSTOP: an orphaned group ignores it instead of stopping forever.
        self.suspend(|| {
            // SAFETY: kill(2) with pid 0 signals our own process group; no memory is involved.
            unsafe { libc::kill(0, libc::SIGTSTP) };
        })
    }
}

impl Drop for InlineTerminal {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Test constructor: no TTY, no raw mode, no panic hook; everything is written to `out`.
#[cfg(test)]
pub fn with_writer(out: Box<dyn Write>, size: (u16, u16)) -> InlineTerminal {
    InlineTerminal::build(out, size, false, false)
}

/// `REPORT_EVENT_TYPES` makes the terminal send key releases, which hold-to-talk needs. Terminals
/// without the protocol never send them; voice then falls back to press-to-toggle.
const KEYBOARD_FLAGS_WANTED: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS)
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES);

/// True when the terminal accepted the keyboard enhancement flags and so reports key releases.
pub fn reports_key_releases() -> bool {
    KEYBOARD_FLAGS.load(Ordering::SeqCst)
}

fn push_command(into: &mut String, command: &impl Command) {
    // Writing into a String cannot fail.
    let _ = command.write_ansi(into);
}

/// Best-effort restore from a panic: the `InlineTerminal` is not reachable, so statics carry the facts.
fn emergency_restore() {
    let ui_thread = UI_THREAD.lock().ok().and_then(|g| *g);
    // A panic on a worker thread is caught there; tearing down the live UI for it would be wrong.
    if ui_thread != Some(std::thread::current().id()) || !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut s = String::new();
    let keyboard = KEYBOARD_FLAGS.load(Ordering::SeqCst);
    if IN_ALT_SCREEN.swap(false, Ordering::SeqCst) {
        if keyboard {
            push_command(&mut s, &PopKeyboardEnhancementFlags);
        }
        s.push_str(MOUSE_OFF);
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

/// The open panel: a ratatui terminal whose output is collected in memory and written to the
/// real terminal as one synchronized write per frame.
struct OverlaySession {
    terminal: Terminal<OverlayBackend<SharedBuffer>>,
    buffer: SharedBuffer,
}

/// A writer the session keeps a second handle to, so frames can be moved to `InlineTerminal::out`.
#[derive(Clone, Default)]
struct SharedBuffer(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `CrosstermBackend` with an optional fixed size so overlays can run against a captured writer
/// (tests) where no TTY can be queried.
struct OverlayBackend<W: Write> {
    inner: CrosstermBackend<W>,
    size_override: Option<(u16, u16)>,
    /// Last cursor visibility written (`None` until the first call), so an unchanged frame
    /// does not re-send the hide/show that ratatui issues on every draw.
    cursor_hidden: Option<bool>,
}

impl<W: Write> Backend for OverlayBackend<W> {
    fn draw<'b, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'b Cell)>,
    {
        // An empty diff must write nothing: crossterm's draw still emits style resets.
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        if self.cursor_hidden != Some(true) {
            self.inner.hide_cursor()?;
            self.cursor_hidden = Some(true);
        }
        Ok(())
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        if self.cursor_hidden != Some(false) {
            self.inner.show_cursor()?;
            self.cursor_hidden = Some(false);
        }
        Ok(())
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
mod tests;
