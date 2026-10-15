//! Background processes the agent started with `bg`: pick one, watch its terminal and type into it.
//! The controller feeds the job list and the emulated screen ([`JobsOverlay::set_jobs`],
//! [`JobsOverlay::set_screen`]) and delivers keystrokes it receives as [`OverlayEvent::JobInput`];
//! this panel never touches a process itself.
use super::{
    DrawCtx, OverlayEvent,
    settings::{filled, hint, hint_line, panel, pressed},
};
use crate::ui::{
    chrome::Section,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};

/// One row of the job list, as the controller reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRow {
    pub name: String,
    pub command: String,
    pub status: String,
    pub running: bool,
    /// Runs on a pseudo-terminal, so full-screen programs and line editing work.
    pub terminal: bool,
    /// `session`, `persist` or `detached`.
    pub mode: String,
}

#[derive(Default)]
pub struct JobsOverlay {
    jobs: Vec<JobRow>,
    cursor: usize,
    attached: Option<String>,
    screen: Vec<String>,
    screen_cursor: Option<(u16, u16)>,
    message: Option<(String, bool)>,
    /// List rows drawn by the last frame, so clicks map to the same rows.
    list_h: std::cell::Cell<usize>,
}

/// The chord that leaves an attached process (everything else is typed into it).
fn is_detach(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
}

