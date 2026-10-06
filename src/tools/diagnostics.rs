//! Recognizing compiler, test-runner and interpreter diagnostics in command output, so clipping a
//! long result keeps the lines the model needs to fix the problem instead of only its two ends.
use regex::Regex;
use std::sync::LazyLock;

/// Longest kept line; a diagnostic is a pointer to a problem, not the whole stack frame dump.
const MAX_LINE_CHARS: usize = 300;

static DIAGNOSTIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^\s*(fatal error|error|fatal)(\[[A-Z]*\d+\])?:", // rustc, gcc/clang, tsc summary
        r"|:\d+(:\d+)?:\s*(fatal )?error\b", // file:line[:col]: error (C, mypy, eslint)
        r"|^\s*-->\s+\S+:\d+",               // rustc location under an error
        r"|panicked at\b",                   // Rust panics
        r"|^Traceback \(most recent call last\)", // Python
        r"|^\s*[A-Za-z_][\w.]*(Error|Exception)(:|$)", // Python/JS/Java exception lines
        r"|^E {3}",                          // pytest assertion detail
        r"|^(FAIL|FAILED|ERROR)\b|^--- FAIL", // pytest, go test
        r"|\.\.\. FAILED$",                  // cargo test
        r"|^\s*● ",                          // jest failing test header
    ))
    .expect("diagnostic pattern is valid")
});

/// Whether one output line reports a failure worth keeping when its surroundings are clipped.
pub fn is_diagnostic(line: &str) -> bool {
    DIAGNOSTIC.is_match(line)
}

/// A kept diagnostic line, shortened to `MAX_LINE_CHARS`.
pub fn shorten(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// Collects diagnostic lines from a byte stream that is otherwise being discarded, within `budget`
/// bytes. Lines may arrive split across chunks.
pub struct Collector {
    pending: Vec<u8>,
    kept: String,
    budget: usize,
    lines: usize,
    overflow: usize,
}

impl Collector {
    pub fn new(budget: usize) -> Self {
        Collector {
            pending: Vec::new(),
            kept: String::new(),
            budget,
            lines: 0,
            overflow: 0,
        }
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        while let Some(newline) = bytes.iter().position(|&b| b == b'\n') {
            self.extend_pending(&bytes[..newline]);
            let line = std::mem::take(&mut self.pending);
            self.line(&line);
            bytes = &bytes[newline + 1..];
        }
        self.extend_pending(bytes);
    }

    /// Flush a trailing partial line.
    pub fn finish(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line);
        }
    }

    /// The kept lines, newline-terminated, and how many matching lines did not fit.
    pub fn kept(&self) -> (&str, usize, usize) {
        (&self.kept, self.lines, self.overflow)
    }

    fn extend_pending(&mut self, bytes: &[u8]) {
        // Enough of an over-long line to recognize it; the rest is never kept anyway.
        let room = (MAX_LINE_CHARS * 4).saturating_sub(self.pending.len());
        self.pending
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn line(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        if !is_diagnostic(&text) {
            return;
        }
        let line = shorten(&text);
        if self.kept.len() + line.len() + 1 > self.budget {
            self.overflow += 1;
            return;
        }
        self.kept.push_str(&line);
        self.kept.push('\n');
        self.lines += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_toolchain_failures_but_not_passing_summaries() {
        for line in [
            "error[E0308]: mismatched types",
            "error: could not compile `dial` (lib) due to 1 previous error",
            "  --> src/agent/mod.rs:42:9",
            "src/main.c:3:5: error: expected ';' before '}' token",
            "app.py:12: error: Incompatible return value type",
            "thread 'tests::x' panicked at src/lib.rs:9:5:",
            "Traceback (most recent call last):",
            "ValueError: invalid literal for int() with base 10: 'x'",
            "E       assert 1 == 2",
            "FAILED tests/test_app.py::test_parse - AssertionError",
            "--- FAIL: TestParse (0.00s)",
            "test parser::handles_emoji ... FAILED",
            "  ● parser › handles emoji",
        ] {
            assert!(is_diagnostic(line), "{line}");
        }
        for line in [
            "test result: ok. 12 passed; 0 failed",
            "   Compiling dial v0.1.0",
            "warning: unused variable: `x`",
            "test a ... ok",
            "no errors found",
        ] {
            assert!(!is_diagnostic(line), "{line}");
        }
    }

    #[test]
    fn collector_joins_split_lines_and_respects_its_budget() {
        let mut collector = Collector::new(60);
        collector.feed(b"noise\nerror[E0");
        collector.feed(b"308]: mismatched types\n  --> src/a.rs:1:1\nmore noise\n");
        collector.feed(b"error: second failure that does not fit in the remaining budget\n");
        collector.finish();
        let (kept, lines, overflow) = collector.kept();
        assert_eq!(kept, "error[E0308]: mismatched types\n  --> src/a.rs:1:1\n");
        assert_eq!((lines, overflow), (2, 1));
    }
}
