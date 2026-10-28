//! Shared build/test run ledger for sessions in one checkout.
//!
//! An entry binds an exact command to a fingerprint of the source tree it ran against, so a peer
//! about to run the same command on the same tree can reuse the finished result or wait for the
//! running one instead of compiling again. Entries are advisory data: they never count as the
//! reader's own verification. A run whose owner is no longer live is dropped; a finished run is
//! published only when the tree fingerprint was unchanged from its start to its end.
use super::{MAX_RECORD, Participant, bounded_text, is_live, transaction};
use crate::platform::{atomic, fs as platform_fs};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const LEDGER: &str = "runs.json";
const MAX_RUNS: usize = 64;
const MAX_COMMAND: usize = 1024;
const MAX_FINGERPRINT: usize = 128;
/// Bounded tail of the run's output kept for peers; the full log stays with its owner.
pub const MAX_SUMMARY: usize = 2048;
const MAX_LEDGER: usize = 2 * MAX_RECORD;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Passed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunEntry {
    /// Peer instance id of the session that ran it.
    pub owner: String,
    pub session_id: String,
    pub command: String,
    pub fingerprint: String,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub summary: String,
    pub artifact: Option<String>,
    pub started_unix: u64,
    pub finished_unix: Option<u64>,
}

/// How a run that just ended is published.
pub struct RunResult<'a> {
    pub exit_code: i32,
    pub summary: &'a str,
    pub artifact: Option<&'a str>,
    /// The tree fingerprint at the end equals the one the run started on.
    pub unchanged: bool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Keep at most `max` bytes from the end of `text`, on a character boundary.
pub fn tail(text: &str, max: usize) -> String {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..]
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

impl Participant {
    /// The newest entry another session holds for this exact command and tree: a finished
    /// result, or a run still going under a live owner.
    pub fn shared_run(&self, command: &str, fingerprint: &str) -> Result<Option<RunEntry>> {
        let _transaction = transaction(&self.dir)?;
        Ok(live_entries(&self.dir)?
            .into_iter()
            .rev()
            .find(|entry| {
                entry.owner != self.id && entry.command == command && entry.fingerprint == fingerprint
            }))
    }

    /// Runs other sessions currently have going, for the peers listing.
    pub fn running_runs(&self) -> Result<Vec<RunEntry>> {
        let _transaction = transaction(&self.dir)?;
        Ok(live_entries(&self.dir)?
            .into_iter()
            .filter(|entry| entry.owner != self.id && entry.status == RunStatus::Running)
            .collect())
    }

    /// Announce that this session is starting `command` on `fingerprint`.
    pub fn begin_run(&self, session_id: &str, command: &str, fingerprint: &str) -> Result<()> {
        bounded_text(command, MAX_COMMAND, "command")?;
        bounded_text(fingerprint, MAX_FINGERPRINT, "fingerprint")?;
        let _transaction = transaction(&self.dir)?;
        let mut entries = live_entries(&self.dir)?;
        entries.retain(|entry| {
            !(entry.owner == self.id && entry.command == command && entry.fingerprint == fingerprint)
        });
        entries.push(RunEntry {
            owner: self.id.clone(),
            session_id: tail(session_id, 256),
            command: command.to_owned(),
            fingerprint: fingerprint.to_owned(),
            status: RunStatus::Running,
            exit_code: None,
            summary: String::new(),
            artifact: None,
            started_unix: now(),
            finished_unix: None,
        });
        save(&self.dir, entries)
    }

    /// Publish this session's result, or withdraw the announcement when `result` is `None`
    /// (the run was cancelled or timed out) or the tree changed during the run.
    pub fn finish_run(
        &self,
        command: &str,
        fingerprint: &str,
        result: Option<RunResult<'_>>,
    ) -> Result<()> {
        let _transaction = transaction(&self.dir)?;
        let mut entries = live_entries(&self.dir)?;
        let position = entries.iter().position(|entry| {
            entry.owner == self.id
                && entry.command == command
                && entry.fingerprint == fingerprint
                && entry.status == RunStatus::Running
        });
        let Some(position) = position else {
            return Ok(());
        };
        match result {
            Some(result) if result.unchanged => {
                let entry = &mut entries[position];
                entry.status = if result.exit_code == 0 {
                    RunStatus::Passed
                } else {
                    RunStatus::Failed
                };
                entry.exit_code = Some(result.exit_code);
                entry.summary = tail(result.summary, MAX_SUMMARY);
                entry.artifact = result.artifact.map(|uri| tail(uri, 512));
                entry.finished_unix = Some(now());
            }
            _ => {
                entries.remove(position);
            }
        }
        save(&self.dir, entries)
    }
}

/// Entries that are still meaningful: finished results, and running ones whose owner is live.
fn live_entries(dir: &Path) -> Result<Vec<RunEntry>> {
    let mut entries = load(dir)?;
    let mut kept = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        if entry.status != RunStatus::Running || is_live(dir, &entry.owner)? {
            kept.push(entry);
        }
    }
    Ok(kept)
}

fn load(dir: &Path) -> Result<Vec<RunEntry>> {
    let path = dir.join(LEDGER);
    let file = match platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), &path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    atomic::check_file(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_LEDGER, "run ledger is oversized");
    // A malformed ledger is advisory state: start over rather than block every check command.
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

fn save(dir: &Path, mut entries: Vec<RunEntry>) -> Result<()> {
    let excess = entries.len().saturating_sub(MAX_RUNS);
    entries.drain(..excess);
    let bytes = serde_json::to_vec(&entries).context("serializing run ledger")?;
    ensure!(bytes.len() <= MAX_LEDGER, "run ledger is oversized");
    atomic::write_private(&dir.join(LEDGER), &bytes)
}
