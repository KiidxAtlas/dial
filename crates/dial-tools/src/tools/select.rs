//! Line selection shared by both `read` modes.
//!
//! A read is described by a path that may end in a selector, plus optional
//! `offset`/`limit` arguments:
//!
//! | selector          | lines                                   |
//! |-------------------|-----------------------------------------|
//! | `path:50-200`     | 50 through 200                          |
//! | `path:50+150`     | 150 lines starting at 50                |
//! | `path:50-`        | from 50, at most `limit` lines          |
//! | `path:-60`        | the last 60 lines                       |
//! | `path:5-16,960-973` | several ranges, in file order         |
//! | `path:raw[:...]`  | verbatim rows without line anchors      |
//!
//! A file that really is named like a selector (`weird:5`) is read literally.
//! [`plan_spans`] turns a selector into concrete line spans for a file of known
//! length and [`footer`] states which lines were returned and how to continue.

use super::Workspace;

pub(super) const MAX_READ_OUTPUT: usize = 48 * 1024;
/// Bytes kept free of rows for a read's header and footer when its rows are limited to a byte budget.
pub(super) const READ_FOOTER_RESERVE: usize = 1024;
pub(super) const MAX_READ_LINE: usize = 2000;
pub(super) const DEFAULT_READ_LINES: usize = 2000;
pub(super) const MAX_READ_LINES: usize = 10_000;

/// A trailing `:`-selector parsed off a read path.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selector {
    pub raw: bool,
    /// 1-based `(start, inclusive end)`; `None` end = open-ended.
    pub ranges: Vec<(usize, Option<usize>)>,
    /// Last N lines.
    pub tail: Option<usize>,
}

impl Selector {
    pub(super) fn selects_lines(&self) -> bool {
        !self.ranges.is_empty() || self.tail.is_some()
    }
}

/// Parsed ranges (merged, sorted) or a tail count. `None`: not a selector.
type Parsed = (Vec<(usize, Option<usize>)>, Option<usize>);

fn parse_ranges(seg: &str) -> Result<Option<Parsed>, String> {
    if seg.is_empty() {
        return Ok(None);
    }
    let number = |s: &str| -> Option<usize> { s.strip_prefix('L').unwrap_or(s).parse().ok() };
    if let Some(rest) = seg.strip_prefix('-') {
        return match rest.parse::<usize>() {
            Ok(0) => Err("tail selector `:-N` needs N >= 1".to_owned()),
            Ok(n) => Ok(Some((Vec::new(), Some(n)))),
            Err(_) => Ok(None),
        };
    }
    let mut out = Vec::new();
    for part in seg.split(',') {
        let (a, b) = if let Some((a, c)) = part.split_once('+') {
            match (number(a), number(c)) {
                (Some(a), Some(c)) if c >= 1 => (a, Some(a.saturating_add(c - 1))),
                (Some(_), Some(_)) => return Err("`A+C` needs C >= 1".to_owned()),
                _ => return Ok(None),
            }
        } else if let Some((a, b)) = part.split_once("..").or_else(|| part.split_once('-')) {
            match (
                number(a),
                if b.is_empty() {
                    Some(None)
                } else {
                    number(b).map(Some)
                },
            ) {
                (Some(a), Some(b)) => (a, b),
                _ => return Ok(None),
            }
        } else {
            match number(part) {
                Some(a) => (a, None),
                None => return Ok(None),
            }
        };
        if a == 0 {
            return Err("line selector 0 is invalid; lines are 1-based. Use :1".to_owned());
        }
        if b.is_some_and(|b| b < a) {
            return Err(format!("range {part:?} ends before it starts"));
        }
        out.push((a, b));
    }
    out.sort_unstable_by_key(|r| r.0);
    let mut merged: Vec<(usize, Option<usize>)> = Vec::new();
    for (a, b) in out {
        match merged.last_mut() {
            Some(last) if last.1.is_none_or(|end| a <= end.saturating_add(1)) => {
                last.1 = match (last.1, b) {
                    (Some(x), Some(y)) => Some(x.max(y)),
                    _ => None,
                };
            }
            _ => merged.push((a, b)),
        }
    }
    Ok(Some((merged, None)))
}

