//! Per-turn memory of empty native searches. When a `grep` or `glob` finds nothing, the result
//! names up to three earlier empty searches from this turn whose scope overlaps, so the model can
//! widen or change its approach instead of re-probing the same ground. Patterns are compared
//! literally; nothing is inferred about what an alternative search would return. The memory
//! is cleared whenever the workspace may have changed (any checkpointed edit, non-read-only
//! shell command or workspace-modifying extension call) and is never persisted.

use crate::agent::verify::evidence::Revision;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Empty searches remembered per workspace revision.
const REMEMBERED: usize = 16;
/// Earlier searches named in one note.
const SHOWN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Search {
    tool: String,
    pattern: String,
    scopes: Vec<PathBuf>,
    /// Display form of the scope as the model wrote it.
    scope: String,
}

#[derive(Debug, Default)]
pub(in crate::agent) struct EmptySearches {
    revision: Revision,
    searches: Vec<Search>,
    /// One actionable recovery per file until it is read or the workspace changes.
    advised: Vec<PathBuf>,
    pending: Vec<PathBuf>,
}

impl EmptySearches {
    /// Record `tool`'s result and, when it is an empty search, return a note naming earlier
    /// empty searches in an overlapping scope. `revision` is the current workspace revision.
    pub(in crate::agent) fn observe(
        &mut self,
        tool: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        revision: Revision,
    ) -> Option<String> {
        if revision != self.revision {
            *self = Self {
                revision,
                ..Self::default()
            };
        }
        if tool == "read" && !is_error {
            let paths: Vec<_> = args["path"]
                .as_str()
                .into_iter()
                .chain(
                    args["paths"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                )
                .map(|path| normalize(path.split(':').next().unwrap_or(path)))
                .collect();
            self.searches
                .retain(|seen| !seen.scopes.iter().any(|scope| paths.contains(scope)));
            self.advised.retain(|scope| !paths.contains(scope));
            self.pending.retain(|scope| !paths.contains(scope));
        }
        let search = Self::empty_search(tool, args, output, is_error)?;
        if search.scopes.len() == 1
            && let Some(scope) = search.scopes.first()
            && scope.extension().is_some()
            && !self.advised.contains(scope)
            && self
                .searches
                .iter()
                .any(|seen| seen.scopes == search.scopes && seen.pattern != search.pattern)
        {
            if self.pending.len() == REMEMBERED {
                self.pending.remove(0);
            }
            self.pending.push(scope.clone());
            if self.advised.len() == REMEMBERED {
                self.advised.remove(0);
            }
            self.advised.push(scope.clone());
        }
        let earlier: Vec<String> = self
            .searches
            .iter()
            .rev()
            .filter(|seen| overlaps(&seen.scopes, &search.scopes))
            .take(SHOWN)
            .map(|seen| format!("{} `{}` in {}", seen.tool, seen.pattern, seen.scope))
            .collect();
        if !self.searches.contains(&search) {
            if self.searches.len() == REMEMBERED {
                self.searches.remove(0);
            }
            self.searches.push(search);
        }
        (!earlier.is_empty()).then(|| {
            format!(
                "\n[Earlier empty searches this turn in an overlapping scope, with no workspace change since: {}.]\n",
                earlier.join("; ")
            )
        })
    }

    /// Render against the current request's policy until the selected note is delivered.
    pub(in crate::agent) fn advice(&self, single_call: bool) -> Option<String> {
        self.pending
            .first()
            .map(|scope| Self::render_advice(scope, single_call))
    }

    fn render_advice(scope: &Path, single_call: bool) -> String {
        use crate::agent::prompts::recovery;
        let scope = scope.display();
        let next = if single_call {
            recovery::DISCOVERY_ONE_CALL
        } else {
            recovery::DISCOVERY_BATCH
        };
        format!(
            "[Discovery recovery] Distinct searches found no matches in {scope}. {} {next}",
            recovery::DISCOVERY_RECOVERY,
        )
    }

    /// Acknowledge only the selected target, not another file discovered by that reply.
    pub(in crate::agent) fn delivered_advice(&mut self, message: &Value) {
        let text = message["content"].as_str().unwrap_or_default();
        self.pending.retain(|scope| {
            Self::render_advice(scope, false) != text && Self::render_advice(scope, true) != text
        });
    }

    pub(in crate::agent) fn is_advice(message: &Value) -> bool {
        message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("[Discovery recovery]"))
            && message["dial_guidance"] == "loop"
    }

