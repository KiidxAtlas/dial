//! Advisory project-local presence and bounded mailboxes in private state, not the checkout.
//!
//! Every transaction takes the stable registry lock before probing instance locks. Instance
//! locks are held for the participant's lifetime and are only ever probed non-blockingly;
//! no transaction waits for a participant. Records and mailboxes are atomically replaced.
//! A busy registry fails after a two-second lock wait. Crashed records and orphan atomic
//! staging files are reaped on join/list; scans fail closed above 1024 directory entries.
//! This is advisory coordination, not workspace exclusion or a durable message service.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use crate::config::paths::Layout;
use crate::platform::{atomic, fs as platform_fs};

const MAX_PEERS: usize = 32;
const MAX_ENTRIES: usize = 1024;
// Each inbox holds at most 32 letters and 64 KiB including serialized envelopes.
// Individual messages are at most 8 KiB; each drain exposes at most two letters (16 KiB).
const MAX_TEXT: usize = 8 * 1024;
const MAX_MESSAGES: usize = 32;
const MAX_INBOX: usize = 64 * 1024;
const MAX_RECORD: usize = 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub id: String,
    pub session_id: String,
    pub summary: String,
    pub phase: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Letter {
    pub from: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    peer: Peer,
    inbox: Vec<Letter>,
}

/// One live process instance, independent of its journal session id. Dropping it removes
/// its presence and inbox; process death releases liveness even if cleanup cannot run.
pub struct Participant {
    dir: PathBuf,
    id: String,
    live: Option<File>,
}

impl Participant {
    pub fn join(layout: &Layout, session_id: &str) -> Result<Self> {
        bounded_text(session_id, 256, "session id")?;
        ensure!(!session_id.is_empty(), "session id must not be empty");
        layout.ensure_project_outside_storage()?;
        let dir = layout.state_dir.join("coordination");
        atomic::private_dir(&dir)?;
        let _transaction = transaction(&dir)?;
        let (_, live_count) = collect(&dir)?;
        ensure!(live_count < MAX_PEERS, "project peer registry is full");
        let id = crate::storage::session::random_hex(16);
        let live = platform_fs::open_owner_only(
            OpenOptions::new().read(true).write(true).create_new(true),
            &dir.join(format!("{id}.live")),
        )?;
        atomic::check_file(&live)?;
        FileExt::try_lock_exclusive(&live)?;
        let participant = Self {
            dir,
            id,
            live: Some(live),
        };
        let record = Record {
            peer: Peer {
                id: participant.id.clone(),
                session_id: session_id.to_owned(),
                summary: String::new(),
                phase: "idle".to_owned(),
                scopes: Vec::new(),
            },
            inbox: Vec::new(),
        };
        // Do not run Drop under the registry lock on a failed publication.
        if let Err(error) = save(&participant.dir, &record) {
            drop(_transaction);
            drop(participant);
            return Err(error);
        }
        Ok(participant)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn publish(&self, summary: &str, phase: &str, scopes: &[String]) -> Result<()> {
        bounded_text(summary, 1024, "summary")?;
        bounded_text(phase, 64, "phase")?;
        let scopes = normalized_scopes(scopes)?;
        let _transaction = transaction(&self.dir)?;
        let mut record = load(&self.dir, &self.id)?;
        record.peer.summary = summary.to_owned();
        record.peer.phase = phase.to_owned();
        record.peer.scopes = scopes;
        save(&self.dir, &record)
    }

    /// Live peers only, excluding this instance. Malformed records are not advertised.
    pub fn peers(&self) -> Result<Vec<Peer>> {
        let _transaction = transaction(&self.dir)?;
        Ok(collect(&self.dir)?
            .0
            .into_iter()
            .filter(|p| p.id != self.id)
            .collect())
    }

    /// Address an exact live instance id. Full or malformed inboxes fail without eviction.
    pub fn send(&self, to: &str, text: &str) -> Result<()> {
        ensure!(valid_id(to), "recipient must be an exact peer instance id");
        ensure!(to != self.id, "cannot send a peer message to self");
        bounded_text(text, MAX_TEXT, "message")?;
        ensure!(!text.trim().is_empty(), "message must not be empty");
        let _transaction = transaction(&self.dir)?;
        ensure!(is_live(&self.dir, to)?, "recipient is no longer live");
        let mut record = load(&self.dir, to)?;
        ensure!(record.inbox.len() < MAX_MESSAGES, "recipient inbox is full");
        record.inbox.push(Letter {
            from: self.id.clone(),
            text: text.to_owned(),
        });
        ensure!(
            serde_json::to_vec(&record.inbox)?.len() <= MAX_INBOX,
            "recipient inbox is full"
        );
        save(&self.dir, &record)
    }

    /// Atomically take up to two oldest letters (at most 16 KiB of text), leaving the
    /// remainder queued for the next poll. A crash after draining can lose delivery.
    pub fn drain(&self) -> Result<Vec<Letter>> {
        let _transaction = transaction(&self.dir)?;
        let mut record = load(&self.dir, &self.id)?;
        let count = record.inbox.len().min(2);
        let inbox = record.inbox.drain(..count).collect();
        if count > 0 {
            save(&self.dir, &record)?;
        }
        Ok(inbox)
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        if let Ok(_transaction) = transaction(&self.dir) {
            // Close first for Windows. No join or probe can race removal under this lock.
            self.live.take();
            let _ = fs::remove_file(self.dir.join(format!("{}.json", self.id)));
            let _ = fs::remove_file(self.dir.join(format!("{}.live", self.id)));
        }
    }
}

fn bounded_text(text: &str, max: usize, name: &str) -> Result<()> {
    ensure!(text.len() <= max, "{name} exceeds {max} bytes");
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "{name} contains control characters"
    );
    Ok(())
}

