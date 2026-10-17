//! Bounded stripping of read/search display prefixes from ordinary file writes.
//!
//! Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
//! Copyright (c) 2025 Mario Zechner; Copyright (c) 2025-2026 Can Bölük;
//! Copyright (c) 2026 Stencil Labs, Inc. See this native module's `mod.rs` for
//! the full MIT permission notice and warranty disclaimer.

use std::sync::LazyLock;

use regex::Regex;

static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:>>>|>>)\s*)?(?:[+*-]\s*)?\d+[:|]").expect("valid prefix regex")
});
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\[[^#\r\n]+#[0-9a-fA-F]{4}\]\s*$").expect("valid header regex")
});
static RANGE_ELISION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[1-9]\d*\s*-\s*[1-9]\d*:.*(?:…|\.\.\.).*$").expect("valid range elision regex")
});
static SINGLE_ELISION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:…|\.\.\.)\s*$").expect("valid elision regex"));

/// Remove display prefixes only if every nonempty content row has one.
///
/// `None` means the exact input must be retained. Canonical `[PATH#TAG]` headers
/// are removed wherever they occur; a loose header is accepted only at the
/// first nonempty row, as in the upstream write wrapper. Empty rows, trailing
/// newlines, CR characters in row bodies, and all unprefixed ordinary content
/// retain the upstream split-on-LF semantics. Each row yields at most one leading
/// prefix, and stripping requires prefixes numbered `1..=n` in row order plus a
/// header or complete footer as read evidence.
///
/// Recognized complete-view footers and engine-owned projection metadata are
/// skipped. Incomplete read notices and elisions instead leave input unchanged;
/// callers must reject them using [`incomplete_projection`] before writing.
/// The caller must restrict this helper to ordinary filesystem writes in
/// active Hashline mode, never owned handlers or device payloads.
pub fn strip(content: &str) -> Option<String> {
    if read_is_incomplete(content) {
        return None;
    }
    if let Some(cleaned) = strip_rows(content, None) {
        return Some(cleaned);
    }

    let (index, header) = content
        .split('\n')
        .enumerate()
        .find(|(_, line)| !line.trim_matches(js_whitespace).is_empty())?;
    if !is_loose_header(header) {
        return None;
    }
    strip_rows(content, Some(index))
}

/// Whether a copied hashline projection visibly omits source content.
///
/// Requires a canonical or loose header at the first nonempty row, avoiding
/// interpretation of ordinary unprefixed file text as a read projection.
/// Complete range footers are accepted only when both their declared ranges
/// and the actual numbered rows cover exactly `1..=total` without gaps.
pub fn incomplete_projection(content: &str) -> bool {
    content
        .split('\n')
        .find(|line| !line.trim_matches(js_whitespace).is_empty())
        .is_some_and(|line| HEADER.is_match(line) || is_loose_header(line))
        && read_is_incomplete(content)
}

fn strip_rows(content: &str, loose_header: Option<usize>) -> Option<String> {
    let mut has_content = false;
    let mut evidence = loose_header.is_some();
    let mut expected = 0usize;
    for (index, line) in content.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        if HEADER.is_match(line) {
            evidence = true;
            continue;
        }
        if loose_header == Some(index) || is_owned_metadata(line) {
            continue;
        }
        if read_footer(line).is_some() {
            evidence = true;
            continue;
        }
        let prefix = PREFIX.find(line)?;
        expected += 1;
        if outer_line_number(&line[..prefix.end()]) != Some(expected) {
            return None;
        }
        has_content = true;
    }
    if !has_content || !evidence {
        return None;
    }

    // Qualification is allocation-free after the fixed regexes initialize.
    // Each stripped row removes at least one byte, so the result is strictly
    // shorter than the input. Traversals and result capacity are bounded by
    // input length.
    let mut cleaned = String::with_capacity(content.len());
    let mut first_row = true;
    for (index, line) in content.split('\n').enumerate() {
        if loose_header == Some(index)
            || HEADER.is_match(line)
            || is_owned_metadata(line)
            || read_footer(line).is_some()
        {
            continue;
        }
        if !first_row {
            cleaned.push('\n');
        }
        first_row = false;
        let body = match PREFIX.find(line) {
            Some(prefix) => &line[prefix.end()..],
            None => line,
        };
        cleaned.push_str(body);
    }
    Some(cleaned)
}

