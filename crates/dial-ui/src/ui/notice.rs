//! Command results ("notices"): a section label over a body laid out by the shape of each line, so
//! the plain-text reports commands produce read as structured panels rather than one grey block.
//!
//! Line shapes, checked in order:
//! - a body that is JSON is flattened to `key: value` rows (empty → "Nothing to show.");
//! - `Blocked:` lines and "… is unavailable" notes are flagged as warnings;
//! - two-column rows (`name  description`, two or more spaces apart) keep their column, and a
//!   wrapped description hangs under the description column instead of returning to the margin;
//! - `key: value` pairs, several per line joined by ` · `, show a muted key and a bright value;
//! - a short unindented line that ends in `:` or introduces indented rows is a section heading.
//!
//! Inside any text, commands (`/name`) and key chords (`Ctrl+X`) are highlighted so they read as
//! things the user can type.
use super::{
    Rows,
    chrome::{self, Section},
    text,
    theme::{Sym, Theme, Tok},
    transcript::{TranscriptCtx, fitted},
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

const INDENT: usize = 2;
const MAX_KEY: usize = 32;
const MAX_LEFT: usize = 40;
const MAX_HEADING: usize = 72;
const EMPTY: &str = "Nothing to show.";

type Row = Vec<Span<'static>>;

pub(super) fn rows(title: &str, body: &str, w: u16, ctx: &TranscriptCtx) -> Rows {
    if title == "Notice"
        && let Some(rows) = super::tool_cards::incoming_notice(body, w, ctx)
    {
        return rows;
    }
    let w = w as usize;
    let theme = ctx.theme;
    let title = text::one_line(title);
    let title = title.trim();
    let section = section_of(title);
    let clean = text::sanitize(body);
    let body = json_lines(clean.trim()).unwrap_or_else(|| clean.trim().to_owned());
    if w == 0 || (title.is_empty() && body.is_empty()) {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let indent = if title.is_empty() {
        0
    } else {
        rows.push(fitted(vec![chrome::tag(theme, section, title)], w, theme));
        INDENT.min(w - 1)
    };
    let room = w.saturating_sub(indent).max(1);
    let body = if body.is_empty() {
        EMPTY.to_owned()
    } else {
        body
    };
    let lines: Vec<&str> = body.lines().collect();
    let mut after_blank = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            if !after_blank {
                rows.push(Line::default());
            }
            after_blank = true;
            continue;
        }
        after_blank = false;
        let next_lead = lines[i + 1..]
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(|l| lead(l));
        for row in layout(line, next_lead, room, theme, section) {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(row);
            rows.push(fitted(spans, w, theme));
        }
    }
    rows
}

/// Leading spaces of a line, in cells (tabs count as two).
fn lead(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 2 } else { 1 })
        .sum()
}

