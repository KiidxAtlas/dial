//! `Did you mean:` candidates appended to a missing-path error.
//!
//! Candidates come from the same ignore-aware [`SearchWalk`] as `glob`, so policy, ignored,
//! linked-worktree and protected (private Dial) entries are never named; secret and
//! dependency/build paths are filtered as well. The search is bounded by files and time so a
//! failed call stays cheap in a huge repository. Ranking is deterministic:
//! case-only difference, small whole-path typo, same file name elsewhere, same name ignoring
//! case or extension, a close name in the nearest existing directory, then a close name anywhere.

use super::walk::SearchWalk;
use crate::tools::Workspace;
use crate::tools::edit::edit_native::fuzzy::{levenshtein_distance, levenshtein_within};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_SUGGESTIONS: usize = 5;
const MAX_CANDIDATE_FILES: usize = 20_000;
const SEARCH_TIME: Duration = Duration::from_millis(300);

/// `error` plus a `Did you mean:` line when it reports a missing path and one of `inputs` names
/// a workspace path that does not exist. The original wording is kept unchanged.
pub(in crate::tools) fn annotate(ws: &Workspace, error: String, inputs: &[&str]) -> String {
    if !(error.contains("no such file") || error.contains("does not exist"))
        || error.contains("\nDid you mean:")
    {
        return error;
    }
    let Some(target) = inputs.iter().find_map(|input| missing_target(ws, input)) else {
        return error;
    };
    let found = suggestions(ws, &target);
    if found.is_empty() {
        error
    } else {
        format!("{error}\nDid you mean: {}", found.join(", "))
    }
}

/// [`annotate`] for an `edit` call: the candidate inputs are the payload's declared targets.
pub(in crate::tools) fn annotate_edit(
    ws: &Workspace,
    args: &serde_json::Value,
    error: String,
) -> String {
    if !(error.contains("no such file") || error.contains("does not exist")) {
        return error;
    }
    match crate::tools::edit::editing::targets(ws.edit_mode, args) {
        Ok(targets) => {
            let inputs: Vec<&str> = targets.iter().map(String::as_str).collect();
            annotate(ws, error, &inputs)
        }
        Err(_) => error,
    }
}
fn missing_target(ws: &Workspace, input: &str) -> Option<PathBuf> {
    let input = input.trim();
    if input.is_empty() || input.contains("://") {
        return None;
    }
    let path = ws.resolve(input).ok()?;
    std::fs::symlink_metadata(&path).is_err().then_some(path)
}

fn slash(path: &Path) -> String {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn name_of(relative: &str) -> &str {
    relative.rsplit('/').next().unwrap_or(relative)
}

fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => name,
        Some(dot) => &name[..dot],
    }
}

/// Up to [`MAX_SUGGESTIONS`] existing workspace paths (directories end in `/`) close to the
/// missing absolute `target`, best first.
pub(in crate::tools) fn suggestions(ws: &Workspace, target: &Path) -> Vec<String> {
    let Some(root) = ws.root_containing(target).map(Path::to_path_buf) else {
        return Vec::new();
    };
    let Ok(relative) = target.strip_prefix(&root) else {
        return Vec::new();
    };
    let wanted = slash(relative);
    let Some(ancestor) = target
        .ancestors()
        .skip(1)
        .find(|dir| dir.starts_with(&root) && dir.is_dir())
        .map(Path::to_path_buf)
    else {
        return Vec::new();
    };
    if wanted.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + SEARCH_TIME;
    let mut files = BTreeSet::new();
    // The nearest existing directory first, so a truncated whole-root walk still covers it.
    collect(ws, &root, &ancestor, deadline, &mut files);
    if ancestor != root {
        collect(ws, &root, &root, deadline, &mut files);
    }
    let mut candidates: BTreeSet<(String, bool)> = BTreeSet::new();
    for file in &files {
        let Ok(rel) = file.strip_prefix(&root) else {
            continue;
        };
        let rel = slash(rel);
        let mut parent = rel.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            candidates.insert((dir.to_owned(), true));
            parent = dir;
        }
        candidates.insert((rel, false));
    }
    let ancestor_rel = ancestor.strip_prefix(&root).map(slash).unwrap_or_default();
    let missing_component = target
        .strip_prefix(&ancestor)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // Lazily: the exclusion check is only paid for the few candidates actually shown.
    rank(&wanted, &ancestor_rel, &missing_component, candidates)
        .into_iter()
        .filter(|(path, _)| {
            let absolute = root.join(path);
            !ws.is_protected(&absolute)
                && !crate::workspace_copy::is_excluded_source_path(&root, &absolute)
        })
        .take(MAX_SUGGESTIONS)
        .map(|(path, directory)| {
            let shown = ws.rel(&root.join(&path));
            if directory {
                format!("{shown}/")
            } else {
                shown
            }
        })
        .collect()
}

