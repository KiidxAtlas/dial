//! Side threads (`/btw`) saved beside their session as `<ts>_<id>.side.json`.
//! The name does not end in `.jsonl`, so session listings never pick the file up.

use crate::config::settings_store::write_private;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SideThreads {
    #[serde(default)]
    pub threads: Vec<SideThread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// An admitted side request and its last durably published prefix. Reopening never reissues it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub thread: String,
    pub question: String,
    pub answer: String,
}

/// Borrowed live view: saving progress does not clone the growing answer or thread history.
#[derive(Serialize)]
pub struct PendingRef<'a> {
    pub thread: &'a str,
    pub question: &'a str,
    pub answer: &'a str,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SideThread {
    pub id: String,
    pub title: String,
    /// Session branch tip used to build the side request; promotion forks this exact ancestry.
    pub anchor: Option<String>,
    /// Active main prompt included in the snapshot before its journal entry existed.
    #[serde(default)]
    pub context_prompt: Option<String>,
    pub exchanges: Vec<SideExchange>,
    /// Full sibling conversation created by promotion, if any.
    #[serde(default)]
    pub promoted_session: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SideExchange {
    pub question: String,
    /// The answer, or the error text when `outcome` is `Failed`.
    pub answer: String,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Answered,
    Cancelled,
    Failed,
    Interrupted,
}

/// The side file that belongs to a session file.
pub fn path_for(session_path: &Path) -> PathBuf {
    session_path.with_extension("side.json")
}

/// Loads the threads at `path`. A missing file is an empty set of threads.
pub fn load(path: &Path) -> Result<SideThreads> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("reading side threads {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SideThreads::default()),
        Err(error) => {
            Err(error).with_context(|| format!("reading side threads {}", path.display()))
        }
    }
}

/// Saves the threads atomically with private permissions.
pub fn save(path: &Path, threads: &SideThreads) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(threads)?)
}

/// Atomically records a live prefix before the controller exposes it.
pub fn save_stream(path: &Path, threads: &SideThreads, pending: PendingRef<'_>) -> Result<()> {
    ensure!(
        threads.pending.is_none(),
        "an unrecovered side request is already saved"
    );
    #[derive(Serialize)]
    struct Snapshot<'a> {
        #[serde(flatten)]
        threads: &'a SideThreads,
        pending: PendingRef<'a>,
    }
    write_private(
        path,
        &serde_json::to_vec_pretty(&Snapshot { threads, pending })?,
    )
}

impl SideThreads {
    pub fn thread(&self, id: &str) -> Option<&SideThread> {
        self.threads.iter().find(|thread| thread.id == id)
    }

    pub fn thread_mut(&mut self, id: &str) -> Option<&mut SideThread> {
        self.threads.iter_mut().find(|thread| thread.id == id)
    }

    /// Adds an empty thread anchored at `anchor` and returns its id.
    pub fn create(&mut self, anchor: Option<String>) -> String {
        let id = (1..)
            .map(|n| n.to_string())
            .find(|id| self.thread(id).is_none())
            .unwrap_or_default();
        self.threads.push(SideThread {
            id: id.clone(),
            title: String::new(),
            anchor,
            context_prompt: None,
            exchanges: Vec::new(),
            promoted_session: None,
        });
        id
    }
}
