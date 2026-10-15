//! Local directory of live collab hosts. Each hosting process writes one
//! owner-only JSON record under `<user_dir>/run/collab-hosts/`. Records carry metadata only, never
//! a link or key. Listing drops records whose process is gone.
use crate::platform::fs as platform_fs;
use crate::tools::background::procinfo::ProcId;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostRecord {
    pub id: String,
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    pub read_only: bool,
    pub guests: usize,
    pub started_ms: u64,
    #[serde(default)]
    pub(crate) proc: ProcId,
}

pub fn dir(user_dir: &Path) -> PathBuf {
    user_dir.join("run").join("collab-hosts")
}

/// Write `record` atomically (private temp file, then rename). Returns the record's path.
pub fn write(user_dir: &Path, record: &HostRecord) -> Result<PathBuf> {
    let dir = dir(user_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.json", record.id));
    let tmp = dir.join(format!(".{}.{}.tmp", record.id, rand::random::<u32>()));
    let body = serde_json::to_vec(record)?;
    let mut out =
        platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), &tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
    out.write_all(&body)?;
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, &path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(path)
}

/// Withdraw a published record. A missing file is not an error.
pub fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Live hosts, sorted by start time. Records of exited processes are deleted as they are found.
pub fn list_live(user_dir: &Path) -> Result<Vec<HostRecord>> {
    let dir = dir(user_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    let mut hosts = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<HostRecord>(&text) else {
            continue;
        };
        if record.proc.alive() {
            hosts.push(record);
        } else {
            remove(&path);
        }
    }
    hosts.sort_by_key(|host| host.started_ms);
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, proc: ProcId) -> HostRecord {
        HostRecord {
            id: id.into(),
            pid: proc.pid as u32,
            proc,
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            read_only: true,
            guests: 2,
            started_ms: 5,
        }
    }

    #[test]
    fn lists_live_hosts_and_prunes_dead_and_recycled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let live_id = ProcId::current().unwrap();
        let dead_id = ProcId {
            pid: i32::MAX - 1,
            start: 1,
        };
        let recycled_id = ProcId {
            start: live_id.start.wrapping_add(1),
            ..live_id
        };
        let live = write(dir.path(), &record("live-1", live_id)).unwrap();
        let dead = write(dir.path(), &record("dead-1", dead_id)).unwrap();
        let recycled = write(dir.path(), &record("recycled-1", recycled_id)).unwrap();
        let hosts = list_live(dir.path()).unwrap();
        assert_eq!(hosts, vec![record("live-1", live_id)]);
        assert!(live.exists());
        assert!(!dead.exists(), "record of a dead process is pruned");
        assert!(!recycled.exists(), "record whose PID was reused is pruned");
        remove(&live);
        assert!(list_live(dir.path()).unwrap().is_empty());
    }
}