fn layout(
    line: &str,
    next_lead: Option<usize>,
    room: usize,
    theme: &Theme,
    section: Section,
) -> Vec<Row> {
    let pad = lead(line).min(room / 2);
    let content = line.trim();
    let gap = |n: usize| Span::raw(" ".repeat(n));
    let muted = theme.fg(Tok::Muted);

    if let Some(rest) = content.strip_prefix("Blocked:") {
        let first = vec![
            gap(pad),
            Span::styled(
                format!("{} Blocked", theme.sym(Sym::Warn)),
                theme.fg(Tok::Warning).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ];
        let hang = pad + text::width(theme.sym(Sym::Warn)) + 1;
        return hang_wrap(first, inline(rest.trim(), muted, theme), hang, room);
    }
    let lower = content.to_ascii_lowercase();
    if [" is unavailable", " are unavailable", "unavailable:"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        let glyph = format!("{} ", theme.sym(Sym::Warn));
        let hang = pad + text::width(&glyph);
        let first = vec![gap(pad), Span::styled(glyph, theme.fg(Tok::Warning))];
        return hang_wrap(first, inline(content, muted, theme), hang, room);
    }
    if let Some((left, spaces, right)) = columns(content) {
        let left_style = if is_command(left) || is_chord(left) {
            theme.fg(Tok::Accent)
        } else {
            theme.fg(Tok::Text)
        };
        let hang = pad + text::width(left) + spaces;
        let first = vec![
            gap(pad),
            Span::styled(left.to_owned(), left_style),
            gap(spaces),
        ];
        if hang < room * 2 / 3 {
            return hang_wrap(first, inline(right, muted, theme), hang, room);
        }
        // Too narrow to keep the column: the description drops below the name.
        let mut rows = hang_wrap(first, Vec::new(), pad, room);
        rows.extend(hang_wrap(
            vec![gap(pad + INDENT)],
            inline(right, muted, theme),
            pad + INDENT,
            room,
        ));
        return rows;
    }
    if pad == 0 && is_heading(content, next_lead) {
        // The label's color and case already mark a heading; a trailing colon is noise.
        let label = content.trim_end_matches(':').trim_end();
        return vec![vec![chrome::tag(theme, section, label)]];
    }
    hang_wrap(vec![gap(pad)], pairs(content, theme), pad, room)
}

/// `name  description`: the first run of two or more spaces after some text splits the columns.
fn columns(content: &str) -> Option<(&str, usize, &str)> {
    let at = content.find("  ")?;
    let left = &content[..at];
    let rest = &content[at..];
    let right = rest.trim_start();
    let spaces = rest.len() - right.len();
    (at > 0 && !right.is_empty() && text::width(left) <= MAX_LEFT && !left.ends_with('.'))
        .then_some((left, spaces, right))
}

/// The label colour of a notice, read from its title.
fn section_of(title: &str) -> Section {
    let t = title.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if has(&["error", "blocked", "failed", "failure"]) {
        Section::Primary
    } else if has(&["memory", "learn", "understand"]) {
        Section::Learn
    } else if has(&["mcp", "extension", "provider", "connect", "collab"]) {
        Section::Connect
    } else if has(&["setup", "doctor", "install"]) {
        Section::Dial
    } else if has(&["sandbox"]) {
        Section::Sandbox
    } else if has(&["tool"]) {
        Section::Tools
    } else if has(&["loop"]) {
        Section::Loop
    } else if has(&["warn"]) {
        Section::Warn
    } else {
        Section::Dial
    }
}

fn is_heading(content: &str, next_lead: Option<usize>) -> bool {
    // `a · b · c` rows and paths are list entries, never headings.
    let colon_free = !content.trim_end_matches(':').contains(": ");
    colon_free
        && !content.contains(" · ")
        && !content.starts_with(['/', '~', '.'])
        && text::width(content) <= MAX_HEADING
        && (content.ends_with(':') || next_lead.is_some_and(|n| n > 0))
}

/// ` · `-joined segments; each `key: value` segment gets a muted key and a bright value.
fn pairs(content: &str, theme: &Theme) -> Row {
    let mut out = Vec::new();
    for (i, segment) in content.split(" · ").enumerate() {
        if i > 0 {
            out.push(Span::styled(
                format!(" {} ", theme.sym(Sym::Dot)),
                theme.fg(Tok::Dim),
            ));
        }
        match segment.split_once(": ") {
            Some((key, value)) if is_key(key) => {
                let value_style = if key.eq_ignore_ascii_case("error") {
                    theme.fg(Tok::Error)
                } else {
                    match value.trim() {
                        "true" | "on" | "enabled" | "yes" => theme.fg(Tok::Success),
                        "false" | "off" | "disabled" | "no" | "none" => theme.fg(Tok::Dim),
                        _ => theme.fg(Tok::Text),
                    }
                };
                out.push(Span::styled(format!("{key}: "), theme.fg(Tok::Muted)));
                out.extend(inline(value, value_style, theme));
            }
            _ => out.extend(inline(segment, theme.fg(Tok::Muted), theme)),
        }
    }
    out
}

fn is_key(key: &str) -> bool {
    !key.is_empty()
        && text::width(key) <= MAX_KEY
        && !key.starts_with('/')
        && !key.contains("://")
        && !key.ends_with('.')
}

fn is_command(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next() == Some('/')
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && !token[1..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .contains('/')
}

fn is_chord(token: &str) -> bool {
    ["Ctrl+", "Alt+", "Option+", "Shift+", "Cmd+"]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

/// Split `text` into spans, giving `/commands` and key chords the accent style.
fn inline(text: &str, base: Style, theme: &Theme) -> Row {
    let mut out: Row = Vec::new();
    let mut plain = String::new();
    let mut previous = ' ';
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let starts_token = previous.is_whitespace() || matches!(previous, '(' | '`' | '"');
        if starts_token && matches!(c, '/' | 'C' | 'A' | 'O' | 'S') {
            let end = rest
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | '`' | '"'))
                .unwrap_or(rest.len());
            let token = rest[..end].trim_end_matches(['.', ':']);
            let highlight = if is_command(token) {
                Some(theme.fg(Tok::MdCode))
            } else if is_chord(token) {
                Some(theme.fg(Tok::Accent))
            } else {
                None
            };
            if let Some(style) = highlight {
                if !plain.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut plain), base));
                }
                out.push(Span::styled(token.to_owned(), style.patch(modifiers(base))));
                rest = &rest[token.len()..];
                previous = token.chars().last().unwrap_or(' ');
                continue;
            }
        }
        plain.push(c);
        previous = c;
        rest = &rest[c.len_utf8()..];
    }
    if !plain.is_empty() {
        out.push(Span::styled(plain, base));
    }
    out
}