fn collect(
    ws: &Workspace,
    root: &Path,
    prefix: &Path,
    deadline: Instant,
    files: &mut BTreeSet<PathBuf>,
) {
    let mut walk = SearchWalk::new(ws, true, true, None);
    walk.run_with_prefix(root, prefix, &mut |path| {
        if files.len() >= MAX_CANDIDATE_FILES || Instant::now() >= deadline {
            return false;
        }
        files.insert(path.to_path_buf());
        true
    });
}

/// `(path, is_directory)` best first; unrelated candidates are dropped. Within a tier, typo
/// matches prefer the smallest edit; name matches prefer paths sharing more leading directories.
fn rank(
    wanted: &str,
    ancestor: &str,
    missing_component: &str,
    candidates: BTreeSet<(String, bool)>,
) -> Vec<(String, bool)> {
    let wanted_parts: Vec<String> = wanted.split('/').map(str::to_lowercase).collect();
    let wanted_lower = wanted.to_lowercase();
    let wanted_chars: Vec<char> = wanted_lower.chars().collect();
    let wanted_name = name_of(wanted);
    let wanted_name_lower = wanted_name.to_lowercase();
    let wanted_name_chars: Vec<char> = wanted_name_lower.chars().collect();
    let wanted_stem = stem_of(&wanted_name_lower).to_owned();
    let component_chars: Vec<char> = missing_component.chars().collect();
    let name_budget = |len: usize| (len / 4).clamp(1, 3);
    let path_budget = (wanted_chars.len() / 10).clamp(2, 6);
    let mut ranked = Vec::new();
    for (path, directory) in candidates {
        let lower = path.to_lowercase();
        let name = name_of(&path);
        let name_lower = name.to_lowercase();
        let in_ancestor = match path.rsplit_once('/') {
            Some((parent, _)) => parent == ancestor,
            None => ancestor.is_empty(),
        };
        let chars: Vec<char> = lower.chars().collect();
        let name_chars: Vec<char> = name_lower.chars().collect();
        let tier = if lower == wanted_lower {
            0
        } else if levenshtein_within(&wanted_chars, &chars, path_budget).is_some() {
            1
        } else if name == wanted_name {
            2
        } else if name_lower == wanted_name_lower || stem_of(&name_lower) == wanted_stem {
            3
        } else if in_ancestor
            && !component_chars.is_empty()
            && levenshtein_within(
                &component_chars,
                &name_chars,
                name_budget(component_chars.len()),
            )
            .is_some()
        {
            4
        } else if levenshtein_within(
            &wanted_name_chars,
            &name_chars,
            name_budget(wanted_name_chars.len()),
        )
        .is_some()
        {
            5
        } else {
            continue;
        };
        let distance = levenshtein_distance(&wanted_lower, &lower);
        let shared = lower
            .split('/')
            .zip(&wanted_parts)
            .take_while(|(a, b)| a == b)
            .count();
        let typo = if tier <= 1 { distance } else { 0 };
        ranked.push((
            (tier, typo, std::cmp::Reverse(shared), distance, path),
            directory,
        ));
    }
    ranked.sort();
    ranked
        .into_iter()
        .map(|((_, _, _, _, path), directory)| (path, directory))
        .collect()
}

#[cfg(test)]
mod tests;