fn normalized_scopes(scopes: &[String]) -> Result<Vec<String>> {
    ensure!(scopes.len() <= 32, "too many coordination scopes");
    let mut result = Vec::new();
    for scope in scopes {
        bounded_text(scope, 512, "scope")?;
        ensure!(
            !scope.contains(['\\', ':', '\n', '\t']),
            "scope must be a portable relative path"
        );
        let mut parts = Vec::new();
        for component in Path::new(scope).components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str().context("scope is not UTF-8")?),
                Component::CurDir => {}
                _ => bail!("scope must be relative without parent traversal"),
            }
        }
        ensure!(!parts.is_empty(), "scope must name a relative path");
        let normalized = parts.join("/");
        ensure!(
            scope == &normalized,
            "scope must be a canonical relative path"
        );
        if !result.contains(&normalized) {
            result.push(normalized);
        }
    }
    Ok(result)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn private_open(path: &Path, create: bool) -> Result<File> {
    if create {
        match platform_fs::open_owner_only(
            OpenOptions::new().read(true).write(true).create_new(true),
            path,
        ) {
            Ok(file) => {
                atomic::check_file(&file)?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file =
        platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true).write(true), path)?;
    atomic::check_file(&file)?;
    Ok(file)
}

fn transaction(dir: &Path) -> Result<File> {
    atomic::validate_private_path(dir)?;
    let file = private_open(&dir.join("registry.lock"), true)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error) if contended(&error) => {
                ensure!(
                    Instant::now() < deadline,
                    "project coordination registry is busy"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(file)
}

fn contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn is_live(dir: &Path, id: &str) -> Result<bool> {
    let file = match private_open(&dir.join(format!("{id}.live")), false) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(false),
        Err(error) if contended(&error) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn load(dir: &Path, id: &str) -> Result<Record> {
    let file = private_open(&dir.join(format!("{id}.json")), false)?;
    ensure!(
        file.metadata()?.len() <= MAX_RECORD as u64,
        "coordination record is oversized"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD,
        "coordination record is oversized"
    );
    let record: Record = serde_json::from_slice(&bytes).context("malformed coordination record")?;
    ensure!(
        record.peer.id == id && valid_id(id),
        "coordination identity mismatch"
    );
    bounded_text(&record.peer.session_id, 256, "session id")?;
    bounded_text(&record.peer.summary, 1024, "summary")?;
    bounded_text(&record.peer.phase, 64, "phase")?;
    ensure!(
        normalized_scopes(&record.peer.scopes)? == record.peer.scopes,
        "noncanonical scopes"
    );
    ensure!(
        record.inbox.len() <= MAX_MESSAGES && serde_json::to_vec(&record.inbox)?.len() <= MAX_INBOX,
        "invalid inbox size"
    );
    for letter in &record.inbox {
        ensure!(valid_id(&letter.from), "invalid message sender");
        bounded_text(&letter.text, MAX_TEXT, "message")?;
    }
    Ok(record)
}

fn save(dir: &Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    ensure!(
        bytes.len() <= MAX_RECORD,
        "coordination record is oversized"
    );
    atomic::write_private(&dir.join(format!("{}.json", record.peer.id)), &bytes)
}

/// Bound both normal registry growth and crash debris. Unrecognized files are never followed;
/// only our atomic writer's orphan `.tmp*` files are eligible for cleanup under the lock.
fn collect(dir: &Path) -> Result<(Vec<Peer>, usize)> {
    let mut ids = std::collections::BTreeSet::new();
    let mut entries = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        entries += 1;
        ensure!(
            entries <= MAX_ENTRIES,
            "coordination directory has too many entries"
        );
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(id) = name
            .strip_suffix(".live")
            .or_else(|| name.strip_suffix(".json"))
        {
            if valid_id(id) {
                ids.insert(id.to_owned());
            }
        } else if name.starts_with(".tmp") {
            // Validates owner, links and regular-file type before deleting crash debris.
            if private_open(&entry.path(), false).is_ok() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let mut peers = Vec::new();
    let mut live_count = 0;
    for id in ids {
        if is_live(dir, &id)? {
            live_count += 1;
            ensure!(
                live_count <= MAX_PEERS,
                "project peer registry exceeds its limit"
            );
            if let Ok(record) = load(dir, &id) {
                peers.push(record.peer);
            }
        } else {
            // Do not follow, open, truncate or reuse stale paths. Deleting a link does not
            // affect its target. The registry lock prevents another join from reusing ids.
            for suffix in ["json", "live"] {
                match fs::remove_file(dir.join(format!("{id}.{suffix}"))) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok((peers, live_count))
}

#[cfg(test)]
mod tests;
