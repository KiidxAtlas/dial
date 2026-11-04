//! Ledger of session moves. Each entry says session `session_id` was moved from `from` to `to`.
//! When the source path is opened for writing again (restored from a backup, a sync tool, or a
//! lingering editor buffer), `Session::open_or_fork` sees the entry and forks a sibling instead of
//! appending to a journal whose history now lives at `to`.
//!
//! The ledger is `<user_dir>/session-transfers.json`, written under an exclusive lock with an
//! atomic private write. A corrupt ledger is quarantined beside itself with a warning and reset
//! to empty, so it never blocks opening sessions.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::paths::Layout;
use crate::config::settings_store;
use crate::storage::dump::resolve_allowing_missing;

const LEDGER_FILE: &str = "session-transfers.json";

/// One recorded move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub session_id: String,
    /// Canonical path the session was moved away from.
    pub from: PathBuf,
    /// Canonical path the session now lives at.
    pub to: PathBuf,
    /// Unix milliseconds when the move was recorded.
    pub at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    transfers: Vec<Transfer>,
}

fn ledger_path(layout: &Layout) -> PathBuf {
    crate::config::paths::session_transfers_file(&layout.user_dir)
}

/// Loads the ledger. A corrupt ledger is moved aside to `session-transfers.json.corrupt-<unix-ts>`
/// with a warning, and moved-away detection restarts from an empty history; it must never block
/// opening sessions.
fn load(layout: &Layout) -> Result<Ledger> {
    let path = ledger_path(layout);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(ledger) => Ok(ledger),
        Err(error) => {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let quarantine = path.with_file_name(format!("{LEDGER_FILE}.corrupt-{ts}"));
            std::fs::rename(&path, &quarantine).with_context(|| {
                format!(
                    "quarantining corrupt session transfer ledger {} to {}",
                    path.display(),
                    quarantine.display()
                )
            })?;
            eprintln!(
                "warning: session transfer ledger was corrupt ({error}); moved it to {} and reset moved-away detection",
                quarantine.display()
            );
            Ok(Ledger::default())
        }
    }
}

/// A ledger failure refused before any publication was attempted. Only this marker proves that no
/// entry was written; every other error from a ledger write may have published, so it is uncertain.
#[derive(Debug)]
struct Unpublished(anyhow::Error);

impl std::fmt::Display for Unpublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Unpublished {}

fn unpublished(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Unpublished(error))
}

/// Whether a ledger error occurred before publication, so no entry was written. Callers may remove
/// a copied destination only on `true`; any other error can mean the ledger entry was published.
pub(super) fn definitely_unpublished(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Unpublished>().is_some()
}

/// Takes the ledger lock and loads the current ledger. Every failure here precedes publication.
fn locked_ledger(layout: &Layout) -> Result<(std::fs::File, Ledger)> {
    let prepared = (|| -> Result<(std::fs::File, Ledger)> {
        crate::platform::atomic::private_dir(&layout.user_dir)?;
        let lock = settings_store::locked(&crate::config::paths::session_transfers_lock_file(
            &layout.user_dir,
        ))?;
        Ok((lock, load(layout)?))
    })();
    prepared.map_err(unpublished)
}

/// Records that `session_id` moved from `from` to `to`. A move onto the same path records nothing.
pub(super) fn record(layout: &Layout, session_id: &str, from: &Path, to: &Path) -> Result<()> {
    let from = resolve_allowing_missing(from).map_err(unpublished)?;
    let to = resolve_allowing_missing(to).map_err(unpublished)?;
    if from == to {
        return Ok(());
    }
    let (_lock, mut ledger) = locked_ledger(layout)?;
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    ledger.transfers.push(Transfer {
        session_id: session_id.to_owned(),
        from,
        to,
        at_ms,
    });
    let bytes = serde_json::to_vec_pretty(&ledger).map_err(|e| unpublished(e.into()))?;
    settings_store::write_private(&ledger_path(layout), &bytes)
        .context("writing session transfer ledger")
}

/// Removes only the newest entry recorded exactly as `session_id` moving `from` to `to`; every other
/// entry is kept in order. An absent entry is not an error, since no claim exists for those paths.
/// Any error means the caller cannot prove the entry is gone and must keep the destination.
pub(super) fn rollback(layout: &Layout, session_id: &str, from: &Path, to: &Path) -> Result<()> {
    let from = resolve_allowing_missing(from)?;
    let to = resolve_allowing_missing(to)?;
    let (_lock, mut ledger) = locked_ledger(layout)?;
    let Some(index) = ledger
        .transfers
        .iter()
        .rposition(|t| t.session_id == session_id && t.from == from && t.to == to)
    else {
        return Ok(());
    };
    ledger.transfers.remove(index);
    let bytes = serde_json::to_vec_pretty(&ledger)?;
    settings_store::write_private(&ledger_path(layout), &bytes)
        .context("removing session transfer ledger entry")
}

/// Moves recorded as leaving `path`, in recording order.
pub(super) fn moved_away_from(layout: &Layout, path: &Path) -> Result<Vec<Transfer>> {
    let path = resolve_allowing_missing(path)?;
    Ok(load(layout)?
        .transfers
        .into_iter()
        .filter(|t| t.from == path)
        .collect())
}

#[cfg(test)]
mod tests;
