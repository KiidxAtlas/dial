//! A single insertion backed by the last working source, not a search for any parseable edit.
use super::{Parsed, Path, Tree};
use std::ops::Range;

/// Called only for a clean → broken parse. Align the unchanged suffix once: the first
/// old byte to its left must be a syntactic closer, not punctuation inside a literal.
/// Abstain unless restoring it leaves exactly one local leaf substitution inside the
/// same delimited construct. In particular, never repair a pure deletion back to `before`.
pub(super) fn restore(path: &Path, before: &str, proposed: &str, prior: &Parsed) -> Option<String> {
    if before.len().max(proposed.len()) > 64 * 1024 {
        return None;
    }
    let suffix = before
        .bytes()
        .rev()
        .zip(proposed.bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_at = before.len().checked_sub(suffix + 1)?;
    let token = char::from(before.as_bytes()[old_at]);
    if !matches!(token, ')' | ']' | '}')
        || before.as_bytes().get(old_at + 1) == Some(&(token as u8))
    {
        return None;
    }
    let at = proposed.len() - suffix;
    if !proposed.is_char_boundary(at) {
        return None;
    }
    let mut text = proposed.to_owned();
    text.insert(at, token);
    let changed = before.bytes().zip(text.bytes()).position(|(a, b)| a != b)?;
    if changed >= old_at || changed >= at {
        return None;
    }
    let old = evidence(&prior.tree, old_at, changed, token)?;
    let fixed = Parsed::parse(path, &text).ok()?;
    if !fixed.errors.is_empty() {
        return None;
    }
    let new = evidence(&fixed.tree, at, changed, token)?;
    if old.owner != new.owner
        || old.leaf.start != new.leaf.start
        || old.kind != new.kind
        || before[old.leaf.end..] != text[new.leaf.end..]
    {
        return None;
    }
    Some(text)
}

struct Evidence {
    owner: (String, usize),
    leaf: Range<usize>,
    kind: String,
}

fn evidence(tree: &Tree, at: usize, changed: usize, token: char) -> Option<Evidence> {
    let root = tree.root().get_inner_node();
    if root.has_error() {
        return None;
    }
    let close = root.descendant_for_byte_range(at, at + 1)?;
    let owner = close.parent()?;
    let opener = match token {
        ')' => "(",
        ']' => "[",
        '}' => "{",
        _ => return None,
    };
    if close.is_named()
        || close.byte_range() != (at..at + 1)
        || close.kind().as_bytes() != [token as u8]
        || owner.child(0)?.kind() != opener
        || owner.end_byte() != close.end_byte()
    {
        return None;
    }
    if changed <= owner.start_byte() || changed >= close.start_byte() {
        return None;
    }
    let leaf = owner.descendant_for_byte_range(changed, changed + 1)?;
    if leaf.child_count() != 0 || leaf.start_position().row != close.start_position().row {
        return None;
    }
    let kind = match leaf.kind() {
        "+" | "-" => "arithmetic sign",
        k @ ("identifier" | "integer" | "integer_literal" | "number") => k,
        _ => return None,
    };
    Some(Evidence {
        owner: (owner.kind().to_owned(), owner.start_byte()),
        leaf: leaf.byte_range(),
        kind: kind.to_owned(),
    })
}
