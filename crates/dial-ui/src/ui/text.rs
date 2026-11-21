//! Terminal-safe text primitives shared by every UI component. Everything is measured in terminal
//! cells (extended grapheme clusters, East Asian width), never bytes or `char`s.
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Strip terminal protocol sequences, C0/C1 controls and bidi overrides so untrusted text (model
/// output, tool output, file contents) can never move the cursor, set the title or recolor the
/// screen. Newlines stay; tabs expand to four spaces. Run on whole messages, not single streaming
/// deltas, so a split OSC/CSI sequence cannot slip through.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    Sanitizer::default().feed(input, &mut out);
    out
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum SanitizeState {
    #[default]
    Text,
    Escape,
    Csi,
    String,
    StringEscape,
}

/// Streaming form of [`sanitize`]: feeding a text in pieces appends exactly what `sanitize` of the
/// whole text would produce, because a half-received escape sequence carries over to the next
/// piece. Output for a prefix is final: later input only appends.
#[derive(Clone, Copy, Default, Debug)]
pub struct Sanitizer {
    state: SanitizeState,
}

impl Sanitizer {
    pub fn feed(&mut self, input: &str, out: &mut String) {
        use SanitizeState as State;
        let mut state = self.state;
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
                // A newline ends any unfinished sequence and is kept: a stray ESC cannot join two
                // lines and an unterminated CSI, OSC or DCS cannot hide the rest of the output.
                _ if c == '\n' => {
                    out.push('\n');
                    State::Text
                }
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
        self.state = state;
    }
}

/// Sanitized, single-line form for labels and status segments.
pub fn one_line(text: &str) -> String {
    sanitize(text).replace('\n', " ")
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn line_width(line: &Line<'_>) -> usize {
    spans_width(&line.spans)
}

/// Cells taken by a run of styled spans.
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
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

/// Clip styled spans to `max` cells. When something is cut, the last cells become `ellipsis` (pass
/// `""` for a plain clip), drawn in the style of the last span that kept text. Never splits a
/// grapheme; spans that already fit come back unchanged.
pub fn truncate_spans(spans: Vec<Span<'static>>, max: usize, ellipsis: &str) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mark_w = width(ellipsis);
    let (room, mark) = if mark_w <= max {
        (max - mark_w, ellipsis)
    } else {
        (max, "")
    };
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut kept = String::new();
        let mut cut = false;
        for grapheme in span.content.graphemes(true) {
            let w = width(grapheme);
            if used + w > room {
                cut = true;
                break;
            }
            kept.push_str(grapheme);
            used += w;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if cut {
            break;
        }
    }
    if !mark.is_empty() {
        let style = out.last().map_or(Style::default(), |span| span.style);
        out.push(Span::styled(mark.to_owned(), style));
    }
    out
}

/// Remove the last grapheme cluster, the unit a user sees as one character (an accented letter,
/// an emoji with modifiers, a flag). Returns whether anything was removed.
pub fn pop_grapheme(text: &mut String) -> bool {
    match text.grapheme_indices(true).next_back() {
        Some((start, _)) => {
            text.truncate(start);
            true
        }
        None => false,
    }
}

/// The mark that ends a cut row: `…`, or `...` on ASCII terminals.
fn ellipsis_mark(ascii: bool) -> &'static str {
    if ascii { "..." } else { "…" }
}

/// Clip to `max` cells, ending in `…` (or `...` when `ascii`) if anything was cut.
pub fn ellipsize(text: &str, max: usize, ascii: bool) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mark = ellipsis_mark(ascii);
    if max < width(mark) {
        return clip(mark, max);
    }
    let room = max - width(mark);
    format!("{}{mark}", clip(text, room))
}

/// [`ellipsize`] that prefers to cut between words: the mark follows the last whole word that
/// fits, unless that would leave less than half the row, where a grapheme cut reads better.
/// `force` marks text that continues elsewhere even though this row fits (a hidden second row).
pub fn ellipsize_words(text: &str, max: usize, ascii: bool, force: bool) -> String {
    if !force && width(text) <= max {
        return text.to_owned();
    }
    let mark = ellipsis_mark(ascii);
    if max < width(mark) {
        return clip(mark, max);
    }
    let room = max - width(mark);
    let clipped = clip(text, room);
    // Whole when the clip ended at a word end: the text continues with a space or not at all.
    let whole = text[clipped.len()..].starts_with(' ') || clipped.len() == text.len();
    let kept = match clipped.rfind(' ') {
        _ if whole => clipped.as_str(),
        Some(at) if width(&clipped[..at]) * 2 >= room => &clipped[..at],
        _ => clipped.as_str(),
    };
    format!("{}{mark}", kept.trim_end())
}