impl JobsOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the list, keeping the selection on the same job when it still exists.
    pub fn set_jobs(&mut self, jobs: Vec<JobRow>) {
        let selected = self.jobs.get(self.cursor).map(|job| job.name.clone());
        self.jobs = jobs;
        if let Some(name) = selected {
            if let Some(index) = self.jobs.iter().position(|job| job.name == name) {
                self.cursor = index;
            }
        }
        self.cursor = self.cursor.min(self.jobs.len().saturating_sub(1));
        if let Some(name) = &self.attached {
            if !self.jobs.iter().any(|job| &job.name == name) {
                self.message = Some((format!("{name} is no longer in the job table"), true));
                self.attached = None;
                self.screen.clear();
                self.screen_cursor = None;
            }
        }
    }

    /// The job whose terminal is shown and receives keystrokes.
    pub fn attached(&self) -> Option<&str> {
        self.attached.as_deref()
    }

    pub fn set_screen(&mut self, rows: Vec<String>, cursor: Option<(u16, u16)>) {
        self.screen = rows;
        self.screen_cursor = cursor;
    }

    pub fn banner(&mut self, message: String, is_error: bool) {
        self.message = Some((text::one_line(&message), is_error));
    }

    /// Columns and rows available to an attached job's terminal on a `cols`×`rows` screen: the
    /// panel border takes two of each, the header and the footer one row each.
    #[cfg(test)]
    pub fn terminal_size(cols: u16, rows: u16) -> (u16, u16) {
        (cols.saturating_sub(2).max(1), rows.saturating_sub(4).max(1))
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Some(name) = self.attached.clone() {
            return match event {
                Event::Key(key) if pressed(key) => {
                    if is_detach(key) {
                        self.attached = None;
                        self.screen.clear();
                        self.screen_cursor = None;
                        self.message = None;
                        return OverlayEvent::None;
                    }
                    match key_bytes(key) {
                        Some(bytes) => OverlayEvent::JobInput { name, bytes },
                        None => OverlayEvent::None,
                    }
                }
                Event::Paste(value) => OverlayEvent::JobInput {
                    name,
                    bytes: value.replace("\r\n", "\r").replace('\n', "\r").into_bytes(),
                },
                _ => OverlayEvent::None,
            };
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) {
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            return OverlayEvent::Close;
        }
        let last = self.jobs.len().saturating_sub(1);
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
        } else if keys.matches(Action::SelectPageUp, key) {
            self.cursor = 0;
        } else if keys.matches(Action::SelectPageDown, key) {
            self.cursor = last;
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(job) = self.jobs.get(self.cursor) {
                self.attached = Some(job.name.clone());
                self.screen.clear();
                self.screen_cursor = None;
                self.message = (!job.running).then(|| {
                    (
                        format!("{} has exited; showing its last output", job.name),
                        false,
                    )
                });
            }
        }
        OverlayEvent::None
    }

    /// A left click on screen `row` of the list: selects the job drawn there; clicking the selected
    /// job attaches to it, like Enter. The list starts below the panel border.
    pub fn click(&mut self, row: u16) -> OverlayEvent {
        if self.attached.is_some() || self.jobs.is_empty() {
            return OverlayEvent::None;
        }
        let list_h = self.list_h.get().max(1);
        let Some(offset) = usize::from(row)
            .checked_sub(1)
            .filter(|offset| *offset < list_h)
        else {
            return OverlayEvent::None;
        };
        let index = (self.cursor + 1).saturating_sub(list_h) + offset;
        if index >= self.jobs.len() {
            return OverlayEvent::None;
        }
        if index == self.cursor {
            let job = &self.jobs[index];
            self.attached = Some(job.name.clone());
            self.screen.clear();
            self.screen_cursor = None;
            self.message = (!job.running).then(|| {
                (
                    format!("{} has exited; showing its last output", job.name),
                    false,
                )
            });
        } else {
            self.cursor = index;
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let extra = match &self.attached {
            Some(name) => format!("attached to {name}"),
            None => match self.jobs.len() {
                0 => "none running".to_owned(),
                1 => "1 process".to_owned(),
                n => format!("{n} processes"),
            },
        };
        panel(
            frame,
            ctx.theme,
            Section::Sandbox,
            "Background processes",
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
        let mut out = Vec::new();
        let message = |out: &mut Vec<Line<'static>>| {
            if let Some((message, error)) = &self.message {
                let tok = if *error { Tok::Error } else { Tok::Muted };
                out.push(filled(
                    vec![Span::styled(format!(" {message}"), theme.fg(tok))],
                    w,
                    Style::default(),
                ));
            }
        };
        if let Some(name) = &self.attached {
            let job = self.jobs.iter().find(|job| &job.name == name);
            let running = job.is_some_and(|job| job.running);
            // A pending message (an input error, "has exited") replaces the header.
            match (&self.message, job) {
                (Some(_), _) => message(&mut out),
                (None, Some(job)) => out.push(filled(
                    vec![Span::styled(
                        format!(
                            " {} · {} · {}",
                            job.mode,
                            text::one_line(&job.status),
                            text::one_line(&job.command)
                        ),
                        theme.fg(Tok::Muted),
                    )],
                    w,
                    Style::default(),
                )),
                (None, None) => out.push(Line::default()),
            }
            for row in 0..h.saturating_sub(2) {
                let content = self.screen.get(row).map(String::as_str).unwrap_or("");
                let content = text::clip(&text::sanitize(content), w);
                let cursor = self
                    .screen_cursor
                    .filter(|(r, _)| running && usize::from(*r) == row)
                    .map(|(_, c)| usize::from(c));
                out.push(screen_row(&content, cursor, w, theme.fg(Tok::Text)));
            }
            let segments = [
                "Ctrl+] detach".to_owned(),
                if running {
                    "keys go to the process".to_owned()
                } else {
                    "process exited".to_owned()
                },
            ];
            out.truncate(h.saturating_sub(1));
            out.push(hint_line(theme, &segments, w));
            return out;
        }
        if self.jobs.is_empty() {
            out.push(filled(
                vec![Span::styled(
                    " No background processes. The agent starts them with the bg tool.".to_owned(),
                    theme.fg(Tok::Muted),
                )],
                w,
                Style::default(),
            ));
        }
        let list_h = h.saturating_sub(3).max(1);
        self.list_h.set(list_h);
        let scroll = (self.cursor + 1).saturating_sub(list_h);
        for (index, job) in self.jobs.iter().enumerate().skip(scroll).take(list_h) {
            let selected = index == self.cursor;
            let fill = if selected {
                theme.bg(Tok::SelectedBg).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let (dot, tok) = if job.running {
                ("●", Tok::Success)
            } else {
                ("○", Tok::Muted)
            };
            let pointer = if selected {
                theme.sym(Sym::Pointer)
            } else {
                " "
            };
            out.push(filled(
                vec![
                    Span::styled(format!(" {pointer} "), theme.fg(Tok::Accent).patch(fill)),
                    Span::styled(format!("{dot} "), theme.fg(tok).patch(fill)),
                    Span::styled(
                        format!("{:<16} ", job.name),
                        theme.fg(Tok::Text).patch(fill),
                    ),
                    Span::styled(
                        format!(
                            "{:<9} {:<12} {}",
                            job.mode,
                            text::one_line(&job.status),
                            if job.terminal { "pty " } else { "" }
                        ),
                        theme.fg(Tok::Muted).patch(fill),
                    ),
                    Span::styled(text::one_line(&job.command), theme.fg(Tok::Dim).patch(fill)),
                ],
                w,
                fill,
            ));
        }
        while out.len() < h.saturating_sub(2) {
            out.push(Line::default());
        }
        message(&mut out);
        out.truncate(h.saturating_sub(1));
        let segments = vec![
            hint(ctx.keys, Action::SelectConfirm, "attach"),
            hint(ctx.keys, Action::SelectCancel, "close"),
        ];
        out.push(hint_line(theme, &segments, w));
        out
    }
}

/// One emulated terminal row, with the job's cursor drawn as a reversed cell.
fn screen_row(content: &str, cursor: Option<usize>, width: usize, style: Style) -> Line<'static> {
    let Some(column) = cursor.filter(|column| *column < width) else {
        return filled(
            vec![Span::styled(content.to_owned(), style)],
            width,
            Style::default(),
        );
    };
    let mut before = String::new();
    let mut at = String::from(" ");
    let mut after = String::new();
    let mut used = 0;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(content, true) {
        let cells = text::width(grapheme);
        if used < column {
            before.push_str(grapheme);
        } else if used == column {
            at = grapheme.to_owned();
        } else {
            after.push_str(grapheme);
        }
        used += cells.max(1);
    }
    if used < column {
        before.push_str(&" ".repeat(column - used));
    }
    filled(
        vec![
            Span::styled(before, style),
            Span::styled(at, style.add_modifier(Modifier::REVERSED)),
            Span::styled(after, style),
        ],
        width,
        Style::default(),
    )
}

