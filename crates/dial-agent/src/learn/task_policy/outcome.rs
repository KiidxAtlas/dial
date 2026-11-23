//! Requested-outcome extraction for task classification.
//!
//! This is a bounded local heuristic, not a parser. It asks which recognised action words a
//! prompt uses in request position. Three things keep a word out of request position:
//! - it is negated by a directly preceding negator ("don't fix", "without changing", "stop
//!   editing", "do not make any changes");
//! - it names the subject: a determiner-led noun ("the fix", "a change") or the object of an
//!   embedded condition ("explain why it fails after you change the config");
//! - it sits inside a backtick code span ("explain why `fix --test` fails").
//!
//! Limits, by design: sentence and clause boundaries are punctuation; each word looks back at
//! most `LOOKBACK` tokens, so cost stays linear in prompt length. Ambiguous phrasing ("how to fix
//! it", "should I fix it", "I need a fix") is treated as a request, so the caller stays on the
//! conservative side: an unclear prompt is not diagnosis-only.

use std::collections::BTreeSet;

/// Tokens a word may look back over when judging negation, object or subordinate context.
const LOOKBACK: usize = 12;
/// Light-verb chains followed when judging negation ("do not make any changes").
const MAX_CHAIN: usize = 4;

/// Every recognised action or diagnosis word; inflections are matched by `is_inflection`.
const ACTIONS: &[&str] = &[
    "fix",
    "repair",
    "resolve",
    "patch",
    "make",
    "solve",
    "correct",
    "implement",
    "change",
    "edit",
    "update",
    "refactor",
    "rename",
    "add",
    "remove",
    "debug",
    "explain",
    "investigate",
    "diagnose",
    "diagnosis",
    "cause",
    "analyze",
    "analyse",
    "inspect",
    "read",
    "audit",
    "locate",
    "find",
    "why",
    "trace",
];

/// Words that request a repair when they are the outcome; diagnosis-only means none is requested.
pub(super) const REPAIR: &[&str] = &[
    "fix",
    "repair",
    "resolve",
    "patch",
    "make",
    "solve",
    "correct",
    "implement",
    "change",
    "edit",
    "update",
    "refactor",
    "rename",
    "add",
    "remove",
];

/// Words that ask for a diagnosis or explanation rather than a change. They are never treated
/// as subject nouns ("give me a diagnosis" still requests one).
pub(super) const DIAGNOSE: &[&str] = &[
    "why",
    "diagnose",
    "diagnosis",
    "explain",
    "investigate",
    "cause",
    "analyze",
    "analyse",
];

/// Words that request a code change when they are the outcome.
pub(super) const EDIT: &[&str] = &[
    "implement",
    "change",
    "edit",
    "rename",
    "add",
    "remove",
    "refactor",
];

/// Words that request investigation when they are the outcome.
pub(super) const INVESTIGATE: &[&str] = &[
    "explain",
    "investigate",
    "diagnose",
    "inspect",
    "read",
    "audit",
    "locate",
    "find",
    "why",
    "trace",
];

/// Words that request debugging when they are the outcome.
pub(super) const DEBUG: &[&str] = &["fix", "debug"];

/// Words that negate the verb after them ("not", "don't" → "don" + "t", "without").
const NEGATORS: &[&str] = &[
    "not", "t", "no", "never", "without", "avoid", "skip", "stop", "instead", "rather", "nor",
    "cannot",
];

/// Words skipped between a negator and the verb it negates ("do not *just* fix").
const SKIPPED: &[&str] = &[
    "just", "really", "even", "actually", "simply", "directly", "also", "ever", "yet", "at", "of",
    "than",
];

/// Words that make the following repair/edit word a noun phrase ("the fix", "a change").
const DETERMINERS: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "my", "your", "our", "their", "its", "his",
    "her", "any", "some", "every", "each", "another", "of", "for", "about", "from", "in", "on",
    "with",
];

/// Verbs that take a repair/edit noun as object ("make the change", "need a fix").
const LIGHT_VERBS: &[&str] = &[
    "make", "apply", "do", "perform", "need", "want", "write", "push", "submit", "include",
];

/// Subordinators opening a condition or time clause ("after you change the config").
const SUBORDINATORS: &[&str] = &[
    "after", "when", "whenever", "because", "since", "once", "while", "until", "unless", "before",
    "if",
];