    /// A completed, first-page local search with no results. Paged, remote and failed
    /// searches are not evidence that nothing matches.
    fn empty_search(tool: &str, args: &Value, output: &str, is_error: bool) -> Option<Search> {
        if tool == "bash" {
            return empty_shell_search(args, output);
        }
        if is_error || !matches!(tool, "grep" | "glob") {
            return None;
        }
        if args.get("cursor").is_some_and(|cursor| !cursor.is_null())
            || args["skip"].as_u64().is_some_and(|skip| skip > 0)
        {
            return None;
        }
        let first = output.lines().next().unwrap_or_default();
        if first != "no matches" || !output.contains("walk complete") {
            return None;
        }
        let pattern = args["pattern"].as_str()?.to_owned();
        let scope = args["path"]
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(".");
        if scope.contains("://") {
            return None;
        }
        let scopes = scope
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(normalize)
            .collect();
        let mut display = scope.to_owned();
        if let Some(glob) = args["glob"].as_str() {
            display.push_str(&format!(" (glob {glob})"));
        }
        Some(Search {
            tool: tool.to_owned(),
            pattern,
            scopes,
            scope: display,
        })
    }
}

/// Only one literal grep count against one explicit local file is attributable. Compound
/// scripts, pipelines, diagnostics and incomplete output are not inferred to be empty searches.
fn empty_shell_search(args: &Value, output: &str) -> Option<Search> {
    let words = crate::agent::safety::approval::shell_words(args["command"].as_str()?)?;
    if words.first()? != "grep" {
        return None;
    }
    let mut index = 1;
    let mut count = false;
    while let Some(flag) = words.get(index).filter(|word| word.starts_with('-')) {
        if flag == "--" {
            index += 1;
            break;
        }
        if !flag[1..]
            .chars()
            .all(|c| matches!(c, 'c' | 'n' | 'E' | 'F' | 'i' | 'w' | 'x'))
        {
            return None;
        }
        count |= flag.contains('c');
        index += 1;
    }
    if !count || words.len() != index + 2 {
        return None;
    }
    let file = &words[index + 1];
    if file.contains("://") || file.starts_with('-') || Path::new(file).extension().is_none() {
        return None;
    }
    let scope = args["cwd"]
        .as_str()
        .map_or_else(|| PathBuf::from(file), |cwd| Path::new(cwd).join(file));
    let (body, trailer) = output.split_once("[exit code: 1]")?;
    if body.trim() != "0"
        || trailer.lines().any(|line| {
            let line = line.trim();
            !line.is_empty()
                && !line.starts_with("[shell launch:")
                && !line.starts_with("[hint]")
                && !line.starts_with("[complete output:")
        })
    {
        return None;
    }
    Some(Search {
        tool: "bash grep".into(),
        pattern: words[index].clone(),
        scopes: vec![normalize(&scope.to_string_lossy())],
        scope: scope.to_string_lossy().into_owned(),
    })
}

/// Workspace-relative lexical form: `./src/` and `src` compare equal; `.` is the root.
fn normalize(scope: &str) -> PathBuf {
    Path::new(scope)
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

/// Scopes overlap when one contains the other; the workspace root contains every scope.
fn overlaps(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter()
        .any(|a| right.iter().any(|b| a.starts_with(b) || b.starts_with(a)))
}

#[cfg(test)]
mod tests;
