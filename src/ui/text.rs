//! Terminal-safe text primitives shared by every UI component. Everything is measured in terminal
//! cells (extended grapheme clusters, East Asian width), never bytes or `char`s.
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Strip terminal protocol sequences, C0/C1 controls and bidi overrides so untrusted text (model
/// output, tool output, file contents) can never move the cursor, set the title or recolor the
/// screen. Newlines stay; tabs expand to four spaces. Run on whole messages, not single streaming
/// deltas, so a split OSC/CSI sequence cannot slip through.
pub fn sanitize(input: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        String,
        StringEscape,
    }
    let mut state = State::Text;
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        state = match state {
            State::Text => match c {
                '\u{1b}' => State::Escape,
                '\u{9b}' => State::Csi,
                '\u{90}' | '\u{9d}' | '\u{98}' | '\u{9e}' | '\u{9f}' => State::String,
                '\n' => {
                    out.push('\n');
                    State::Text
                }
                '\t' => {
                    out.push_str("    ");
                    State::Text
                }
                c if c.is_control()
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}') =>
                {
                    State::Text
                }
                c => {
                    out.push(c);
                    State::Text
                }
            },
            State::Escape => match c {
                '[' => State::Csi,
                ']' | 'P' | 'X' | '^' | '_' => State::String,
                '\u{20}'..='\u{2f}' => State::Escape,
                _ => State::Text,
            },
            State::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::String => match c {
                '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
            State::StringEscape => match c {
                '\\' | '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::String,
            },
        };
    }
    out
}

/// Sanitized, single-line form for labels and status segments.
pub fn one_line(text: &str) -> String {
    sanitize(text).replace('\n', " ")
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| width(&s.content)).sum()
}

/// Cell-safe clipping: never bisect a wide or combining grapheme.
pub fn clip(text: &str, max: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|g| {
            used += width(g);
            used <= max
        })
        .collect()
}

/// Clip to `max` cells, ending in `…` (or `...` when `ascii`) if anything was cut.
pub fn ellipsize(text: &str, max: usize, ascii: bool) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mark = if ascii { "..." } else { "…" };
    let room = max.saturating_sub(width(mark));
    format!("{}{mark}", clip(text, room))
}

/// Pad (never truncate) with spaces to exactly `cells` when shorter.
pub fn pad(text: &str, cells: usize) -> String {
    let used = width(text);
    if used >= cells {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(cells - used))
    }
}

/// Hard-wrap styled lines at `max` cells, preserving each span's style. Wide graphemes that cannot
/// fit even on an empty row are replaced by `?` so wrapping always terminates.
pub fn wrap(lines: Vec<Line<'static>>, max: usize) -> Vec<Line<'static>> {
    if max == 0 {
        return Vec::new();
    }
    let mut result = Vec::new();
    for line in lines {
        let style = line.style;
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut cells = 0;
        for span in line.spans {
            let mut run = String::new();
            for grapheme in span.content.graphemes(true) {
                let grapheme = if width(grapheme) > max { "?" } else { grapheme };
                let size = width(grapheme);
                if cells + size > max {
                    if !run.is_empty() {
                        row.push(Span::styled(std::mem::take(&mut run), span.style));
                    }
                    result.push(Line::from(std::mem::take(&mut row)).style(style));
                    cells = 0;
                }
                run.push_str(grapheme);
                cells += size;
            }
            if !run.is_empty() {
                row.push(Span::styled(run, span.style));
            }
        }
        result.push(Line::from(row).style(style));
    }
    result
}

/// Word-aware wrap of plain text into rows of at most `max` cells (words longer than a row are
/// hard-split). Used for prose; code and tables must use [`wrap`].
pub fn wrap_words(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split(' ') {
            let w = width(word);
            if w > max {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                let mut piece = String::new();
                for g in word.graphemes(true) {
                    if width(&piece) + width(g) > max {
                        rows.push(std::mem::take(&mut piece));
                    }
                    piece.push_str(g);
                }
                row = piece;
                continue;
            }
            let needed = if row.is_empty() {
                w
            } else {
                width(&row) + 1 + w
            };
            if needed > max {
                rows.push(std::mem::take(&mut row));
                row.push_str(word);
            } else {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(word);
            }
        }
        rows.push(row);
    }
    rows
}

