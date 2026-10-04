//! Unified-diff-like preview of a set of splices, one hunk per run of touched lines.

use super::engine::{Splice, apply};

/// Start of the line containing byte `at`.
fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// End (exclusive, before the newline) of the line containing byte `at`.
fn line_end(src: &str, at: usize) -> usize {
    src[at..].find('\n').map_or(src.len(), |i| at + i)
}

fn lines_of(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Splices grouped by overlapping or adjacent lines, each with its byte span `[start, end)` in `src`.
fn hunks<'a>(src: &str, splices: &'a [Splice]) -> Vec<(usize, usize, &'a [Splice])> {
    let mut out: Vec<(usize, usize, usize, usize)> = Vec::new(); // (start, end, first, last+1)
    for (i, splice) in splices.iter().enumerate() {
        let start = line_start(src, splice.start);
        let last_byte = if splice.end > splice.start { splice.end - 1 } else { splice.start };
        let end = line_end(src, last_byte.max(splice.start));
        match out.last_mut() {
            Some(prev) if start <= prev.1 => {
                prev.1 = prev.1.max(end);
                prev.3 = i + 1;
            }
            _ => out.push((start, end, i, i + 1)),
        }
    }
    out.into_iter().map(|(s, e, a, b)| (s, e, &splices[a..b])).collect()
}

/// Render the change from `src` as `--- a/path` / `+++ b/path` plus `@@` hunks.
pub(super) fn preview(rel: &str, src: &str, splices: &[Splice]) -> String {
    let mut out = format!("--- a/{rel}\n+++ b/{rel}\n");
    let mut delta: isize = 0;
    for (start, end, group) in hunks(src, splices) {
        let old = &src[start..end];
        let shifted: Vec<Splice> = group
            .iter()
            .map(|s| Splice { start: s.start - start, end: s.end - start, text: s.text.clone() })
            .collect();
        let new = apply(old, &shifted);
        let (old_lines, new_lines) = (lines_of(old), lines_of(&new));
        let old_first = src[..start].matches('\n').count() + 1;
        let new_first = (old_first as isize + delta) as usize;
        out.push_str(&format!("@@ -{old_first},{} +{new_first},{} @@\n", old_lines.len(), new_lines.len()));
        for line in &old_lines {
            out.push_str(&format!("-{line}\n"));
        }
        for line in &new_lines {
            out.push_str(&format!("+{line}\n"));
        }
        delta += new_lines.len() as isize - old_lines.len() as isize;
    }
    out
}
