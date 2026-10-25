//! Caller obligations: when this turn changed the header of a named declaration (function,
//! method, type), files that mention that name as a whole word and were neither read nor edited
//! this turn are listed in completion evidence. Mentions are lexical, not resolved symbols, so
//! the list is a review prompt, not proof that a caller is broken. Under
//! `verification.strict_requirements` open obligations block completion.

use crate::agent::Agent;
use crate::storage::checkpoint::TurnVersion;
use crate::tools::Workspace;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Files listed per completion.
const MAX_FILES: usize = 20;
/// Changed names searched per completion.
const MAX_NAMES: usize = 16;
/// Names this short or this common mention too many unrelated symbols to be useful.
const MIN_NAME_CHARS: usize = 4;
const COMMON_NAMES: &[&str] = &[
    "main",
    "test",
    "init",
    "self",
    "this",
    "from",
    "into",
    "new",
    "default",
    "clone",
    "drop",
    "next",
    "call",
    "run",
    "get",
    "set",
    "load",
    "save",
    "read",
    "write",
    "open",
    "close",
    "start",
    "stop",
    "send",
    "parse",
    "build",
    "render",
    "update",
    "handle",
    "process",
    "execute",
    "apply",
    "value",
    "name",
    "data",
    "item",
    "list",
    "error",
    "result",
    "config",
    "options",
    "state",
    "__init__",
    "constructor",
    "toString",
    "equals",
    "hashCode",
];

/// `(path, name)` for every named declaration whose header changed or disappeared between the
/// turn-start and current content of a file. New declarations have no existing callers.
pub(in crate::agent) fn changed_signatures(versions: &[TurnVersion]) -> Vec<(String, String)> {
    let mut changed = Vec::new();
    for version in versions {
        let (Some(before), Some(after)) = (&version.before, &version.after) else {
            continue;
        };
        if !version.in_project || before == after {
            continue;
        }
        let path = Path::new(&version.path);
        let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after))
        else {
            continue;
        };
        let (Some(before), Some(after)) = (
            crate::tools::edit::syntax::signatures(path, before),
            crate::tools::edit::syntax::signatures(path, after),
        ) else {
            continue;
        };
        let headers = |signatures: Vec<(String, String)>| {
            let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (name, header) in signatures {
                map.entry(name).or_default().insert(header);
            }
            map
        };
        let after = headers(after);
        for (name, before) in headers(before) {
            if after.get(&name) != Some(&before)
                && name.chars().count() >= MIN_NAME_CHARS
                && !COMMON_NAMES.contains(&name.as_str())
            {
                changed.push((version.path.clone(), name));
            }
        }
    }
    changed
}

/// Completion-evidence record, or `Null` when no tracked signature changed.
pub(in crate::agent) fn evaluate(
    workspace: &Workspace,
    versions: &[TurnVersion],
    visited: &BTreeSet<String>,
) -> Value {
    let changed = changed_signatures(versions);
    if changed.is_empty() {
        return Value::Null;
    }
    let mut names: Vec<String> = Vec::new();
    for (_, name) in &changed {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let names_truncated = names.len() > MAX_NAMES;
    names.truncate(MAX_NAMES);
    let mut exclude = visited.clone();
    exclude.extend(versions.iter().map(|version| version.path.clone()));
    let mentions =
        crate::tools::code::references::files_mentioning(workspace, &names, &exclude, MAX_FILES);
    json!({
        "status": if mentions.files.is_empty() { "clear" } else { "open" },
        "resolution": "lexical_whole_word_not_symbol_resolved",
        "changed_signatures": changed.iter().map(|(path, name)| json!({"path":path,"name":name})).collect::<Vec<_>>(),
        "unvisited_mentions": mentions.files.iter().map(|(path, names)| json!({"path":path,"names":names})).collect::<Vec<_>>(),
        "incomplete": mentions.incomplete || names_truncated,
        "note": "Files that mention a changed declaration and were not read or edited this turn. A mention may be an unrelated symbol with the same name.",
    })
}

/// Workspace-relative files a successful `read` call viewed (`path`, or each of `paths`, with
/// any line selector removed).
pub(in crate::agent) fn read_targets(workspace: &Workspace, args: &Value) -> Vec<String> {
    let mut raw: Vec<&str> = args["path"].as_str().into_iter().collect();
    if let Some(paths) = args["paths"].as_array() {
        raw.extend(paths.iter().filter_map(Value::as_str));
    }
    raw.into_iter()
        .filter_map(|target| {
            // `file`, `file:10-20`, `file:raw:1-40` or `file:outline`: try the longest prefix first.
            let mut candidate = target;
            loop {
                if let Ok(resolved) = workspace.resolve(candidate)
                    && resolved.is_file()
                {
                    return resolved
                        .strip_prefix(workspace.root())
                        .ok()
                        .map(|relative| relative.to_string_lossy().into_owned());
                }
                candidate = candidate.rsplit_once(':')?.0;
            }
        })
        .collect()
}

impl Agent<'_> {
    /// Caller obligations for this turn's checkpointed changes; `Null` when nothing applies.
    pub(in crate::agent) fn caller_obligations(&self) -> Value {
        if self.hermetic {
            return Value::Null;
        }
        let (Some(checkpoints), Some(turn)) = (&self.checkpoints, self.step_checks.turn) else {
            return Value::Null;
        };
        match checkpoints.turn_versions(turn) {
            Ok(versions) => evaluate(&self.workspace, &versions, &self.step_checks.visited_paths),
            Err(_) => Value::Null,
        }
    }

    /// Strict requirements refuse completion while caller obligations are open.
    pub(in crate::agent) fn caller_obligation_blocker(&self) -> Option<String> {
        if !self.config.verification.strict_requirements {
            return None;
        }
        let obligations = self.caller_obligations();
        if obligations["status"] != "open" {
            return None;
        }
        let files: Vec<&str> = obligations["unvisited_mentions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["path"].as_str())
            .collect();
        Some(format!(
            "Cannot complete under strict requirements: this turn changed declaration signatures, and these files mention a changed name but were not read or edited: {}. Read them (and update callers if needed) before finishing, or report a blocker.",
            files.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests;
