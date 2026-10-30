//! `/peers`: the independent Dial sessions working in this checkout, as a board. It shows this
//! session and every live peer with its phase, task summary and advisory path claims (claims that
//! overlap are highlighted), plus how many peer messages wait for this session's model.
//!
//! The panel is a pure view of a [`PeersView`] the controller builds from the agent's coordination
//! state. Refreshing, clearing this session's claims and sending a message are requested through
//! [`OverlayEvent::Action`] strings ([`PeerAction::encode`]); the panel never touches the registry.
use super::{
    DrawCtx, OverlayEvent,
    settings::{Axis, arrows, editor_spans, filled, hint, hint_line, panel, pressed, typed_char},
};
use crate::ui::{
    chrome::Section,
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};

/// One advisory path claim; `overlaps` marks a claim that overlaps one on the other side.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claim {
    pub path: String,
    pub overlaps: bool,
}

/// One session in the checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerRow {
    /// The exact instance id messages are addressed to.
    pub id: String,
    pub phase: String,
    pub summary: String,
    pub claims: Vec<Claim>,
}

/// Everything the panel shows. `own` claims overlap any peer claim; a peer's claims overlap one
/// of `own`'s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeersView {
    pub own: PeerRow,
    pub peers: Vec<PeerRow>,
    /// Peer messages queued for this session that its model has not received yet.
    pub queued: usize,
}

/// A request from the panel to the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerAction {
    Refresh,
    /// Clear this session's own claims.
    Clear,
    Send {
        to: String,
        text: String,
    },
}

const PREFIX: &str = "peers:";

impl PeerAction {
    /// The [`OverlayEvent::Action`] string, carrying the selected peer so a reopened panel keeps it.
    pub fn encode(&self, selected: Option<&str>) -> String {
        let selected = selected.unwrap_or_default();
        match self {
            Self::Refresh => format!("{PREFIX}refresh:{selected}"),
            Self::Clear => format!("{PREFIX}clear:{selected}"),
            Self::Send { to, text } => format!("{PREFIX}send:{to}\n{text}"),
        }
    }

    /// The action and the selected peer id, or `None` for anything that is not a peers action.
    pub fn parse(action: &str) -> Option<(Self, Option<String>)> {
        let rest = action.strip_prefix(PREFIX)?;
        let (head, body) = rest.split_once('\n').unwrap_or((rest, ""));
        let (verb, id) = head.split_once(':').unwrap_or((head, ""));
        let selected = (!id.is_empty()).then(|| id.to_owned());
        match verb {
            "refresh" if body.is_empty() => Some((Self::Refresh, selected)),
            "clear" if body.is_empty() => Some((Self::Clear, selected)),
            "send" if !id.is_empty() && !body.trim().is_empty() => Some((
                Self::Send {
                    to: id.to_owned(),
                    text: body.to_owned(),
                },
                selected,
            )),
            _ => None,
        }
    }
}

/// The short form of an instance id shown in the panel.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub struct PeersOverlay {
    view: PeersView,
    cursor: usize,
    /// The message being typed to the selected peer.
    compose: Option<Editor>,
    message: Option<(String, bool)>,
}

impl PeersOverlay {
    pub fn new(view: PeersView) -> Self {
        Self {
            view,
            cursor: 0,
            compose: None,
            message: None,
        }
    }

    /// Select the peer with `id`; nothing changes when it is no longer live.
    pub fn select(&mut self, id: &str) {
        if let Some(index) = self.view.peers.iter().position(|peer| peer.id == id) {
            self.cursor = index;
        }
    }

    /// The selected peer's id.
    pub fn selected(&self) -> Option<&str> {
        self.view
            .peers
            .get(self.cursor)
            .map(|peer| peer.id.as_str())
    }

    pub fn banner(&mut self, message: String, is_error: bool) {
        self.message = Some((text::one_line(&message), is_error));
    }

    fn request(&self, action: PeerAction) -> OverlayEvent {
        OverlayEvent::Action(action.encode(self.selected()))
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if self.compose.is_some() {
            return self.handle_compose(event, keys);
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
        let last = self.view.peers.len().saturating_sub(1);
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
        } else if key.code == KeyCode::Home && key.modifiers.is_empty() {
            self.cursor = 0;
        } else if key.code == KeyCode::End && key.modifiers.is_empty() {
            self.cursor = last;
        } else if keys.matches(Action::SelectConfirm, key) || typed_char(key) == Some('m') {
            if self.view.peers.is_empty() {
                self.banner("No other session to message".into(), true);
            } else {
                self.compose = Some(Editor::default());
                self.message = None;
            }
        } else {
            match typed_char(key) {
                Some('r') => return self.request(PeerAction::Refresh),
                Some('c') if self.view.own.claims.is_empty() => {
                    self.banner("This session has no claims to clear".into(), false);
                }
                Some('c') => return self.request(PeerAction::Clear),
                _ => {}
            }
        }
        OverlayEvent::None
    }

