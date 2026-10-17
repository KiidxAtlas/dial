//! Pinned sessions: a global JSON array of session ids in `<user_dir>/session-pins.json`.
//! Pinned sessions sort first in session lists.
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::platform::fs as platform_fs;

static LOCK: Mutex<()> = Mutex::new(());

fn file(user_dir: &Path) -> PathBuf {
    user_dir.join("session-pins.json")
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
    fs::create_dir_all(user_dir).with_context(|| format!("creating {}", user_dir.display()))?;
    let path = file(user_dir);
    let tmp = user_dir.join(format!(
        ".session-pins.{}.{}.tmp",
        std::process::id(),
        crate::storage::session::random_hex(4)
    ));
    let body = serde_json::to_vec_pretty(ids)?;
    let mut out =
        platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), &tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
    out.write_all(&body)?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
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
