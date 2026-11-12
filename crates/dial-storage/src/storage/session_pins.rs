//! Pinned sessions: a global JSON array of session ids in `<user_dir>/session-pins.json`.
//! Pinned sessions sort first in session lists.
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{
    fs,
    path::{Path, PathBuf},
};

static LOCK: Mutex<()> = Mutex::new(());

fn file(user_dir: &Path) -> PathBuf {
    crate::config::paths::session_pins_file(user_dir)
}

/// Pinned session ids in stored order. A missing file means nothing is pinned.
pub fn load(user_dir: &Path) -> Result<Vec<String>> {
    let _guard = LOCK.lock();
    read(user_dir)
}

fn read(user_dir: &Path) -> Result<Vec<String>> {
    let path = file(user_dir);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Vec<String>>(&text)
            .with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Pin the session when unpinned, unpin it when pinned. Returns `true` when now pinned.
pub fn toggle(user_dir: &Path, session_id: &str) -> Result<bool> {
    let _guard = LOCK.lock();
    let _lock = crate::config::settings_store::locked(&file(user_dir))?;
    let mut ids = read(user_dir)?;
    let now_pinned = if let Some(at) = ids.iter().position(|id| id == session_id) {
        ids.remove(at);
        false
    } else {
        ids.push(session_id.to_string());
        true
    };
    write(user_dir, &ids)?;
    Ok(now_pinned)
}

/// Pin or unpin a session explicitly. Idempotent: a matching stored state writes nothing.
/// Returns whether the stored state changed.
pub fn set(user_dir: &Path, session_id: &str, pinned: bool) -> Result<bool> {
    let _guard = LOCK.lock();
    let _lock = crate::config::settings_store::locked(&file(user_dir))?;
    let mut ids = read(user_dir)?;
    if ids.iter().any(|id| id == session_id) == pinned {
        return Ok(false);
    }
    if pinned {
        ids.push(session_id.to_string());
    } else {
        ids.retain(|id| id != session_id);
    }
    write(user_dir, &ids)?;
    Ok(true)
}

/// Remove a pin if present (used when the session is deleted).
pub fn unpin(user_dir: &Path, session_id: &str) -> Result<()> {
    let _guard = LOCK.lock();
    let _lock = crate::config::settings_store::locked(&file(user_dir))?;
    let mut ids = read(user_dir)?;
    let before = ids.len();
    ids.retain(|id| id != session_id);
    if ids.len() != before {
        write(user_dir, &ids)?;
    }
    Ok(())
}

/// Atomic replace: write a private temp file, fsync, then rename over the target.
fn write(user_dir: &Path, ids: &[String]) -> Result<()> {
    crate::platform::atomic::private_dir(user_dir)?;
    let path = file(user_dir);
    let mut body = serde_json::to_vec_pretty(ids)?;
    body.push(b'\n');
    crate::platform::atomic::write_private(&path, &body)
}

/// Stable partition: pinned ids first, each group keeps its incoming order.
pub fn sort_pinned_first<T>(
    items: Vec<T>,
    pinned: &[String],
    id_of: impl Fn(&T) -> &str,
) -> Vec<T> {
    let (mut top, rest): (Vec<T>, Vec<T>) = items
        .into_iter()
        .partition(|item| pinned.iter().any(|id| id == id_of(item)));
    top.extend(rest);
    top
}

#[cfg(test)]
mod tests;