/// Keep the end of `text` within `max` cells, starting with `…` (or `...` when `ascii`) if anything
/// was cut. For paths, where the tail is what identifies the thing.
pub fn ellipsize_start(text: &str, max: usize, ascii: bool) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mark = ellipsis_mark(ascii);
    if max < width(mark) {
        return clip(mark, max);
    }
    let room = max - width(mark);
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        used += width(grapheme);
        if used > room {
            break;
        }
        kept.push(grapheme);
    }
    kept.reverse();
    format!("{mark}{}", kept.concat())
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
///
/// A paragraph's leading spaces are kept as a hanging indent (capped at half the row): its first
/// row starts with them and continuation rows hang under the paragraph's own text, so pasted code,
/// stack traces and compiler carets keep their shape.
pub fn wrap_words(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let body = paragraph.trim_start_matches(' ');
        if body.is_empty() {
            rows.push(String::new());
            continue;
        }
        let indent = (paragraph.len() - body.len()).min(max / 2);
        if indent == 0 {
            wrap_paragraph(body, max, &mut rows);
            continue;
        }
        let pad = " ".repeat(indent);
        let start = rows.len();
        wrap_paragraph(body, max - indent, &mut rows);
        for row in &mut rows[start..] {
            row.insert_str(0, &pad);
        }
    }
    rows
}

