//! Orders a directory outline by how central each file is: PageRank over a reference graph.
//!
//! A file links to another when it mentions a name the other declares at top level. This is a
//! lexical approximation of imports and calls (no resolution, so a shared name can create a false
//! edge); ranking only reorders the outline and decides what survives its budget, it never hides a
//! file that fits. A name declared in several files splits its weight between them, and repeated
//! mentions count with diminishing weight.

use std::collections::{BTreeMap, HashMap};

/// Shorter names (`a`, `id`, `ok`) are too common to signal a dependency.
const MIN_NAME: usize = 3;
/// A single lowercase word shorter than this (`write`, `system`, `owner`) is ordinary vocabulary
/// that appears in most files; names with `_` or an uppercase letter are specific at any length.
const PLAIN_WORD_MIN: usize = 8;
/// Words that may precede a declaration keyword. Anything else first (prose such as "This function
/// returns ...") means the line declares nothing.
const MODIFIERS: &[&str] = &[
    "pub",
    "crate",
    "super",
    "in",
    "export",
    "default",
    "async",
    "public",
    "private",
    "protected",
    "internal",
    "abstract",
    "final",
    "open",
    "sealed",
    "data",
    "unsafe",
    "extern",
    "inline",
    "declare",
    "readonly",
    "override",
];
/// Names declared in more files than this (`new`, `main`, `test`) are ignored.
const MAX_DEFINERS: usize = 5;
const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;

/// Keywords that introduce a declared name. `impl` names another type; `mod`/`module` name a child
/// file rather than define the name here.
const KEYWORDS: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "trait",
    "type",
    "const",
    "static",
    "union",
    "class",
    "def",
    "function",
    "interface",
    "func",
    "var",
    "let",
    "val",
    "object",
    "record",
    "namespace",
    "protocol",
    "typealias",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Whether `name` is specific enough that a mention of it suggests a dependency.
fn specific(name: &str) -> bool {
    name.len() >= MIN_NAME
        && (name.len() >= PLAIN_WORD_MIN
            || name.contains('_')
            || name.chars().any(|c| c.is_uppercase()))
}

/// The name a declaration's first line introduces: the word after a declaration keyword that
/// follows only [`MODIFIERS`]. Comment lines declare nothing.
pub(super) fn declared_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if ["//", "/*", "*", "#", "--", ";", "\"\"\"", "'''"]
        .iter()
        .any(|marker| trimmed.starts_with(marker))
    {
        return None;
    }
    let mut seen_keyword = false;
    for word in trimmed
        .split(|c: char| !is_word(c))
        .filter(|w| !w.is_empty())
    {
        // Keywords can stack (`const fn`, `static final`): the name follows the last of them.
        if KEYWORDS.contains(&word) {
            seen_keyword = true;
        } else if seen_keyword {
            return word
                .chars()
                .next()
                .is_some_and(|c| !c.is_ascii_digit())
                .then_some(word);
        } else if !MODIFIERS.contains(&word) {
            return None;
        }
    }
    None
}

/// One file's rank (summing to 1 across `files`) given each file's declared names and text.
/// With no cross-file references every file gets the same rank.
pub(super) fn ranks(files: &[(Vec<&str>, &str)]) -> Vec<f64> {
    let n = files.len();
    if n == 0 {
        return Vec::new();
    }
    let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, (names, _)) in files.iter().enumerate() {
        for name in names {
            if specific(name) {
                let list = definers.entry(name).or_default();
                if !list.contains(&index) {
                    list.push(index);
                }
            }
        }
    }
    definers.retain(|_, list| list.len() <= MAX_DEFINERS);

    // edges[from] = (to, weight); BTreeMap keeps iteration deterministic.
    let mut edges: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); n];
    for (from, (_, text)) in files.iter().enumerate() {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for word in text.split(|c: char| !is_word(c)) {
            if definers.contains_key(word) {
                *counts.entry(word).or_default() += 1;
            }
        }
        for (name, count) in counts {
            let targets: Vec<usize> = definers[name]
                .iter()
                .copied()
                .filter(|&to| to != from)
                .collect();
            if targets.is_empty() || definers[name].contains(&from) {
                // A file that declares the name itself most likely means its own.
                continue;
            }
            let weight = (count as f64).sqrt() / targets.len() as f64;
            for to in targets {
                *edges[from].entry(to).or_default() += weight;
            }
        }
    }

    let base = 1.0 / n as f64;
    let mut rank = vec![base; n];
    for _ in 0..ITERATIONS {
        let mut next = vec![(1.0 - DAMPING) * base; n];
        let mut dangling = 0.0;
        for (from, out) in edges.iter().enumerate() {
            let total: f64 = out.values().sum();
            if total == 0.0 {
                dangling += rank[from];
                continue;
            }
            for (&to, &weight) in out {
                next[to] += DAMPING * rank[from] * weight / total;
            }
        }
        for value in &mut next {
            *value += DAMPING * dangling * base;
        }
        rank = next;
    }
    rank
}

#[cfg(test)]
mod tests;
