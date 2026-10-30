//! The side pane's live dashboard contract: a state snapshot the session publishes and a command
//! inbox the pane writes. Both live in the session's private live directory beside the feed.
//!
//! - `state.json` is replaced atomically by the session at most a few times a second while a pane
//!   is attached. It carries bounded copies of the plan, jobs, workers, side threads and peers; the
//!   pane only renders it and never reads session internals.
//! - `inbox/` holds one file per pane request. The session drains it in name order, deletes each
//!   file before acting, and runs every request through the same owner the main window uses.
//!   Requests are limited to stopping work and messaging a peer; approvals never travel here.
use super::directory;
use crate::agent::workflow::todo::Task;
use crate::config::paths::Layout;
use crate::platform::atomic;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Lines of output kept for each job.
pub(crate) const JOB_LINES: usize = 200;
/// Characters kept of any one answer, output line or detail.
pub(crate) const TEXT_LIMIT: usize = 8 * 1024;
/// Requests handled per drain; the rest wait for the next one.
const DRAIN_LIMIT: usize = 16;
const MAX_REQUEST: usize = 8 * 1024;
const MAX_MESSAGE: usize = 4096;
/// Shapes this build can read. A pane from another version shows a clear refusal instead of guessing.
pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub version: u32,
    pub updated_ms: u64,
    /// A turn is running; requests that need the idle session wait for it.
    pub busy: bool,
    pub plan: Vec<Task>,
    pub jobs: Vec<Job>,
    pub workers: Vec<Worker>,
    pub threads: Vec<Thread>,
    pub peers: Peers,
    /// Outcomes of recent pane requests, oldest first.
    pub outcomes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobKind {
    /// A `bg` process owned by the session's tools.
    #[default]
    Process,
    /// A slash-command job such as `/update`.
    Command,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Job {
    pub kind: JobKind,
    pub name: String,
    pub detail: String,
    pub status: String,
    pub running: bool,
    /// Newest last, bounded to [`JOB_LINES`].
    pub output: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Worker {
    pub id: String,
    pub name: String,
    pub agent: String,
    pub status: String,
    pub task: String,
    pub current: Option<String>,
    pub model: String,
    pub steps: u64,
    pub cost_usd: Option<f64>,
    pub elapsed_ms: Option<u64>,
    pub cancellable: bool,
    /// Recent activity, newest last.
    pub activity: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Thread {
    pub id: String,
    pub title: String,
    pub exchanges: Vec<Exchange>,
    /// The answer streaming into this thread now.
    pub streaming: Option<Exchange>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Exchange {
    pub question: String,
    pub answer: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Peers {
    /// Why the board is unavailable; the rows below are then empty.
    pub error: Option<String>,
    pub scopes: Vec<String>,
    pub overlapping: Vec<String>,
    pub queued: usize,
    pub rows: Vec<PeerRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PeerRow {
    pub id: String,
    pub summary: String,
    pub phase: String,
    pub scopes: Vec<String>,
    pub overlapping: Vec<String>,
}

/// One pane request. Unknown fields or actions are refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    /// Stop a `bg` process.
    StopProcess {
        name: String,
    },
    /// Cancel the running slash-command job with this label.
    CancelCommand {
        label: String,
    },
    CancelWorker {
        id: String,
    },
    /// Stop the streaming `/btw` answer.
    CancelSideAnswer,
    MessagePeer {
        to: String,
        text: String,
    },
}

impl Request {
    fn validate(&self) -> Result<()> {
        let short = |value: &str, what: &str| -> Result<()> {
            ensure!(
                !value.trim().is_empty() && value.len() <= 256 && !value.contains(char::is_control),
                "invalid {what}"
            );
            Ok(())
        };
        match self {
            Self::StopProcess { name } => short(name, "process name"),
            Self::CancelCommand { label } => short(label, "job label"),
            Self::CancelWorker { id } => short(id, "worker id"),
            Self::CancelSideAnswer => Ok(()),
            Self::MessagePeer { to, text } => {
                short(to, "peer id")?;
                ensure!(
                    !text.trim().is_empty() && text.len() <= MAX_MESSAGE,
                    "a peer message must be 1 to {MAX_MESSAGE} bytes"
                );
                Ok(())
            }
        }
    }
}

fn state_path(layout: &Layout, session: &str) -> Result<PathBuf> {
    Ok(directory(layout, session)?.join("state.json"))
}

fn inbox(layout: &Layout, session: &str) -> Result<PathBuf> {
    Ok(directory(layout, session)?.join("inbox"))
}

pub(crate) fn publish(layout: &Layout, session: &str, snapshot: &Snapshot) -> Result<()> {
    let dir = directory(layout, session)?;
    atomic::private_dir(&dir)?;
    atomic::write_private(&dir.join("state.json"), &serde_json::to_vec(snapshot)?)
}

/// The last published snapshot, or `None` before the session publishes one.
pub(crate) fn read(layout: &Layout, session: &str) -> Result<Option<Snapshot>> {
    let Some(bytes) = atomic::read_private(&state_path(layout, session)?)? else {
        return Ok(None);
    };
    let snapshot: Snapshot = serde_json::from_slice(&bytes).context("invalid side-pane state")?;
    ensure!(
        snapshot.version == VERSION,
        "the session publishes side-pane state version {}, this pane reads {VERSION}; run the pane from the same Dial build",
        snapshot.version
    );
    Ok(Some(snapshot))
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Queue a request for the session. Names sort by time, so the session handles them in order.
pub(crate) fn send(layout: &Layout, session: &str, request: &Request) -> Result<()> {
    request.validate()?;
    let dir = inbox(layout, session)?;
    atomic::private_dir(&dir)?;
    let name = format!(
        "{:020}-{:010}-{:06}.json",
        crate::ui::subagent_model::epoch_ms(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed) % 1_000_000
    );
    atomic::write_private(&dir.join(name), &serde_json::to_vec(request)?)
}

fn request_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let parts: Vec<&str> = stem.split('-').collect();
    parts.len() == 3
        && [20, 10, 6]
            .iter()
            .zip(&parts)
            .all(|(len, part)| part.len() == *len && part.bytes().all(|b| b.is_ascii_digit()))
}

/// Take up to [`DRAIN_LIMIT`] queued requests, oldest first. Each file is removed before it is
/// returned, so a request runs at most once; malformed ones come back as errors.
pub(crate) fn drain(layout: &Layout, session: &str) -> Result<Vec<Result<Request>>> {
    let dir = inbox(layout, session)?;
    atomic::validate_private_path(&dir)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut names = Vec::new();
    for entry in entries.take(1024) {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if request_name(&name) {
            names.push(name);
        }
    }
    names.sort();
    names.truncate(DRAIN_LIMIT);
    Ok(names
        .into_iter()
        .map(|name| take(&dir.join(name)))
        .collect())
}

fn take(path: &Path) -> Result<Request> {
    let bytes = atomic::read_private(path);
    // Remove first: a request that fails to parse or act must not be retried forever.
    std::fs::remove_file(path).context("cannot remove side-pane request")?;
    let bytes = bytes?.context("side-pane request vanished")?;
    ensure!(bytes.len() <= MAX_REQUEST, "side-pane request is too large");
    let request: Request = serde_json::from_slice(&bytes).context("invalid side-pane request")?;
    request.validate()?;
    Ok(request)
}

/// `value` cut to `max` bytes on a character boundary, marked when cut.
pub(crate) fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests;
