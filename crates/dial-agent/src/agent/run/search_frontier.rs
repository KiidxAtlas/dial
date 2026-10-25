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
            self.searches.clear();
            self.revision = revision;
        }
        let search = Self::empty_search(tool, args, output, is_error)?;
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

    /// A completed, first-page local search with no results. Paged, remote and failed
    /// searches are not evidence that nothing matches.
    fn empty_search(tool: &str, args: &Value, output: &str, is_error: bool) -> Option<Search> {
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