    fn handle_compose(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Some(editor) = self.compose.as_mut() else {
            return OverlayEvent::None;
        };
        match event {
            // The field is one line; pasted line breaks become spaces.
            Event::Paste(value) => editor.insert(&value.replace("\r\n", " ").replace('\n', " ")),
            Event::Key(key) if pressed(key) => {
                if keys.matches(Action::SelectCancel, key) {
                    self.compose = None;
                } else if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                    let text = editor.text.trim().to_owned();
                    if text.is_empty() {
                        self.banner("Type a message first".into(), true);
                        return OverlayEvent::None;
                    }
                    let Some(to) = self.selected().map(str::to_owned) else {
                        self.compose = None;
                        return OverlayEvent::None;
                    };
                    self.compose = None;
                    return self.request(PeerAction::Send { to, text });
                } else {
                    editor.key_with(*key, keys);
                }
            }
            _ => {}
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let dot = ctx.theme.sym(Sym::Dot);
        let overlapping = self
            .view
            .peers
            .iter()
            .filter(|peer| peer.claims.iter().any(|claim| claim.overlaps))
            .count();
        let mut extra = vec![plural(self.view.peers.len(), "peer", "peers")];
        if overlapping > 0 {
            extra.push(format!("{overlapping} overlapping"));
        }
        if self.view.queued > 0 {
            extra.push(format!("{} unread", self.view.queued));
        }
        panel(
            frame,
            ctx.theme,
            Section::Connect,
            "Project peers",
            &extra.join(&format!(" {dot} ")),
            |w, h| self.body(w, h, ctx),
        );
    }

    fn body(&self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let mut lines = vec![heading(theme, "This session", w)];
        lines.extend(session_lines(theme, &self.view.own, None, w));
        let (inbox, tok) = if self.view.queued > 0 {
            (
                format!(
                    "inbox  {}, delivered to the model at its next request",
                    plural(self.view.queued, "unread message", "unread messages")
                ),
                Tok::Accent,
            )
        } else {
            ("inbox  empty".to_owned(), Tok::Dim)
        };
        lines.push(indented(theme, &inbox, tok, w));
        lines.push(Line::default());
        lines.push(heading(theme, "Other sessions in this checkout", w));
        let mut selected = (lines.len(), lines.len());
        if self.view.peers.is_empty() {
            for line in text::wrap_words(
                "No other Dial sessions are active here. Sessions started in another tab or terminal on this checkout appear here.",
                w.saturating_sub(3).max(1),
            ) {
                lines.push(indented(theme, &line, Tok::Muted, w));
            }
        }
        for (index, peer) in self.view.peers.iter().enumerate() {
            let start = lines.len();
            lines.extend(session_lines(theme, peer, Some(index == self.cursor), w));
            if index == self.cursor {
                selected = (start, lines.len());
            }
        }
        // Footer: an optional message row, an optional compose row, then the key hints.
        let footer = 1 + usize::from(self.compose.is_some()) + usize::from(self.message.is_some());
        let room = h.saturating_sub(footer);
        let offset = selected.1.saturating_sub(room).min(selected.0);
        let mut out: Vec<Line<'static>> = lines.into_iter().skip(offset).take(room).collect();
        while out.len() < room {
            out.push(Line::default());
        }
        if let Some((message, error)) = &self.message {
            let tok = if *error { Tok::Error } else { Tok::Muted };
            out.push(filled(
                vec![Span::styled(format!(" {message}"), theme.fg(tok))],
                w,
                Style::default(),
            ));
        }
        if let Some(editor) = &self.compose {
            let prefix = format!(" To {}: ", short(self.selected().unwrap_or_default()));
            let mut spans = vec![Span::styled(
                prefix.clone(),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            )];
            spans.extend(editor_spans(
                theme,
                editor,
                w.saturating_sub(text::width(&prefix)),
                false,
            ));
            out.push(filled(spans, w, Style::default()));
        }
        out.truncate(h.saturating_sub(1));
        let segments = if self.compose.is_some() {
            vec![
                "Enter send".to_owned(),
                hint(ctx.keys, Action::SelectCancel, "cancel"),
            ]
        } else {
            let mut segments = Vec::new();
            if !self.view.peers.is_empty() {
                segments.push(hint(ctx.keys, Action::SelectConfirm, "message"));
                segments.push(format!("{} select", arrows(theme, Axis::Vertical)));
            }
            segments.push("r refresh".to_owned());
            if !self.view.own.claims.is_empty() {
                segments.push("c clear my claims".to_owned());
            }
            segments.push(hint(ctx.keys, Action::SelectCancel, "close"));
            segments
        };
        out.push(hint_line(theme, &segments, w));
        out
    }
}