/// Coordinators ending the scope of a subordinator or negator.
const CONJUNCTIONS: &[&str] = &["and", "then", "but", "or", "so"];

/// Whether `word` is `base` or one of its regular inflections: base, -s, -es, -ing, and -ing after
/// dropping a final `e`. Allocation-free.
fn is_inflection(word: &str, base: &str) -> bool {
    if word == base {
        return true;
    }
    if let Some(rest) = word.strip_prefix(base) {
        return matches!(rest, "s" | "es" | "ing");
    }
    match (word.strip_suffix("ing"), base.strip_suffix('e')) {
        (Some(stem), Some(base_stem)) => stem == base_stem,
        _ => false,
    }
}

fn base_form(word: &str) -> Option<&'static str> {
    ACTIONS
        .iter()
        .copied()
        .find(|base| is_inflection(word, base))
}

/// Replaces backtick code spans with a space so their words are never read as requests. An
/// unclosed backtick leaves the rest of the prompt as ordinary text.
fn strip_code_spans(lower: &str) -> String {
    let parts: Vec<&str> = lower.split('`').collect();
    let mut out = String::with_capacity(lower.len());
    for (index, part) in parts.iter().enumerate() {
        if index % 2 == 1 && index + 1 < parts.len() {
            out.push(' ');
        } else {
            out.push_str(part);
        }
    }
    out
}

/// Clause spans of `text`, split at sentence and clause punctuation.
fn clauses(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let boundary = match c {
            '.' | '!' | '?' => chars.peek().is_none_or(|(_, next)| next.is_whitespace()),
            ';' | ',' | ':' | '(' | ')' | '"' | '\n' | '—' | '–' => true,
            _ => false,
        };
        if boundary {
            spans.push(&text[start..index]);
            start = index + c.len_utf8();
        }
    }
    spans.push(&text[start..]);
    spans
}

/// Whether the word at `index` is negated by a negator before it, or followed by "nothing".
fn negated(words: &[&str], index: usize) -> bool {
    if matches!(words.get(index + 1), Some(&"nothing") | Some(&"none")) {
        return true;
    }
    let mut end = index;
    for _ in 0..MAX_CHAIN {
        let lo = end.saturating_sub(LOOKBACK);
        let Some(before) = (lo..end)
            .rev()
            .find(|&k| !SKIPPED.contains(&words[k]) && !DETERMINERS.contains(&words[k]))
        else {
            return false;
        };
        let word = words[before];
        if NEGATORS.contains(&word) {
            return true;
        }
        if !LIGHT_VERBS.contains(&word) {
            return false;
        }
        // A light verb may itself be negated: "do not make any changes".
        end = before;
    }
    false
}

/// Whether the word at `index` names the subject rather than a request.
fn names_subject(words: &[&str], index: usize) -> bool {
    if index == 0 {
        return false;
    }
    let determined = DETERMINERS.contains(&words[index - 1])
        && !(index >= 2 && LIGHT_VERBS.contains(&words[index - 2]));
    if determined {
        return true;
    }
    let lo = index.saturating_sub(LOOKBACK);
    for k in (lo..index).rev() {
        if CONJUNCTIONS.contains(&words[k]) {
            return false;
        }
        // A subordinator opening the clause ("if it fails, ...") does not make a condition.
        if k > 0 && SUBORDINATORS.contains(&words[k]) {
            return true;
        }
    }
    false
}

/// Base forms of the words that are requested outcomes of `prompt`: recognised words that are
/// neither negated, nor naming the subject, nor inside a code span.
pub(super) fn requested(prompt: &str) -> BTreeSet<&'static str> {
    let text = strip_code_spans(&prompt.to_lowercase());
    let mut outcomes = BTreeSet::new();
    let mut words: Vec<&str> = Vec::new();
    for clause in clauses(&text) {
        words.clear();
        words.extend(
            clause
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty()),
        );
        for index in 0..words.len() {
            let Some(base) = base_form(words[index]) else {
                continue;
            };
            if negated(&words, index) || (!DIAGNOSE.contains(&base) && names_subject(&words, index))
            {
                continue;
            }
            // "make sure" / "make sense" hedge the request rather than ask for a change.
            if base == "make" && matches!(words.get(index + 1), Some(&"sure" | &"sense" | &"clear"))
            {
                continue;
            }
            outcomes.insert(base);
        }
    }
    outcomes
}
