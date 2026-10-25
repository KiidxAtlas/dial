//! Placement checks for stale-tag recovery.
//!
//! The line map only aligns equal rows, so a remap can pass every positional test and still land
//! in the wrong place. Two situations are refused after the map succeeds, and the edit then fails
//! with the stale-file diagnostic that asks for a re-read:
//!
//! * the anchor sits on a row repeated in a sibling construct (an identical `work();` in the next
//!   function), so the shifted anchor's enclosing constructs differ from the authored ones;
//! * the line numbers were already current (copied from `grep -n` or `sed -n` and paired with an
//!   old tag), so the replacement resembles the lines at the authored numbers more than the lines
//!   the remap moved it onto.

use super::{Edit, indent_of, is_blank};
use crate::tools::edit::edit_native::fuzzy::similarity;
use crate::tools::edit::syntax::{self, Enclosing};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

/// Share of the lines at the authored numbers a replacement must keep before it counts as written
/// for them.
const AUTHORED_FLOOR: f64 = 0.5;
/// How much more of those lines than of the remap target the replacement must keep.
const AUTHORED_MARGIN: f64 = 0.3;
/// `window chars x body chars` products one recovery may spend on similarity; past it the check
/// abstains rather than run quadratic work on a large rewrite.
const SIMILARITY_BUDGET: usize = 1 << 23;
/// Shortest trimmed line (in chars) that counts as content evidence; blank lines, closers and
/// one-word rows resemble short bodies by chance.
const MIN_EVIDENCE_CHARS: usize = 8;

/// `Ok` when the remapped edits still belong where they were authored, else the reason they do not.
pub(super) fn check(
    path: &Path,
    base: &[String],
    cur: &[String],
    authored: &[Edit],
    remapped: &[Edit],
) -> Result<(), &'static str> {
    if !context_preserved(path, base, cur, authored, remapped) {
        return Err("the anchor now sits in a different enclosing block");
    }
    if aimed_at_authored_lines(cur, authored, remapped) {
        return Err(
            "the new text matches the current lines at the numbers you gave, not the lines the old tag points at",
        );
    }
    Ok(())
}

/// (base line, current line) pairs whose enclosing constructs must agree: the endpoints of a
/// replacement, or the lines on either side of an insertion gap. The shift is uniform across a
/// remapped edit, so endpoints pin the interior. Unshifted pairs are skipped: the very same line
/// at the very same number cannot have landed in a sibling.
fn anchor_pairs(authored: &Edit, remapped: &Edit, base_len: usize) -> Vec<(usize, usize)> {
    let pairs = if authored.insert {
        let gap = authored.lo;
        let mut pairs = Vec::new();
        if gap >= 1 {
            pairs.push((gap, remapped.lo));
        }
        if gap < base_len {
            pairs.push((gap + 1, remapped.lo + 1));
        }
        pairs
    } else {
        vec![(authored.lo, remapped.lo), (authored.hi, remapped.hi)]
    };
    pairs.into_iter().filter(|(a, b)| a != b).collect()
}

fn context_preserved(
    path: &Path,
    base: &[String],
    cur: &[String],
    authored: &[Edit],
    remapped: &[Edit],
) -> bool {
    let pairs: BTreeSet<(usize, usize)> = authored
        .iter()
        .zip(remapped)
        .flat_map(|(a, r)| anchor_pairs(a, r, base.len()))
        .collect();
    if pairs.is_empty() {
        return true;
    }
    let base_lines: Vec<usize> = pairs.iter().map(|&(a, _)| a).collect();
    let cur_lines: Vec<usize> = pairs.iter().map(|&(_, b)| b).collect();
    let (base_chains, cur_chains) = match (
        syntax::enclosing_constructs(path, &base.join("\n"), &base_lines),
        syntax::enclosing_constructs(path, &cur.join("\n"), &cur_lines),
    ) {
        (Some(base_chains), Some(cur_chains)) => (base_chains, cur_chains),
        _ => (
            indent_chains(base, &base_lines),
            indent_chains(cur, &cur_lines),
        ),
    };
    pairs
        .iter()
        .all(|(a, b)| base_chains.get(a) == cur_chains.get(b))
}

