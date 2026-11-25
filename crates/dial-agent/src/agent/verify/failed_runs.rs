//! Failed runs of files this turn changed. When an `eval` cell or `bash` command that ran a
//! changed file fails with a diagnostic naming that file (a traceback `File "main.py"`, a
//! compiler `main.c:3:` location), the failure stays open until the file changes or the same
//! consumer runs it again successfully. A different consumer passing (compiling the C half of a
//! polyglot after its Python half raised a SyntaxError) does not discharge it.
//!
//! This is evidence bookkeeping, not a verdict: the entry records what was observed, and the
//! completion gate asks for that observation to be resolved or reported.
use std::collections::BTreeMap;
use std::path::Path;

/// One unresolved failure: who ran the file, the file's content fingerprint at that moment, and
/// the first diagnostic line for the notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::agent) struct FailedRun {
    pub(in crate::agent) consumer: String,
    fingerprint: u64,
    pub(in crate::agent) summary: String,
}

#[derive(Default, Debug)]
pub(in crate::agent) struct FailedRuns {
    /// Workspace-relative path → latest unresolved failure.
    open: BTreeMap<String, FailedRun>,
}

/// A run that just settled, described independently of tool output types for testing.
pub(in crate::agent) struct Run<'a> {
    pub(in crate::agent) tool: &'a str,
    /// The command (bash) or cell source (eval) as the model sent it.
    pub(in crate::agent) source: &'a str,
    /// Interpreter label for eval (`py`, `js`); ignored for bash.
    pub(in crate::agent) language: Option<&'a str>,
    pub(in crate::agent) failed: bool,
    /// Diagnostic text the run produced.
    pub(in crate::agent) output: &'a str,
}

impl FailedRuns {
    /// Record or clear failures for `changed` files after `run` settles. `root` resolves the
    /// relative paths to fingerprint their current content.
    pub(in crate::agent) fn observe(&mut self, root: &Path, changed: &[String], run: &Run<'_>) {
        for path in changed {
            let Some(name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(consumer) = consumer(run, name) else {
                continue;
            };
            if !run.failed {
                if self
                    .open
                    .get(path)
                    .is_some_and(|open| open.consumer == consumer)
                {
                    self.open.remove(path);
                }
                continue;
            }
            let Some(summary) = diagnostic_naming(run.output, name) else {
                continue;
            };
            let Some(fingerprint) = fingerprint(&root.join(path)) else {
                continue;
            };
            self.open.insert(
                path.clone(),
                FailedRun {
                    consumer,
                    fingerprint,
                    summary,
                },
            );
        }
    }

    /// Failures whose file is still byte-identical to the content that failed.
    pub(in crate::agent) fn unresolved(&self, root: &Path) -> Vec<(&str, &FailedRun)> {
        self.open
            .iter()
            .filter(|(path, run)| fingerprint(&root.join(path)) == Some(run.fingerprint))
            .map(|(path, run)| (path.as_str(), run))
            .collect()
    }
}

/// Who ran `name` in this call, or `None` when the call did not name the file. For bash this is
/// the program word directly before the file's first mention (`python3 main.py.c` → `python3`).
fn consumer(run: &Run<'_>, name: &str) -> Option<String> {
    if !run.source.contains(name) {
        return None;
    }
    match run.tool {
        "eval" => Some(format!("eval:{}", run.language.unwrap_or("py"))),
        "bash" => {
            let words: Vec<&str> = run
                .source
                .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
                .filter(|word| !word.is_empty())
                .collect();
            let at = words.iter().position(|word| word.contains(name))?;
            let program = words[..at]
                .iter()
                .rev()
                .find(|word| !word.starts_with('-'))
                .map_or("", |word| word.rsplit('/').next().unwrap_or(word));
            Some(format!("bash:{program}"))
        }
        _ => None,
    }
}

/// The diagnostic that locates a parse or compile error in `name`: a Python traceback frame
/// (`File ".../name", line N`) or a `name:LINE` compiler location, followed by a syntax-class
/// message. Runtime exceptions are left alone: a script may raise on bad input by design, but a
/// file that does not parse for one of its consumers is never the intended result.
fn diagnostic_naming(output: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    let located = lines.iter().position(|line| {
        let quoted = format!("{name}\", line ");
        let colon = format!("{name}:");
        line.contains(&quoted)
            || line.match_indices(&colon).any(|(at, _)| {
                line[at + colon.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            })
    })?;
    // The error message follows the location (Python prints it last).
    let message = lines[located..].iter().find(|line| {
        ["SyntaxError", "IndentationError", "TabError", ": error:"]
            .iter()
            .any(|marker| line.contains(marker))
    })?;
    Some(crate::agent::safety::guard::clip_middle(message, 200))
}

fn fingerprint(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests;
