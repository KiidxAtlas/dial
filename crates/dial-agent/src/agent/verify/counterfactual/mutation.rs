//! `experimental.mutation_runs`: test strength of a passing behavioral check.
//!
//! Small, language-agnostic mutations (flipped comparison or boolean operator, swapped `true` and
//! `false`, an integer off by one) are applied one at a time to lines this turn added or changed,
//! and the check is rerun in an isolated copy. A mutant the check still passes marks changed
//! behavior the check does not pin down: a test gap, or equivalent code. Evidence only; check
//! status and repair credit never change.

use super::*;

/// Changed files larger than this are not mutated.
const MAX_FILE_BYTES: usize = 512 << 10;
/// Stop after this many consecutive runs that neither pass nor fail on a code diagnostic.
const MAX_INCONCLUSIVE: usize = 2;

/// Operator rewrites, tried in order; the first one found outside a string literal is used.
const OPERATORS: &[(&str, &str)] = &[
    ("==", "!="),
    ("!=", "=="),
    ("<=", "<"),
    (">=", ">"),
    ("&&", "||"),
    ("||", "&&"),
    (" and ", " or "),
    (" or ", " and "),
    (" + ", " - "),
    (" - ", " + "),
];

/// Keywords swapped only as whole words.
const WORDS: &[(&str, &str)] = &[
    ("true", "false"),
    ("false", "true"),
    ("True", "False"),
    ("False", "True"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::agent) struct Mutant {
    pub path: String,
    /// 1-based line in the current file.
    pub line: usize,
    pub original: String,
    pub mutated: String,
    content: Vec<u8>,
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether byte offset `at` of `line` falls inside a quoted string (an approximation: an odd
/// number of unescaped `"` or `'` before it).
fn in_string(line: &str, at: usize) -> bool {
    let prefix = &line[..at];
    let unescaped = |quote: char| {
        let mut count = 0;
        let mut escaped = false;
        for c in prefix.chars() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                count += 1;
            }
        }
        count
    };
    unescaped('"') % 2 == 1 || unescaped('\'') % 2 == 1
}

fn comment_or_blank(line: &str) -> bool {
    let line = line.trim_start();
    line.is_empty()
        || ["//", "#", "/*", "*", "--", ";", "%"]
            .iter()
            .any(|marker| line.starts_with(marker))
}

/// Every candidate mutation of one source line, in operator order.
pub(in crate::agent) fn line_mutations(line: &str) -> Vec<String> {
    if comment_or_blank(line) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (from, to) in OPERATORS {
        if let Some(at) = line
            .match_indices(from)
            .map(|(at, _)| at)
            .find(|at| !in_string(line, *at))
        {
            out.push(format!("{}{to}{}", &line[..at], &line[at + from.len()..]));
        }
    }
    for (from, to) in WORDS {
        let whole = line.match_indices(from).map(|(at, _)| at).find(|at| {
            let before = line[..*at].chars().next_back();
            let after = line[at + from.len()..].chars().next();
            !before.is_some_and(word_char) && !after.is_some_and(word_char) && !in_string(line, *at)
        });
        if let Some(at) = whole {
            out.push(format!("{}{to}{}", &line[..at], &line[at + from.len()..]));
        }
    }
    if let Some((at, len, value)) = integer_literal(line) {
        out.push(format!(
            "{}{}{}",
            &line[..at],
            value.saturating_add(1),
            &line[at + len..]
        ));
    }
    out
}

/// The first standalone decimal integer outside a string: (offset, length, value).
fn integer_literal(line: &str) -> Option<(usize, usize, u64)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let before = line[..start].chars().next_back();
            let after = line[i..].chars().next();
            let standalone = !before.is_some_and(|c| word_char(c) || c == '.')
                && !after.is_some_and(|c| word_char(c) || c == '.');
            if standalone && !in_string(line, start) {
                return Some((start, i - start, line[start..i].parse().ok()?));
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Up to `budget` mutants of lines added or changed this turn, spread evenly across them.
pub(in crate::agent) fn plan(versions: &[TurnVersion], budget: usize) -> Vec<Mutant> {
    let mut candidates = Vec::new();
    for version in versions.iter().filter(|version| version.in_project) {
        let Some(after) = version.after.as_deref() else {
            continue;
        };
        if after.len() > MAX_FILE_BYTES || version.before.as_deref() == Some(after) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(after) else {
            continue;
        };
        let before: std::collections::HashSet<&str> = version
            .before
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(|text| text.lines().collect())
            .unwrap_or_default();
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        for (index, raw) in lines.iter().enumerate() {
            let body = raw.trim_end_matches(['\n', '\r']);
            if before.contains(body) {
                continue;
            }
            // One mutant per line keeps the sample spread across the changed behavior.
            let Some(mutated) = line_mutations(body).into_iter().next() else {
                continue;
            };
            let ending = &raw[body.len()..];
            let mut content = String::with_capacity(text.len() + 8);
            for (other, line) in lines.iter().enumerate() {
                if other == index {
                    content.push_str(&mutated);
                    content.push_str(ending);
                } else {
                    content.push_str(line);
                }
            }
            candidates.push(Mutant {
                path: version.path.clone(),
                line: index + 1,
                original: body.trim().to_owned(),
                mutated: mutated.trim().to_owned(),
                content: content.into_bytes(),
            });
        }
    }
    if candidates.len() <= budget {
        return candidates;
    }
    let total = candidates.len();
    (0..budget)
        .map(|slot| candidates[slot * total / budget].clone())
        .collect()
}

impl Agent<'_> {
    /// Run sampled mutants of this turn's changed lines against a passing behavioral check.
    pub(in crate::agent) fn test_strength(
        &mut self,
        check: &FocusedCheck,
        versions: &[TurnVersion],
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let budget = usize::from(self.config.experimental.mutation_runs);
        if budget == 0
            || !self.behavioral_check(&check.name)
            || !self.step_checks.counterfactual.test_strength.is_empty()
        {
            return None;
        }
        let mutants = plan(versions, budget);
        if mutants.is_empty() {
            return None;
        }
        let bounded = FocusedCheck {
            timeout_secs: match check.timeout_secs {
                0 => WITNESS_TIMEOUT_SECS,
                seconds => seconds.min(WITNESS_TIMEOUT_SECS),
            },
            ..check.clone()
        };
        let (mut killed, mut survived, mut inconclusive, mut streak) = (0, Vec::new(), 0, 0);
        for mutant in &mutants {
            if self.interrupt.load(Ordering::SeqCst) || streak >= MAX_INCONCLUSIVE {
                break;
            }
            let overrides = Overrides::from([(mutant.path.clone(), Some(mutant.content.clone()))]);
            match self.counterfactual_run(&bounded, &overrides, ui) {
                Ok(CheckStatus::CodeFailed) => {
                    killed += 1;
                    streak = 0;
                }
                Ok(CheckStatus::Passed) => {
                    survived.push(mutant);
                    streak = 0;
                }
                _ => {
                    inconclusive += 1;
                    streak += 1;
                }
            }
        }
        let report = json!({
            "mutants": mutants.len(),
            "killed": killed,
            "inconclusive": inconclusive,
            "survived": survived.iter().map(|m| json!({
                "path": m.path, "line": m.line, "original": m.original, "mutated": m.mutated,
            })).collect::<Vec<_>>(),
            "timeout_secs": bounded.timeout_secs,
        });
        self.step_checks
            .counterfactual
            .test_strength
            .insert(check.name.clone(), report);
        let ran = killed + survived.len();
        if ran == 0 {
            return Some(format!(
                "[test strength: not established for {}; {inconclusive} mutant run(s) were inconclusive]",
                check.name
            ));
        }
        if survived.is_empty() {
            return Some(format!(
                "[test strength: {} failed for all {killed} sampled mutants of this turn's changed lines]",
                check.name
            ));
        }
        let mut line = format!(
            "[test strength: {} still passes with {} of {ran} sampled mutants of this turn's changed lines; the check does not distinguish these changes (missing test cases, or equivalent code):",
            check.name,
            survived.len()
        );
        for mutant in survived.iter().take(5) {
            line.push_str(&format!(
                "\n  {}:{}  `{}` -> `{}`",
                mutant.path,
                mutant.line,
                clip(&mutant.original),
                clip(&mutant.mutated)
            ));
        }
        line.push(']');
        Some(line)
    }
}

fn clip(text: &str) -> String {
    if text.chars().count() > 100 {
        format!("{}…", text.chars().take(100).collect::<String>())
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests;