fn heading(theme: &Theme, label: &str, w: usize) -> Line<'static> {
    filled(
        vec![Span::styled(
            format!(" {label}"),
            theme.fg(Tok::Muted).add_modifier(Modifier::BOLD),
        )],
        w,
        Style::default(),
    )
}

fn indented(theme: &Theme, value: &str, tok: Tok, w: usize) -> Line<'static> {
    filled(
        vec![Span::styled(format!("   {value}"), theme.fg(tok))],
        w,
        Style::default(),
    )
}

/// Indent of the claim rows, under the id column.
const CLAIM_INDENT: usize = 5;

/// A session's header row (pointer, phase dot, short id, phase, summary) and its claim rows.
/// `selected` is `None` for this session, which has no pointer.
fn session_lines(
    theme: &Theme,
    row: &PeerRow,
    selected: Option<bool>,
    w: usize,
) -> Vec<Line<'static>> {
    let ascii = theme.ascii();
    let is_selected = selected == Some(true);
    let fill = if is_selected {
        theme.bg(Tok::SelectedBg).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let working = row.phase == "working";
    let (dot, tok) = if working {
        (theme.sym(Sym::Working), Tok::Success)
    } else {
        (theme.sym(Sym::Dot), Tok::Muted)
    };
    let pointer = if is_selected {
        theme.sym(Sym::Pointer)
    } else {
        " "
    };
    let phase = text::one_line(&row.phase);
    let phase = if phase.is_empty() {
        "unknown".to_owned()
    } else {
        phase
    };
    let head = [
        Span::styled(format!(" {pointer} "), theme.fg(Tok::Accent).patch(fill)),
        Span::styled(format!("{dot} "), theme.fg(tok).patch(fill)),
        Span::styled(
            text::pad(short(&row.id), 10),
            theme.fg(Tok::Text).patch(fill),
        ),
        Span::styled(
            text::pad(&text::ellipsize(&phase, 9, ascii), 10),
            theme.fg(tok).patch(fill),
        ),
    ];
    let used: usize = head.iter().map(|span| text::width(&span.content)).sum();
    let summary = text::one_line(&row.summary);
    let summary = if summary.trim().is_empty() {
        Span::styled("no task summary", theme.fg(Tok::Dim).patch(fill))
    } else {
        Span::styled(
            text::ellipsize(&summary, w.saturating_sub(used), ascii),
            theme.fg(Tok::Muted).patch(fill),
        )
    };
    let mut spans = head.to_vec();
    spans.push(summary);
    let mut out = vec![filled(spans, w, fill)];
    out.extend(claim_lines(theme, &row.claims, w));
    out
}

/// Claims packed into rows under the header; overlapping claims carry a warning mark and color.
fn claim_lines(theme: &Theme, claims: &[Claim], w: usize) -> Vec<Line<'static>> {
    let indent = " ".repeat(CLAIM_INDENT);
    if claims.is_empty() {
        return vec![filled(
            vec![Span::styled(
                format!("{indent}no claims"),
                theme.fg(Tok::Dim),
            )],
            w,
            Style::default(),
        )];
    }
    let ascii = theme.ascii();
    let room = w.saturating_sub(CLAIM_INDENT).max(1);
    let warn = theme.sym(Sym::Warn);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for claim in claims {
        let mark = if claim.overlaps {
            format!("{warn} ")
        } else {
            String::new()
        };
        // Path tails identify the claim, so long paths keep their end.
        let path = text::ellipsize_start(
            &text::one_line(&claim.path),
            room.saturating_sub(text::width(&mark)).max(1),
            ascii,
        );
        let label = format!("{mark}{path}");
        let cells = text::width(&label);
        if !row.is_empty() && used + 2 + cells > room {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if !row.is_empty() {
            row.push(Span::raw("  "));
            used += 2;
        }
        let style = if claim.overlaps {
            theme.fg(Tok::Warning).add_modifier(Modifier::BOLD)
        } else {
            theme.fg(Tok::Muted)
        };
        row.push(Span::styled(label, style));
        used += cells;
    }
    rows.push(row);
    rows.into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw(indent.clone())];
            spans.extend(row);
            filled(spans, w, Style::default())
        })
        .collect()
}

#[cfg(test)]
mod tests;
