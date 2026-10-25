//! `experimental.outcome_gradient_rounds`: one error number per round in which a check result
//! was observed after the turn's first edit. The number is the count of retained failing checks
//! plus distinct failing test names in this round's check outputs. When it has not dropped below
//! its best value for the configured number of scored rounds, one loop note suggests changing
//! approach; the count restarts after the note and whenever the number improves.

use std::collections::BTreeSet;

/// Distinct failing test names in common runner formats (Rust/libtest, pytest, Go, Jest/Vitest).
/// Unrecognized output contributes nothing; the score then rests on failing checks alone.
pub(in crate::agent) fn failing_tests(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        let name = if let Some(rest) = line.strip_prefix("test ")
            && let Some(name) = rest.strip_suffix(" ... FAILED")
        {
            Some(name)
        } else if let Some(rest) = line.strip_prefix("FAILED ") {
            // pytest: `FAILED tests/test_x.py::test_y - AssertionError`
            rest.split(" - ").next()
        } else if let Some(rest) = line.strip_prefix("--- FAIL: ") {
            rest.split_whitespace().next()
        } else {
            // Jest/Vitest failure markers.
            line.strip_prefix("✕ ").or_else(|| line.strip_prefix("× "))
        };
        if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
            names.insert(name.to_owned());
        }
    }
    names
}

#[derive(Default)]
pub(in crate::agent) struct Gradient {
    best: Option<usize>,
    flat: u8,
}

impl Gradient {
    /// Record one scored round; `Some(note)` when `rounds` scored rounds passed without improvement.
    /// `rounds == 0` disables the tracker.
    pub(in crate::agent) fn observe(&mut self, score: usize, rounds: u8) -> Option<String> {
        if rounds == 0 {
            return None;
        }
        match self.best {
            Some(best) if score >= best => self.flat = self.flat.saturating_add(1),
            _ => {
                self.best = Some(score);
                self.flat = 0;
                return None;
            }
        }
        if score == 0 || self.flat < rounds {
            return None;
        }
        self.flat = 0;
        Some(format!(
            "Outcome gradient: across the last {rounds} checked rounds the error count (failing checks plus failing tests) stayed at {score}, not below its best of {}. Repeating the same kind of edit is unlikely to help; re-read the failure, test a different hypothesis, or reduce the change.",
            self.best.unwrap_or(score)
        ))
    }
}

#[cfg(test)]
mod tests;
