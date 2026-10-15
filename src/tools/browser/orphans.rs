//! Cross-process reaping for browser drivers.
//!
//! Each Dial process owns at most one record file under
//! `<state>/browser/orphans/<owner_pid>-<driver_pid>.json`, naming its driver process group.
//! A later Dial process reaps a driver only when the owner has stayed gone for the grace window,
//! which starts when a sweep first observes the owner dead. Owner and driver are stored as
//! [`ProcId`] identities (PID plus
//! kernel start time), so a recycled PID is never mistaken for either one. A driver whose identity
//! cannot be verified is never signalled. Records that do not parse (including the pre-identity
//! format) have no verifiable identity and are removed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::tools::background::procinfo::ProcId;

/// Reap only records whose owner has been dead AND untouched this long.
pub(super) const GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    pub(super) owner: ProcId,
    pub(super) driver: ProcId,
    /// When a sweep first found the owner dead. The grace window runs from here, so a long-lived
    /// session is never reaped without it. `None` until that first observation.
    pub(super) dead_since_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Liveness {
    Alive,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// Owner or driver still matters, or the grace window has not elapsed.
    Keep,
    /// Owner is gone and the grace window elapsed while the driver lives: kill its group.
    Reap,
    /// Nothing left to reap; drop the record.
    Forget,
}

/// Pure reap decision. `now_ms` and `dead_since_ms` share a clock.
pub(super) fn decide(
    record: &Record,
    now_ms: u64,
    owner: Liveness,
    driver: Liveness,
    grace: Duration,
) -> Action {
    if owner == Liveness::Alive {
        return Action::Keep;
    }
    // The sweep starts the grace clock when it first sees the owner dead; until then, keep.
    let Some(dead_since) = record.dead_since_ms else {
        return Action::Keep;
    };
    let idle = now_ms.saturating_sub(dead_since);
    if idle < grace.as_millis() as u64 {
        return Action::Keep;
    }
    match driver {
        Liveness::Alive => Action::Reap,
        Liveness::Gone => Action::Forget,
    }
}

/// Owner liveness. An unverifiable owner (zero start time) is treated as alive: its state is
/// unknown, so nothing is reaped on its account.
fn owner_liveness(owner: ProcId) -> Liveness {
    if owner.start == 0 || owner.alive() {
        Liveness::Alive
    } else {
        Liveness::Gone
    }
}

