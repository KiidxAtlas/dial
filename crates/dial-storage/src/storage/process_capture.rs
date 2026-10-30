//! Bounded, project-private output for explicitly launched foreground wrappers.
//! Snapshots are advisory source data, never authority to attach to or signal a PID.
use crate::config::paths::Layout;
use crate::platform::{atomic, fs as platform_fs, procinfo::ProcId};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

pub const CAPACITY: usize = 256 * 1024;
const MAX_RECORD: usize = CAPACITY * 5 + 2048;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    generation: String,
    owner: ProcId,
    slot: u8,
    total: u64,
    bytes: Vec<u8>,
    closed: bool,
    error: Option<String>,
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && name != "."
            && name != "..",
        "run name must be 1-32 letters, digits, -, _ or . (not . or ..)"
    );
    Ok(())
}

fn read(path: &Path) -> Result<Snapshot> {
    let bytes =
        atomic::read_private(path)?.context("run capture is missing; approved rearm required")?;
    ensure!(bytes.len() <= MAX_RECORD, "oversized run capture");
    let record: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(
        record.version == 1
            && record.slot < 32
            && record.bytes.len() <= CAPACITY
            && record.total >= record.bytes.len() as u64
            && record.generation.len() == 32,
        "invalid run capture protocol"
    );
    Ok(record)
}

/// One exclusive wrapper lifetime. Dropping releases the name; readers detect crashes via ProcId.
#[derive(Debug)]
pub struct Capture {
    path: PathBuf,
    snapshot: Snapshot,
    _live: File,
}

impl Capture {
    pub fn start(layout: &Layout, name: &str) -> Result<Self> {
        validate_name(name)?;
        layout.ensure_project_outside_storage()?;
        let dir = layout.state_dir.join("runs");
        atomic::private_dir(&dir)?;
        let registry = lock_file(&dir.join("registry.lock"))?;
        FileExt::try_lock_exclusive(&registry).context("run registry is busy; retry")?;
        // A fixed ceiling bounds retained disk state, including after crashes. Clean up only
        // dead owners while serialized against other launchers. Readers fail closed on removal.
        let entries = fs::read_dir(&dir)?
            .take(130)
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(entries.len() < 130, "run registry has too many entries");
        let mut live_count = 0;
        for entry in entries {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let record = read(&entry.path())?;
                if !record.owner.alive() || !slot_busy(&dir, record.slot, &record.generation)? {
                    fs::remove_file(entry.path())?;
                } else {
                    live_count += 1;
                    ensure!(
                        entry.file_name() != format!("{name}.json").as_str(),
                        "run name is already in use"
                    );
                }
            }
        }
        ensure!(live_count < 32, "at most 32 foreground runs per project");
        // Stable lifetime locks are never unlinked, avoiding split lock domains. A pool of
        // 32 slots bounds lock files without making unrelated names collide.
        let mut selected = None;
        for slot in 0..32 {
            let file = lock_file(&dir.join(format!("slot-{slot}.lock")))?;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    selected = Some((slot, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        let (slot, mut live) = selected.context("at most 32 foreground runs per project")?;
        let path = dir.join(format!("{name}.json"));
        let snapshot = Snapshot {
            version: 1,
            generation: crate::storage::session::random_hex(16),
            owner: ProcId::current().context("cannot establish wrapper process identity")?,
            slot,
            total: 0,
            bytes: Vec::new(),
            closed: false,
            error: None,
        };
        live.set_len(0)?;
        live.write_all(snapshot.generation.as_bytes())?;
        live.sync_all()?;
        let capture = Self {
            path,
            snapshot,
            _live: live,
        };
        capture.save()?;
        Ok(capture)
    }

    fn save(&self) -> Result<()> {
        atomic::write_private(&self.path, &serde_json::to_vec(&self.snapshot)?)
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.snapshot.total = self
            .snapshot
            .total
            .checked_add(bytes.len() as u64)
            .context("capture offset overflow")?;
        if bytes.len() >= CAPACITY {
            self.snapshot.bytes.clear();
            self.snapshot
                .bytes
                .extend_from_slice(&bytes[bytes.len() - CAPACITY..]);
        } else {
            let excess = (self.snapshot.bytes.len() + bytes.len()).saturating_sub(CAPACITY);
            self.snapshot.bytes.drain(..excess);
            self.snapshot.bytes.extend_from_slice(bytes);
        }
        self.save()
    }

    pub fn finish(&mut self, error: Option<&str>) -> Result<()> {
        self.snapshot.closed = true;
        self.snapshot.error = error.map(|e| e.chars().take(256).collect());
        self.save()
    }
}

fn lock_file(path: &Path) -> Result<File> {
    let file = platform_fs::open_owner_only(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false),
        path,
    )?;
    atomic::check_file(&file)?;
    Ok(file)
}

use std::io::{Read, Write};

fn slot_busy(dir: &Path, slot: u8, generation: &str) -> Result<bool> {
    let mut file = platform_fs::open_nofollow_nonblocking(
        OpenOptions::new().read(true).write(true),
        &dir.join(format!("slot-{slot}.lock")),
    )?;
    atomic::check_file(&file)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let mut bytes = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take(33)
                .read_to_end(&mut bytes)?;
            Ok(bytes == generation.as_bytes())
        }
        Err(error) => Err(error.into()),
    }
}

/// A future-only cursor pinned to one wrapper generation, independent of agent sessions.
#[derive(Debug)]
pub struct Reader {
    path: PathBuf,
    generation: String,
    owner: ProcId,
    cursor: u64,
}

impl Reader {
    pub fn attach(layout: &Layout, name: &str) -> Result<Self> {
        validate_name(name)?;
        let path = layout.state_dir.join("runs").join(format!("{name}.json"));
        let record = read(&path)?;
        ensure!(
            !record.closed
                && record.owner.alive()
                && slot_busy(
                    path.parent().context("capture directory missing")?,
                    record.slot,
                    &record.generation
                )?,
            "run is not live"
        );
        Ok(Self {
            path,
            generation: record.generation,
            owner: record.owner,
            cursor: record.total,
        })
    }

    pub fn poll(&mut self, limit: usize) -> Result<(Vec<u8>, bool)> {
        let record = read(&self.path)?;
        ensure!(
            record.generation == self.generation && record.owner == self.owner,
            "run restarted or replaced; approved rearm required"
        );
        ensure!(record.total >= self.cursor, "run capture truncated");
        let first = record.total - record.bytes.len() as u64;
        ensure!(
            self.cursor >= first,
            "run capture overrun ({} bytes missed); approved rearm required",
            first.saturating_sub(self.cursor)
        );
        if let Some(error) = record.error {
            anyhow::bail!("run capture failed: {error}");
        }
        ensure!(
            record.closed
                || (record.owner.alive()
                    && slot_busy(
                        self.path.parent().context("capture directory missing")?,
                        record.slot,
                        &record.generation
                    )?),
            "run wrapper disappeared without completion"
        );
        let start = (self.cursor - first) as usize;
        let end = start.saturating_add(limit).min(record.bytes.len());
        let bytes = record.bytes[start..end].to_vec();
        self.cursor += bytes.len() as u64;
        Ok((bytes, record.closed && self.cursor == record.total))
    }
}

#[cfg(test)]
mod tests;