fn is_owned_metadata(line: &str) -> bool {
    if line == "[Notebook cell-source projection]" {
        return true;
    }
    line.strip_prefix("[Source raw SHA256:")
        .and_then(|value| value.strip_suffix(']'))
        .is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn read_is_incomplete(content: &str) -> bool {
    let first_nonempty = content
        .split('\n')
        .position(|line| !line.trim_matches(js_whitespace).is_empty());
    let mut total = None;
    let mut numbered = 0usize;
    let mut consecutive = true;
    for (index, line) in content.split('\n').enumerate() {
        if let Some(footer) = read_footer(line) {
            let Some(footer_total) = footer else {
                return true;
            };
            if total.is_some_and(|previous| previous != footer_total) {
                return true;
            }
            total = Some(footer_total);
            continue;
        }
        if is_incomplete_read_row(line) {
            return true;
        }
        if let Some(prefix) = PREFIX.find(line) {
            numbered += 1;
            consecutive &= outer_line_number(&line[..prefix.end()]) == Some(numbered);
        } else if !line.is_empty()
            && !HEADER.is_match(line)
            && !is_owned_metadata(line)
            && !(first_nonempty == Some(index) && is_loose_header(line))
        {
            consecutive = false;
        }
    }
    total.is_some_and(|total| !consecutive || numbered != total)
}

fn outer_line_number(prefix: &str) -> Option<usize> {
    let mut number = prefix.trim_start();
    if let Some(rest) = number
        .strip_prefix(">>>")
        .or_else(|| number.strip_prefix(">>"))
    {
        number = rest.trim_start();
    }
    if number
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(*byte, b'+' | b'*' | b'-'))
    {
        number = number[1..].trim_start();
    }
    let digits = &number[..number.len() - 1];
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

// The outer Option distinguishes ordinary source rows from display footers;
// the inner Option is the total of a potentially complete full-view footer.
fn read_footer(line: &str) -> Option<Option<usize>> {
    let body = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    if (body.starts_with("output truncated at ")
        && body.contains(" after line ")
        && body.contains(" of "))
        || (body.starts_with("continue with `") && body.ends_with('`'))
    {
        return Some(None);
    }
    let range = body
        .strip_prefix("Showing lines ")
        .or_else(|| body.strip_prefix("lines "))?;
    let Some((ranges, remainder)) = range.split_once(" of ") else {
        return Some(None);
    };
    let digit_end = remainder
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(remainder.len());
    let suffix = remainder[digit_end..].trim();
    let Some(total) = remainder[..digit_end]
        .parse::<usize>()
        .ok()
        .filter(|&total| total > 0)
    else {
        return Some(None);
    };
    // Continuation, byte-limit and partial-read suffixes are not full views.
    if !suffix.is_empty() && suffix != "." {
        return Some(None);
    }
    let mut last = 0usize;
    for range in ranges.split(',') {
        let range = range.trim();
        let (start, end) = range.split_once('-').unwrap_or((range, range));
        if start.is_empty()
            || end.is_empty()
            || !start.bytes().all(|byte| byte.is_ascii_digit())
            || !end.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Some(None);
        }
        let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else {
            return Some(None);
        };
        if last.checked_add(1) != Some(start) || end < start || end > total {
            return Some(None);
        }
        last = end;
    }
    Some((last == total).then_some(total))
}

fn is_incomplete_read_row(line: &str) -> bool {
    if line.trim() == "[File not scanned to EOF]" {
        return true;
    }
    if RANGE_ELISION.is_match(line) || SINGLE_ELISION.is_match(line) {
        return true;
    }
    let Some(body) = line
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };
    let showing = body.starts_with("Showing ")
        && (body.contains(" line") || body.contains("lines ") || body.contains("bytes "))
        && (body.contains(" of ") || body.contains(" elided"));
    let more = (body.starts_with("More lines in ")
        || body
            .split_once(" more line")
            .is_some_and(|(count, _)| count.parse::<usize>().is_ok()))
        && body.contains(" in ")
        && body.contains(". Use ")
        && body.ends_with(" to continue");
    let elided = (body.starts_with('…') || body.starts_with("..."))
        && body.contains("ln elided;")
        && body.contains("re-read needed ranges");
    let oversized =
        body.starts_with("Line ") && body.contains(" exceeds ") && body.contains(" limit.");
    showing || more || elided || oversized
}

fn is_loose_header(line: &str) -> bool {
    let Some(body) = line
        .trim_matches(js_whitespace)
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return false;
    };
    let Some((path, tag)) = body.split_once('#') else {
        return false;
    };
    !path.is_empty()
        && !path.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
        && !tag
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}

// ECMAScript \s/trim includes BOM but not NEL, unlike native Unicode \s.
// Preserve that distinction for the TypeScript wrapper's loose-header path.
fn js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