/// [`wrap_words`] of one paragraph with no leading spaces, appended to `rows`.
fn wrap_paragraph(paragraph: &str, max: usize, rows: &mut Vec<String>) {
    let mut row = String::new();
    for word in paragraph.split(' ') {
        let w = width(word);
        if w > max {
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            let mut piece = String::new();
            for g in word.graphemes(true) {
                // As in `wrap`: a grapheme wider than a whole row becomes `?`.
                let g = if width(g) > max { "?" } else { g };
                if !piece.is_empty() && width(&piece) + width(g) > max {
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

/// A span of time in whole seconds, in the one compact form every surface uses: `0s`, `45s`,
/// `12m34s`, `1h15m`. Partial seconds are dropped (an elapsed clock never runs ahead); negative or
/// non-finite input reads as `0s`. Use [`duration_label_precise`] for measured run times.
pub fn duration_label(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    };
    match total {
        0..=59 => format!("{total}s"),
        60..=3599 => format!("{}m{:02}s", total / 60, total % 60),
        _ => format!("{}h{:02}m", total / 3600, total % 3600 / 60),
    }
}

/// [`duration_label`] with tenths under ten seconds (`<0.1s`, `0.4s`, `3.2s`), for measured run
/// times of tools and turns where a fast result is worth telling apart.
pub fn duration_label_precise(secs: f64) -> String {
    if !(secs.is_finite() && secs > 0.0) {
        return "0s".to_owned();
    }
    let tenths = (secs * 10.0).round() as u64;
    match tenths {
        0 => "<0.1s".to_owned(),
        1..=99 => format!("{}.{}s", tenths / 10, tenths % 10),
        _ => duration_label(secs.max(10.0)),
    }
}

/// A count in the one compact form every surface uses: `999`, `1.5k`, `32k`, `172k`, `1.2M`, `42M`.
/// Floored, so a used amount never looks larger than it is.
pub fn compact_count(n: u64) -> String {
    let scaled = |unit: u64, suffix: &str| {
        let tenths = n / (unit / 10);
        if tenths.is_multiple_of(10) {
            format!("{}{suffix}", tenths / 10)
        } else {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => scaled(1_000, "k"),
        10_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=9_999_999 => scaled(1_000_000, "M"),
        _ => format!("{}M", n / 1_000_000),
    }
}

/// The plural suffix of a counted noun: `""` for one, `"s"` otherwise (`1 session`, `2 sessions`).
pub fn plural_s<N: From<u8> + PartialEq>(n: N) -> &'static str {
    if n == N::from(1) { "" } else { "s" }
}

/// A count with its noun, pluralised by `plural_s`: `1 file`, `3 files`, `0 files`.
pub fn counted<N: From<u8> + PartialEq + Copy + std::fmt::Display>(n: N, noun: &str) -> String {
    format!("{n} {noun}{}", plural_s(n))
}

/// A count whose plural is not a plain `s` on the noun: each form is given explicitly, such as the
/// invariant `failed` (`1 failed` / `2 failed`) or `child` / `children`.
pub fn count_with(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Compact elapsed time in one unit, floored: `45s`, `12m`, `3h`, `2d`. Callers add a suffix such as
/// `ago` when the context needs one.
pub fn age_compact(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Observed USD in the one `$` form of the status line, worker rows and cost notices: cents
/// (`$0.14`, `$12.35`), with four decimals only below a cent (`$0.0041`) so small local spend still
/// reads. Zero (or unknown garbage) is `$0.00`; a positive amount that rounds to nothing is
/// `<$0.0001`, never a misleading zero.
pub fn usd(amount: f64) -> String {
    if !(amount.is_finite() && amount > 0.0) {
        "$0.00".to_owned()
    } else if amount >= 0.01 {
        format!("${amount:.2}")
    } else if amount < 0.000_05 {
        "<$0.0001".to_owned()
    } else {
        format!("${amount:.4}")
    }
}

/// Right-align `right` against `left` on a row of exactly `total` cells, dropping `left` characters
/// (never `right`) when they do not both fit. Returns spans ready to render.
#[cfg(test)]
pub fn justify(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    total: usize,
    fill: Style,
) -> Vec<Span<'static>> {
    let (lw, rw) = (spans_width(&left), spans_width(&right));
    if lw + rw < total {
        let mut out = left;
        out.push(Span::styled(" ".repeat(total - lw - rw), fill));
        out.extend(right);
        return out;
    }
    let room = total.saturating_sub(rw + 1);
    let mut out = truncate_spans(left, room, "");
    let used = spans_width(&out);
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

/// Whether every word of `query` starts a word of `text` (case-insensitive): `them` and `ui the`
/// do for "ui.theme", `hm` does not. Prose is long enough to contain almost any scattered letters,
/// so a description counts as a match only on a word piece like this.
pub fn word_prefix(query: &str, text: &str) -> bool {
    let text = text.to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let mut any = false;
    for needle in query.split_whitespace() {
        let needle = needle.to_lowercase();
        let needle = needle.trim_matches(|c: char| !c.is_alphanumeric());
        if needle.is_empty() {
            continue;
        }
        any = true;
        // A needle with inner punctuation (`ui.the`) is matched against the whole text instead.
        let found = if needle.contains(|c: char| !c.is_alphanumeric()) {
            text.contains(needle)
        } else {
            words.iter().any(|word| word.starts_with(needle))
        };
        if !found {
            return false;
        }
    }
    any
}

/// [`fuzzy`] over prose (descriptions, combined metadata): only when [`word_prefix`] holds, so
/// `theme` never matches a description that merely contains t, h, e, m and e in order.
pub fn fuzzy_prose(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    if query.trim().is_empty() {
        return fuzzy(query, text);
    }
    word_prefix(query, text)
        .then(|| fuzzy(query, text))
        .flatten()
}

/// [`fuzzy`] over a short name, rejecting letters picked from across the name: the hits must lie
/// within half again the query's length (plus one), unless the query starts a word of the name.
/// `st` and `sndbx` still find "settings" and "sandbox"; `se` no longer finds "shake".
pub fn fuzzy_compact(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let (score, hits) = fuzzy(query, text)?;
    let n = query.chars().count();
    let compact = hits
        .first()
        .zip(hits.last())
        .is_none_or(|(first, last)| last - first < n + n / 2 + 1);
    (compact || word_prefix(query, text)).then_some((score, hits))
}

#[cfg(test)]
mod tests;

/// Error chains as `anyhow`'s `{:#}` prints them ("A: B: C: D") read as one long clause. Keep
/// what the user acts on: the first two clauses (what failed and the immediate reason) and the
/// root cause. Short text and ordinary colons are left alone.
pub fn brief_chain(text: &str) -> String {
    const MAX_FLAT: usize = 140;
    if text.chars().count() <= MAX_FLAT || text.contains('\n') {
        return text.to_owned();
    }
    let parts: Vec<&str> = text.split(": ").collect();
    if parts.len() < 4 {
        return text.to_owned();
    }
    format!("{}: {} … {}", parts[0], parts[1], parts[parts.len() - 1])
}

#[cfg(test)]
mod brief_chain_tests;