fn modifiers(style: Style) -> Style {
    Style::default().add_modifier(style.add_modifier)
}

/// Word-wrap `body` to `room`, the first row led by `first` and every later row by `hang` spaces.
fn hang_wrap(first: Row, body: Row, hang: usize, room: usize) -> Vec<Row> {
    let first_w: usize = first.iter().map(|s| text::width(&s.content)).sum();
    let hang = hang.min(room.saturating_sub(1));
    let mut rows = vec![first];
    let mut used = first_w;
    for span in body {
        for piece in split_words(&span.content) {
            let piece_w = text::width(piece);
            let blank = piece.trim().is_empty();
            if used + piece_w > room && !blank && used > hang {
                trim_end(rows.last_mut().expect("rows starts non-empty"));
                rows.push(vec![Span::raw(" ".repeat(hang))]);
                used = hang;
            }
            if blank && used == hang && rows.len() > 1 {
                continue; // no leading space on a continuation row
            }
            let row = rows.last_mut().expect("rows starts non-empty");
            if used + piece_w <= room {
                push_piece(row, piece, span.style);
                used += piece_w;
                continue;
            }
            // A single word wider than the row: hard-split it.
            for grapheme in piece.graphemes(true) {
                let g = text::width(grapheme);
                if used + g > room {
                    rows.push(vec![Span::raw(" ".repeat(hang))]);
                    used = hang;
                }
                push_piece(
                    rows.last_mut().expect("rows starts non-empty"),
                    grapheme,
                    span.style,
                );
                used += g;
            }
        }
    }
    rows
}

/// Append to the row, extending the last span when the style matches (rows stay a few spans).
fn push_piece(row: &mut Row, piece: &str, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(piece),
        _ => row.push(Span::styled(piece.to_owned(), style)),
    }
}

/// Words and the whitespace runs between them, in order.
fn split_words(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut space = None;
    for (i, c) in text.char_indices() {
        let is_space = c.is_whitespace();
        if space.is_some_and(|s| s != is_space) {
            out.push(&text[start..i]);
            start = i;
        }
        space = Some(is_space);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn trim_end(row: &mut Row) {
    while let Some(last) = row.last() {
        let trimmed = last.content.trim_end().to_owned();
        if trimmed.is_empty() && row.len() > 1 {
            row.pop();
        } else {
            if let Some(last) = row.last_mut() {
                last.content = trimmed.into();
            }
            break;
        }
    }
}

/// A JSON body flattened to readable rows, or `None` when the body is not JSON.
fn json_lines(body: &str) -> Option<String> {
    if !(body.starts_with('{') || body.starts_with('[')) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut out = Vec::new();
    flatten(&value, 0, &mut out);
    Some(if out.is_empty() {
        EMPTY.to_owned()
    } else {
        out.join("\n")
    })
}

fn flatten(value: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    use serde_json::Value;
    let pad = "  ".repeat(depth);
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key = key.replace('_', " ");
                match value {
                    Value::Object(inner) if !inner.is_empty() => {
                        out.push(format!("{pad}{key}"));
                        flatten(value, depth + 1, out);
                    }
                    Value::Array(items) if items.iter().any(|i| i.is_object()) => {
                        out.push(format!("{pad}{key}"));
                        flatten(value, depth + 1, out);
                    }
                    _ => out.push(format!("{pad}{key}: {}", scalar(value))),
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if item.is_object() {
                    if i > 0 {
                        out.push(String::new());
                    }
                    flatten(item, depth, out);
                } else {
                    out.push(format!("{pad}{}", scalar(item)));
                }
            }
        }
        _ => out.push(format!("{pad}{}", scalar(value))),
    }
}

fn scalar(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::String(s) => text::one_line(s),
        Value::Null => "none".into(),
        Value::Array(items) if items.is_empty() => "none".into(),
        Value::Object(map) if map.is_empty() => "none".into(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(" · "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