/// Split `file.rs:50-100`, `file.rs:raw`, `file.rs:raw:5-9`, `file.rs:-30`
/// into path and selector. Unrecognized trailing text stays in the path.
pub fn split_selector(input: &str) -> Result<(&str, Selector), String> {
    let mut rest = input;
    let mut sel = Selector::default();
    for _ in 0..2 {
        let Some(i) = rest.rfind(':') else { break };
        let seg = &rest[i + 1..];
        if seg == "raw" && !sel.raw {
            sel.raw = true;
            rest = &rest[..i];
            continue;
        }
        if !sel.selects_lines()
            && let Some((ranges, tail)) = parse_ranges(seg)?
        {
            sel.ranges = ranges;
            sel.tail = tail;
            rest = &rest[..i];
            continue;
        }
        // A numeric start followed by range punctuation is an attempted selector, not
        // arbitrary colon-containing path text. Literal existing filenames still win
        // in split_target before we get here.
        let start = seg.strip_prefix('L').unwrap_or(seg);
        let digits = start.bytes().take_while(u8::is_ascii_digit).count();
        if !sel.selects_lines() && digits > 0 && start[digits..].starts_with(['-', '+', '.', ',']) {
            return Err(format!(
                "Invalid line selector {seg:?}: use numeric 1-based lines, e.g. :25-50 or :25+20. Correct the selector or use separate offset/limit arguments; do not retry this path unchanged."
            ));
        }
        break;
    }
    Ok((rest, sel))
}

/// The path and selector of a `read` argument. A literal file name wins over
/// selector interpretation.
pub(super) fn split_target<'a>(
    ws: &Workspace,
    input: &'a str,
) -> Result<(&'a str, Selector), String> {
    if ws.resolve(input).is_ok_and(|p| p.exists()) {
        return Ok((input, Selector::default()));
    }
    split_selector(input)
}

/// Concrete 1-based inclusive lines to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Span {
    pub start: usize,
    pub end: usize,
    /// The end was chosen by `limit`, so the file may continue past it.
    pub bounded: bool,
}

/// Spans for a file of `len` lines (`usize::MAX` when not yet known, which is
/// valid for everything except a tail selector). Spans starting past the end
/// are dropped; explicit ends are clipped to the file.
pub(super) fn plan_spans(sel: &Selector, offset: usize, limit: usize, len: usize) -> Vec<Span> {
    let bounded_end = |start: usize| start.saturating_add(limit.max(1) - 1).min(len);
    if let Some(n) = sel.tail {
        let n = n.min(MAX_READ_LINES);
        return if len == 0 {
            Vec::new()
        } else {
            vec![Span {
                start: len.saturating_sub(n) + 1,
                end: len,
                bounded: false,
            }]
        };
    }
    if !sel.selects_lines() {
        let start = offset.max(1);
        return if start <= len {
            vec![Span {
                start,
                end: bounded_end(start),
                bounded: true,
            }]
        } else {
            Vec::new()
        };
    }
    sel.ranges
        .iter()
        .filter(|&&(a, _)| a <= len)
        .map(|&(a, b)| match b {
            Some(b) => Span {
                start: a,
                end: b.min(len),
                bounded: false,
            },
            None => Span {
                start: a,
                end: bounded_end(a),
                bounded: true,
            },
        })
        .collect()
}

fn describe_spans(shown: &[(usize, usize)]) -> String {
    let parts: Vec<String> = shown
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect();
    parts.join(", ")
}

/// Trailing status line, or empty when nothing needs saying.
///
/// `shown` are the line spans actually returned and `len` the file's total
/// lines. `capped` is set when the byte budget (the model-facing output limit) cut the read short;
/// `next` is the first line not returned that the model may want, if any.
/// Anchored output states its range and the total whenever it is not the
/// whole file read without a selector; raw output stays verbatim and only says
/// how to continue.
pub(super) fn footer(
    rel: &str,
    sel: &Selector,
    shown: &[(usize, usize)],
    len: usize,
    next: Option<usize>,
    capped: bool,
) -> String {
    let mut parts = Vec::new();
    let last = shown.last().map_or(0, |s| s.1);
    if capped {
        parts.push(format!(
            "output truncated at the byte budget after line {last} of {len}"
        ));
    } else if !sel.raw && (sel.selects_lines() || shown != [(1, len)]) {
        parts.push(format!("lines {} of {len}", describe_spans(shown)));
    }
    if let Some(n) = next {
        parts.push(format!("continue with `{rel}:{n}-`"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]\n", parts.join("; "))
    }
}

/// What to say when a read selects no lines at all.
pub(super) fn empty_read(len: usize) -> String {
    if len == 0 {
        "(empty file)".to_owned()
    } else {
        format!("(no lines in that range; file has {len} lines)")
    }
}

#[cfg(test)]
mod tests;