/// Right-align `right` against `left` on a row of exactly `total` cells, dropping `left` characters
/// (never `right`) when they do not both fit. Returns spans ready to render.
pub fn justify(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    total: usize,
    fill: Style,
) -> Vec<Span<'static>> {
    let (lw, rw): (usize, usize) = (
        left.iter().map(|s| width(&s.content)).sum(),
        right.iter().map(|s| width(&s.content)).sum(),
    );
    if lw + rw < total {
        let mut out = left;
        out.push(Span::styled(" ".repeat(total - lw - rw), fill));
        out.extend(right);
        return out;
    }
    let room = total.saturating_sub(rw + 1);
    let mut out = Vec::new();
    let mut used = 0;
    for span in left {
        let w = width(&span.content);
        if used + w <= room {
            used += w;
            out.push(span);
            continue;
        }
        let keep = clip(&span.content, room - used);
        used += width(&keep);
        if !keep.is_empty() {
            out.push(Span::styled(keep, span.style));
        }
        break;
    }
    out.push(Span::styled(
        " ".repeat(total.saturating_sub(used + rw)),
        fill,
    ));
    out.extend(right);
    out
}

/// Subsequence fuzzy match. Higher is better; `positions` are char indexes into `text` that matched
/// (for highlighting). Prefix and word-boundary hits and consecutive runs score higher; a query that
/// is not a subsequence of `text` does not match. Case-insensitive; an empty query matches everything.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let hay: Vec<char> = text.chars().collect();
    let low: Vec<char> = text.to_lowercase().chars().collect();
    // Lowercasing can change length for exotic scripts; fall back to exact char comparison then.
    let low = if low.len() == hay.len() {
        low
    } else {
        hay.clone()
    };
    let mut positions = Vec::new();
    let mut score = 0i32;
    let mut at = 0usize;
    let mut last: Option<usize> = None;
    for q in query.to_lowercase().chars() {
        let found = (at..low.len()).find(|&i| low[i] == q)?;
        let boundary = found == 0 || !hay[found - 1].is_alphanumeric();
        score += 10;
        if found == 0 {
            score += 30;
        }
        if boundary {
            score += 15;
        }
        if last.is_some_and(|l| l + 1 == found) {
            score += 20;
        }
        score -= (found - last.map_or(0, |l| l + 1)).min(10) as i32;
        positions.push(found);
        last = Some(found);
        at = found + 1;
    }
    score -= (hay.len() as i32 - positions.len() as i32).min(20) / 4;
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_terminal_control_but_keeps_text_and_newlines() {
        assert_eq!(
            sanitize("a\x1b[31mred\x1b[0m\x1b]0;owned\x07 b\tc\nd\u{202e}e\x07"),
            "ared b    c\nde"
        );
        assert_eq!(sanitize("\u{9d}title\u{9c}ok"), "ok");
    }

    #[test]
    fn clip_and_wrap_never_split_wide_or_combining_graphemes() {
        assert_eq!(clip("日本語", 5), "日本");
        assert_eq!(clip("e\u{301}x", 1), "e\u{301}");
        let rows = wrap(vec![Line::from("日本語日本語")], 5);
        assert!(rows.iter().all(|r| line_width(r) <= 5), "{rows:?}");
        assert_eq!(
            rows.iter().map(|r| r.to_string()).collect::<String>(),
            "日本語日本語"
        );
        assert_eq!(
            wrap(vec![Line::from("日")], 1).len(),
            1,
            "an unfittable grapheme terminates as ?"
        );
    }

    #[test]
    fn justify_keeps_the_right_side_and_trims_the_left() {
        let s = |t: &str| Span::raw(t.to_owned());
        let row = justify(
            vec![s("model name that is long")],
            vec![s("42%")],
            14,
            Style::default(),
        );
        let text: String = row.iter().map(|x| x.content.as_ref()).collect();
        assert_eq!(width(&text), 14);
        assert!(
            text.ends_with("42%") && text.starts_with("model"),
            "{text:?}"
        );
    }

    #[test]
    fn fuzzy_ranks_prefix_and_boundary_matches_first_and_rejects_non_subsequences() {
        fn rank<'a>(q: &str, items: &[&'a str]) -> Vec<&'a str> {
            let mut found: Vec<(i32, &'a str)> = items
                .iter()
                .filter_map(|t| fuzzy(q, t).map(|(s, _)| (s, *t)))
                .collect();
            found.sort_by(|a, b| b.0.cmp(&a.0));
            found.into_iter().map(|(_, t)| t).collect()
        }
        assert_eq!(
            rank("mo", &["remove-dir", "model", "memory", "modelpreset"])[0],
            "model"
        );
        assert!(fuzzy("zz", "model").is_none());
        assert_eq!(fuzzy("", "anything").unwrap().0, 0);
        assert_eq!(fuzzy("st", "settings").unwrap().1, vec![0, 2]);
        assert!(fuzzy("MODEL", "model").is_some(), "case-insensitive");
    }

    #[test]
    fn word_wrap_breaks_on_spaces_and_hard_splits_long_words() {
        assert_eq!(wrap_words("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap_words("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_words("a\n\nb", 5), vec!["a", "", "b"]);
    }
}
