//! Whole-word textual mentions of names across the workspace. This is lexical evidence, not
//! symbol resolution: a mention may be a different symbol with the same name.

use crate::tools::Workspace;
use crate::tools::fs::walk::Walk;
use regex::Regex;
use std::collections::BTreeSet;

/// Files larger than this are not scanned.
const MAX_FILE_BYTES: u64 = 1 << 20;
/// Total bytes scanned per call.
const MAX_SCANNED_BYTES: u64 = 64 << 20;

/// Mentions found by [`files_mentioning`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Mentions {
    /// Workspace-relative paths (sorted) with the names each mentions.
    pub files: Vec<(String, Vec<String>)>,
    /// More files matched than `limit`, or the walk or byte budget stopped early.
    pub incomplete: bool,
}

/// Up to `limit` workspace files, outside `exclude` (workspace-relative paths), that contain any
/// of `names` as a whole word. Ignore rules and skipped directories follow `grep`.
pub fn files_mentioning(
    ws: &Workspace,
    names: &[String],
    exclude: &BTreeSet<String>,
    limit: usize,
) -> Mentions {
    let mut mentions = Mentions::default();
    if names.is_empty() || limit == 0 {
        return mentions;
    }
    let alternatives = names
        .iter()
        .map(|name| regex::escape(name))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(matcher) = Regex::new(&format!(r"\b(?:{alternatives})\b")) else {
        return mentions;
    };
    let root = ws.root.clone();
    let mut scanned = 0u64;
    let mut walk = Walk::new(ws);
    walk.run(&root, &mut |path| {
        let Ok(relative) = path.strip_prefix(&root) else {
            return true;
        };
        let relative = relative.to_string_lossy().into_owned();
        if exclude.contains(&relative) {
            return true;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return true;
        };
        if meta.len() > MAX_FILE_BYTES {
            return true;
        }
        scanned += meta.len();
        if scanned > MAX_SCANNED_BYTES {
            mentions.incomplete = true;
            return false;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return true;
        };
        if bytes.contains(&0) {
            return true;
        }
        let text = String::from_utf8_lossy(&bytes);
        let found: BTreeSet<&str> = matcher.find_iter(&text).map(|m| m.as_str()).collect();
        if found.is_empty() {
            return true;
        }
        if mentions.files.len() == limit {
            mentions.incomplete = true;
            return false;
        }
        mentions
            .files
            .push((relative, found.into_iter().map(str::to_owned).collect()));
        true
    });
    mentions.incomplete |= walk.truncated;
    mentions
}

#[cfg(test)]
mod tests;
