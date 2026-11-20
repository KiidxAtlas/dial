//! Approved plan artifacts: the session copy, the optional project autosave, and the bounded
//! record that compaction carries losslessly so a long implementation keeps its plan.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

/// Largest plan text carried through compaction; longer plans keep their head and point at the
/// saved file for the rest.
pub const MAX_CARRIED_PLAN_BYTES: usize = 16 * 1024;
/// Numbered autosave names tried before giving up.
const MAX_AUTOSAVE_CANDIDATES: usize = 1000;
/// Autosave directory used when `plan.autosave_dir` is empty.
const DEFAULT_AUTOSAVE_DIR: &str = ".dial/plans";

/// A file-name slug from the plan's first Markdown heading, or `plan`.
fn slug(plan: &str) -> String {
    let title = plan
        .lines()
        .find_map(|line| line.trim().strip_prefix('#'))
        .map(|title| title.trim_start_matches('#').trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("plan");
    let mut slug = String::new();
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.truncate(80);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("plan");
    }
    slug
}

/// Saves the plan privately under the session's state directory and returns its path.
pub fn save(state_dir: &Path, session_id: &str, plan: &str) -> Result<PathBuf> {
    ensure!(
        !session_id.is_empty()
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid session id"
    );
    let dir = state_dir.join("plans").join(session_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", slug(plan)));
    crate::security::store::write_private_atomic(&path, plan.as_bytes())?;
    Ok(path)
}

/// The autosave directory for `configured` (`plan.autosave_dir`) in the workspace at `root`.
fn autosave_dir(root: &Path, configured: &str) -> Result<PathBuf> {
    let configured = configured.trim();
    if configured.is_empty() {
        return Ok(root.join(DEFAULT_AUTOSAVE_DIR));
    }
    let path = Path::new(configured);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("plan.autosave_dir {configured:?} must stay inside the workspace (no ..)");
    }
    Ok(root.join(path))
}

/// Copies an approved plan to `<dir>/<slug>-plan.md` (or `-2`, `-3`, ... when taken) with an
/// exclusive create, so an existing file, including a concurrent session's, is never replaced.
/// Returns `None` for an empty plan.
pub fn autosave(root: &Path, configured_dir: &str, plan: &str) -> Result<Option<PathBuf>> {
    if plan.trim().is_empty() {
        return Ok(None);
    }
    let dir = autosave_dir(root, configured_dir)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating plan autosave directory {}", dir.display()))?;
    let stem = format!("{}-plan", slug(plan));
    for index in 1..=MAX_AUTOSAVE_CANDIDATES {
        let name = if index == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{index}.md")
        };
        let path = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(plan.as_bytes())
                    .and_then(|()| file.sync_all())
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(Some(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
    bail!(
        "no free plan file name in {} after {MAX_AUTOSAVE_CANDIDATES} attempts",
        dir.display()
    )
}

/// The `approved_plan` control-state record: the plan text (at most [`MAX_CARRIED_PLAN_BYTES`],
/// cut at a character boundary) and where the full text was saved.
pub fn approved_record(plan: &str, saved: Option<&Path>, autosaved: Option<&Path>) -> Value {
    let mut end = plan.len().min(MAX_CARRIED_PLAN_BYTES);
    while !plan.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "text": &plan[..end],
        "truncated": end < plan.len(),
        "bytes": plan.len(),
        "saved": saved.map(|path| path.display().to_string()),
        "autosaved": autosaved.map(|path| path.display().to_string()),
    })
}

#[cfg(test)]
mod tests;
