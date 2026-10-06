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

pub(super) const MAX_READ_OUTPUT: usize = 20 * 1024;
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
        if !sel.selects_lines() {
            if let Some((ranges, tail)) = parse_ranges(seg)? {
                sel.ranges = ranges;
                sel.tail = tail;
                rest = &rest[..i];
                continue;
            }
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
/// lines. `capped` is set when the 64 KiB output cap cut the read short;
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
            "output truncated at 64 KiB after line {last} of {len}"
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
mod tests {
    use super::*;

    fn sel(ranges: &[(usize, Option<usize>)]) -> Selector {
        Selector {
            raw: false,
            ranges: ranges.to_vec(),
            tail: None,
        }
    }

    fn span(start: usize, end: usize, bounded: bool) -> Span {
        Span {
            start,
            end,
            bounded,
        }
    }

    #[test]
    fn selectors_parse_ranges_tail_and_raw() {
        let (path, s) = split_selector("src/a.rs:50-100").unwrap();
        assert_eq!(
            (path, s.ranges.clone(), s.raw),
            ("src/a.rs", vec![(50, Some(100))], false)
        );
        let (path, s) = split_selector("a.rs:raw:5-9").unwrap();
        assert_eq!((path, s.ranges, s.raw), ("a.rs", vec![(5, Some(9))], true));
        let (path, s) = split_selector("a.rs:5..9:raw").unwrap();
        assert_eq!((path, s.ranges, s.raw), ("a.rs", vec![(5, Some(9))], true));
        assert_eq!(split_selector("a.rs:-30").unwrap().1.tail, Some(30));
        assert_eq!(
            split_selector("a.rs:10+5").unwrap().1.ranges,
            vec![(10, Some(14))]
        );
        assert_eq!(split_selector("a.rs:7-").unwrap().1.ranges, vec![(7, None)]);
        assert_eq!(
            split_selector("a.rs:12,3-5,4-8").unwrap().1.ranges,
            vec![(3, Some(8)), (12, None)]
        );
        assert_eq!(
            split_selector("a.rs:5-16,960-973").unwrap().1.ranges,
            vec![(5, Some(16)), (960, Some(973))]
        );
        let (path, s) = split_selector("archive.zip:inner/file").unwrap();
        assert_eq!((path, s), ("archive.zip:inner/file", Selector::default()));
        assert!(split_selector("a.rs:0").is_err());
        assert!(split_selector("a.rs:9-3").is_err());
        assert!(split_selector("a.rs:-0").is_err());
        assert!(split_selector("a.rs:5+0").is_err());
    }

    #[test]
    fn spans_clip_to_the_file_and_drop_ranges_past_the_end() {
        let ranges = sel(&[(2, Some(4)), (8, Some(50)), (90, Some(95))]);
        assert_eq!(
            plan_spans(&ranges, 1, 100, 10),
            vec![span(2, 4, false), span(8, 10, false)]
        );
        assert!(plan_spans(&sel(&[(11, None)]), 1, 100, 10).is_empty());
        assert_eq!(
            plan_spans(&sel(&[(10, None)]), 1, 100, 10),
            vec![span(10, 10, true)]
        );
    }

    #[test]
    fn open_ended_ranges_and_offsets_are_bounded_by_limit() {
        assert_eq!(
            plan_spans(&sel(&[(3, None)]), 1, 4, 100),
            vec![span(3, 6, true)]
        );
        assert_eq!(
            plan_spans(&Selector::default(), 5, 2, 100),
            vec![span(5, 6, true)]
        );
        assert!(plan_spans(&Selector::default(), 5, 2, 4).is_empty());
        assert!(plan_spans(&Selector::default(), 1, 2, 0).is_empty());
        // Unknown length (streaming reads) never overflows.
        assert_eq!(
            plan_spans(&Selector::default(), 1, 3, usize::MAX),
            vec![span(1, 3, true)]
        );
    }

    #[test]
    fn tail_selects_the_last_lines_even_when_longer_than_the_file() {
        let tail = |n| Selector {
            raw: false,
            ranges: vec![],
            tail: Some(n),
        };
        assert_eq!(plan_spans(&tail(3), 1, 10, 10), vec![span(8, 10, false)]);
        assert_eq!(plan_spans(&tail(50), 1, 10, 10), vec![span(1, 10, false)]);
        assert!(plan_spans(&tail(5), 1, 10, 0).is_empty());
    }

    #[test]
    fn footer_states_range_total_and_continuation() {
        let none = Selector::default();
        let raw = Selector {
            raw: true,
            ..Selector::default()
        };
        let explicit = sel(&[(1, Some(5))]);
        assert_eq!(footer("f", &none, &[(1, 5)], 5, None, false), "");
        assert_eq!(
            footer("f", &explicit, &[(1, 5)], 5, None, false),
            "[lines 1-5 of 5]\n"
        );
        assert_eq!(
            footer("f", &none, &[(2, 3)], 5, None, false),
            "[lines 2-3 of 5]\n"
        );
        assert_eq!(
            footer("f", &none, &[(5, 5), (9, 12)], 20, None, false),
            "[lines 5, 9-12 of 20]\n"
        );
        assert_eq!(
            footer("f", &none, &[(1, 2)], 9, Some(3), false),
            "[lines 1-2 of 9; continue with `f:3-`]\n"
        );
        assert_eq!(footer("f", &raw, &[(2, 3)], 5, None, false), "");
        assert_eq!(
            footer("f", &raw, &[(1, 2)], 9, Some(3), false),
            "[continue with `f:3-`]\n"
        );
        assert_eq!(
            footer("f", &none, &[(1, 2)], 9, Some(3), true),
            "[output truncated at 64 KiB after line 2 of 9; continue with `f:3-`]\n"
        );
    }
}
