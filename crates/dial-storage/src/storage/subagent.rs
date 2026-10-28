//! Subagent (worker) transcripts: one JSONL file per job under
//! `<state_dir>/subagents/<parent session>/`. `dial-agent` writes them; this module owns the
//! location so export, dump and pack can read them without depending on the agent crate.
//!
//! Symlinks are never followed: a planted link must not pull files from outside the state tree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::paths::Layout;

/// Directory holding transcripts for one parent session.
pub fn transcript_dir(layout: &Layout, parent_session: &str) -> PathBuf {
    layout
        .state_dir
        .join("subagents")
        .join(safe_component(parent_session))
}

/// Maps arbitrary text to a single path-safe component; empty input becomes `session`.
pub fn safe_component(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_owned()
    } else {
        cleaned
    }
}

/// Regular `.jsonl` transcript files for a parent session, sorted by file name.
pub fn transcript_files(layout: &Layout, parent_session: &str) -> Result<Vec<PathBuf>> {
    let dir = transcript_dir(layout, parent_session);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", dir.display()));
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let meta =
            fs::symlink_metadata(&path).with_context(|| format!("checking {}", path.display()))?;
        if meta.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Every message record of one transcript, oldest first, with its header's `id` as the title.
#[derive(Debug, Clone, PartialEq)]
pub struct Worker {
    pub title: String,
    pub messages: Vec<Value>,
}

/// Reads one transcript's message records. Unparseable lines are skipped, as the viewer does.
pub fn read_worker(path: &Path) -> Result<Worker> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut title = None;
    let mut messages = Vec::new();
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record["type"].as_str() {
            Some("header") => {
                title = record["id"].as_str().map(str::to_owned);
            }
            Some("message") => messages.push(record["message"].clone()),
            _ => {}
        }
    }
    let title = title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("subagent")
            .to_owned()
    });
    Ok(Worker { title, messages })
}

/// Every worker transcript of a parent session, in stable file-name order.
pub fn workers(layout: &Layout, parent_session: &str) -> Result<Vec<Worker>> {
    transcript_files(layout, parent_session)?
        .iter()
        .map(|path| read_worker(path))
        .collect()
}

#[cfg(test)]
mod tests;