/// Driver liveness. The group leader may be a zombie while members remain, so this is
/// zombie-tolerant; an unverifiable driver reads as gone and is never signalled.
fn driver_liveness(driver: ProcId) -> Liveness {
    if driver.same_process() {
        Liveness::Alive
    } else {
        Liveness::Gone
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn record_path(dir: &Path, owner_pid: i32, driver_pid: i32) -> PathBuf {
    dir.join(format!("{owner_pid}-{driver_pid}.json"))
}

/// Persist this process's record atomically. Records are written before any
/// driver can outlive its owner.
pub(super) fn write(dir: &Path, owner: ProcId, driver: ProcId) -> Result<(), String> {
    let record = Record {
        owner,
        driver,
        dead_since_ms: None,
    };
    store(dir, &record_path(dir, owner.pid, driver.pid), &record)
}

/// Atomically replace the record file at `path`.
fn store(dir: &Path, path: &Path, record: &Record) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.persist(path)
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

/// Drop this owner's record after a normal shutdown.
pub(super) fn forget(dir: &Path, owner_pid: i32, driver_pid: i32) {
    let _ = std::fs::remove_file(record_path(dir, owner_pid, driver_pid));
}

/// Reap orphaned drivers left by dead owners. Returns the number of killed groups.
/// `kill_group` is injected so the sweep can be exercised without signals.
pub(super) fn sweep(dir: &Path, kill_group: impl Fn(ProcId)) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut reaped = 0;
    let now = now_ms();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_slice::<Record>(&bytes) else {
            // No verifiable identity (unreadable or pre-identity format): it can never be reaped.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let owner = owner_liveness(record.owner);
        if owner == Liveness::Gone && record.dead_since_ms.is_none() {
            // First sweep to see the owner dead: start its grace window. Nothing is reaped yet.
            record.dead_since_ms = Some(now);
            let _ = store(dir, &path, &record);
            continue;
        }
        let driver = driver_liveness(record.driver);
        match decide(&record, now, owner, driver, GRACE) {
            Action::Keep => {}
            Action::Reap => {
                kill_group(record.driver);
                reaped += 1;
                let _ = std::fs::remove_file(&path);
            }
            Action::Forget => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(dead_since_ms: u64) -> Record {
        Record {
            owner: ProcId { pid: 10, start: 1 },
            driver: ProcId { pid: 20, start: 2 },
            dead_since_ms: Some(dead_since_ms),
        }
    }

    #[test]
    fn live_owner_is_never_reaped() {
        let action = decide(
            &record(0),
            1_000_000,
            Liveness::Alive,
            Liveness::Alive,
            GRACE,
        );
        assert_eq!(action, Action::Keep);
    }

    #[test]
    fn dead_owner_inside_grace_is_kept() {
        let grace_ms = GRACE.as_millis() as u64;
        let action = decide(
            &record(1_000),
            1_000 + grace_ms - 1,
            Liveness::Gone,
            Liveness::Alive,
            GRACE,
        );
        assert_eq!(action, Action::Keep);
    }

    #[test]
    fn dead_owner_after_grace_reaps_live_driver_and_forgets_gone_driver() {
        let grace_ms = GRACE.as_millis() as u64;
        assert_eq!(
            decide(
                &record(1_000),
                1_000 + grace_ms,
                Liveness::Gone,
                Liveness::Alive,
                GRACE
            ),
            Action::Reap
        );
        assert_eq!(
            decide(
                &record(1_000),
                1_000 + grace_ms,
                Liveness::Gone,
                Liveness::Gone,
                GRACE
            ),
            Action::Forget
        );
    }

    #[test]
    fn record_round_trips_and_forget_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ProcId {
            pid: 4242,
            start: 9,
        };
        let driver = ProcId {
            pid: 4343,
            start: 11,
        };
        write(dir.path(), owner, driver).unwrap();
        let path = record_path(dir.path(), 4242, 4343);
        let parsed: Record = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.owner, owner);
        assert_eq!(parsed.driver, driver);
        forget(dir.path(), 4242, 4343);
        assert!(!path.exists());
    }

    #[test]
    fn sweep_ignores_non_record_files_and_keeps_fresh_records() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        write(
            dir.path(),
            ProcId { pid: 7, start: 5 },
            ProcId { pid: 8, start: 6 },
        )
        .unwrap();
        // The owner identity matches no real process, so the owner reads as gone, but the record
        // was just touched, so the grace window keeps it.
        let reaped = sweep(dir.path(), |_| panic!("fresh record must not be reaped"));
        assert_eq!(reaped, 0);
        assert!(record_path(dir.path(), 7, 8).exists());
        assert!(dir.path().join("notes.txt").exists());
    }

    #[test]
    fn legacy_record_without_identity_is_removed_and_never_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("1-2.json");
        std::fs::write(
            &legacy,
            br#"{"owner_pid":1,"owner_start":"x","driver_pid":2,"driver_start":"y","touched_ms":0}"#,
        )
        .unwrap();
        let reaped = sweep(dir.path(), |_| {
            panic!("unverifiable record must not be signalled")
        });
        assert_eq!(reaped, 0);
        assert!(!legacy.exists());
    }

    #[test]
    fn unverifiable_driver_is_never_signalled_even_after_grace() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("7-8.json");
        let stale_record = Record {
            owner: ProcId { pid: 7, start: 5 },
            driver: ProcId { pid: 8, start: 0 },
            dead_since_ms: Some(0),
        };
        std::fs::write(&stale, serde_json::to_vec(&stale_record).unwrap()).unwrap();
        let reaped = sweep(dir.path(), |_| {
            panic!("unverifiable driver must not be signalled")
        });
        assert_eq!(reaped, 0);
        assert!(!stale.exists());
    }
}