/// The bytes a terminal sends for `key` (xterm conventions), or `None` for keys with no encoding.
pub fn key_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let lower = c.to_ascii_lowercase();
            match lower {
                'a'..='z' => vec![lower as u8 - b'a' + 1],
                '@' | ' ' | '2' => vec![0],
                '[' | '3' => vec![0x1b],
                '\\' | '4' => vec![0x1c],
                ']' | '5' => vec![0x1d],
                '^' | '6' => vec![0x1e],
                '_' | '-' | '7' => vec![0x1f],
                '?' | '8' => vec![0x7f],
                _ => c.to_string().into_bytes(),
            }
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + (n - 1)],
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            format!("\x1b[{code}~").into_bytes()
        }
        _ => return None,
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press)
    }

    fn row(name: &str, running: bool) -> JobRow {
        JobRow {
            name: name.into(),
            command: format!("run {name}"),
            status: if running { "running" } else { "exited 0" }.into(),
            running,
            terminal: true,
            mode: "session".into(),
        }
    }

    #[test]
    fn keys_encode_like_a_terminal() {
        assert_eq!(
            key_bytes(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes(&key(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            key_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_bytes(&key(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&key(KeyCode::F(5), KeyModifiers::NONE)),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            key_bytes(&key(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn attach_forwards_keys_and_detach_returns_to_the_list() {
        let keys = Keymap::defaults();
        let mut overlay = JobsOverlay::new();
        overlay.set_jobs(vec![row("server", true), row("watch", true)]);
        overlay.handle(&Event::Key(key(KeyCode::Down, KeyModifiers::NONE)), &keys);
        overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
        assert_eq!(overlay.attached(), Some("watch"));
        // Esc and Ctrl+C belong to the process while attached; they never close the panel.
        match overlay.handle(&Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)), &keys) {
            OverlayEvent::JobInput { name, bytes } => {
                assert_eq!(name, "watch");
                assert_eq!(bytes, vec![0x1b]);
            }
            other => panic!("expected input, got {other:?}"),
        }
        match overlay.handle(&Event::Paste("a\nb".into()), &keys) {
            OverlayEvent::JobInput { bytes, .. } => assert_eq!(bytes, b"a\rb".to_vec()),
            other => panic!("expected input, got {other:?}"),
        }
        overlay.handle(
            &Event::Key(key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            &keys,
        );
        assert_eq!(overlay.attached(), None);
        assert!(matches!(
            overlay.handle(&Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn a_vanished_job_detaches_and_the_selection_follows_names() {
        let keys = Keymap::defaults();
        let mut overlay = JobsOverlay::new();
        overlay.set_jobs(vec![row("a", true), row("b", true)]);
        overlay.handle(&Event::Key(key(KeyCode::Down, KeyModifiers::NONE)), &keys);
        overlay.set_jobs(vec![row("new", true), row("a", true), row("b", false)]);
        overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
        assert_eq!(overlay.attached(), Some("b"));
        overlay.set_jobs(vec![row("a", true)]);
        assert_eq!(overlay.attached(), None);
    }

    #[test]
    fn terminal_size_leaves_room_for_the_frame() {
        assert_eq!(JobsOverlay::terminal_size(80, 24), (78, 20));
        assert_eq!(JobsOverlay::terminal_size(1, 1), (1, 1));
    }
}