/// Enclosing constructs for grammar-less files: each earlier line indented less than the one
/// below it, innermost first, keyed by the trimmed row. Blank lines have no enclosing block.
fn indent_chains(lines: &[String], wanted: &[usize]) -> BTreeMap<usize, Vec<Enclosing>> {
    wanted
        .iter()
        .filter(|&&n| n >= 1 && n <= lines.len())
        .map(|&n| {
            let mut chain = Vec::new();
            if !is_blank(&lines[n - 1]) {
                let mut limit = indent_of(&lines[n - 1]);
                for row in lines[..n - 1].iter().rev() {
                    if limit == 0 {
                        break;
                    }
                    if !is_blank(row) && indent_of(row) < limit {
                        limit = indent_of(row);
                        chain.push(("indent".to_owned(), row.trim().to_owned()));
                    }
                }
            }
            (n, chain)
        })
        .collect()
}

fn identifies(line: &str) -> bool {
    line.trim().chars().count() >= MIN_EVIDENCE_CHARS
}

/// How much of `window` a replacement `body` keeps: each identifying window line scores its best
/// similarity to a body line, other lines score zero, divided by the window length. `None`
/// (leaving `budget` untouched) when the comparison costs more than the budget has left.
fn kept_share(
    window: &[&str],
    body: &[&str],
    verbatim: &HashSet<&str>,
    budget: &mut usize,
) -> Option<f64> {
    let window_chars: usize = window.iter().map(|l| l.trim().chars().count()).sum();
    let body_chars: usize = body.iter().map(|l| l.chars().count()).sum();
    let cost = window_chars.saturating_mul(body_chars);
    if window.is_empty() || cost > *budget {
        return None;
    }
    *budget -= cost;
    let total: f64 = window
        .iter()
        .map(|line| {
            let line = line.trim();
            if !identifies(line) {
                0.0
            } else if verbatim.contains(line) {
                1.0
            } else {
                body.iter()
                    .map(|row| similarity(line, row))
                    .fold(0.0, f64::max)
            }
        })
        .sum();
    Some(total / window.len() as f64)
}

/// Whether a shifted replacement reads as written for the current lines at its authored numbers.
/// The remap is right when the numbers came from the tagged read; numbers taken from an untagged
/// view of the current file make the shift wrong, and the body keeps much of the lines at the
/// authored numbers and little of the target. A heuristic: a rewrite that by chance resembles the
/// lines the shift points away from is refused too, costing one retry under the fresh tag.
fn aimed_at_authored_lines(cur: &[String], authored: &[Edit], remapped: &[Edit]) -> bool {
    let window = |lo: usize, hi: usize| -> Vec<&str> {
        (lo..=hi)
            .map(|n| cur.get(n - 1).map_or("", String::as_str))
            .collect()
    };
    let mut budget = SIMILARITY_BUDGET;
    for (a, r) in authored.iter().zip(remapped) {
        if a.insert || a.lo == r.lo {
            continue;
        }
        let body: Vec<&str> = a
            .lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let target = window(r.lo, r.hi);
        // Without an identifying target line the two readings cannot be told apart.
        if body.is_empty() || !target.iter().any(|l| identifies(l)) {
            continue;
        }
        let verbatim: HashSet<&str> = body.iter().copied().collect();
        let Some(kept) = kept_share(&window(a.lo, a.hi), &body, &verbatim, &mut budget) else {
            return false;
        };
        if kept < AUTHORED_FLOOR {
            continue;
        }
        let Some(target_kept) = kept_share(&target, &body, &verbatim, &mut budget) else {
            return false;
        };
        if kept > target_kept + AUTHORED_MARGIN {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
