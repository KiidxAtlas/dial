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

/// A rustc-style `warning:` header. Locations under it (`--> file:line`) point at warnings, not at
/// the failure, and a build with many warnings would otherwise spend the budget on them.
static WARNING_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*warning(\[[A-Za-z_:]*\d*\])?:").expect("warning pattern"));

static LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-->\s+\S+:\d+").expect("location pattern"));

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
    under_warning: bool,
}

impl Collector {
    pub fn new(budget: usize) -> Self {
        Collector {
            pending: Vec::new(),
            kept: String::new(),
            budget,
            lines: 0,
            overflow: 0,
            under_warning: false,
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
        if WARNING_HEADER.is_match(&text) {
            self.under_warning = true;
            return;
        }
        if !is_diagnostic(&text) {
            return;
        }
        if LOCATION.is_match(&text) {
            if self.under_warning {
                return;
            }
        } else {
            self.under_warning = false;
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
mod tests;
