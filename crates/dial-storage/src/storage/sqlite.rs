//! Ordered, transactional schema upgrades shared by the private SQLite stores.
use crate::{config::paths::Layout, platform::fs as platform_fs};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use std::{
    fs::OpenOptions,
    path::{Component, Path},
    time::Duration,
};

/// Version zero is the unversioned legacy schema, not an empty-database marker.
/// Its first migration must use idempotent DDL and preserve existing rows.
pub(super) fn migrate(conn: &Connection, migrations: &[&str]) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let version = check_version(&tx, migrations.len())?;
    for (index, sql) in migrations.iter().enumerate().skip(version) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
    }
    tx.commit()?;
    Ok(())
}

/// Observational connections check compatibility without silently upgrading the database.
pub(super) fn check_version(conn: &Connection, supported: usize) -> Result<usize> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        version >= 0 && version as u64 <= supported as u64,
        "SQLite schema version {version} is newer than supported version {supported}; use a newer Dial binary"
    );
    Ok(version as usize)
}

/// Opens the owner-private database `file`, which must be a plain file name inside the project's
/// state directory, for writing. The directory is created and secured, the file is created
/// owner-only and never through a link, and the connection runs in WAL mode with `busy_timeout`.
/// `supported_version` is the newest `user_version` the caller's schema understands. Schema setup
/// stays with the caller.
pub fn open_private_writable(
    layout: &Layout,
    file: &str,
    busy_timeout: Duration,
    supported_version: usize,
) -> Result<Connection> {
    layout.ensure_project_outside_storage()?;
    let mut parts = Path::new(file).components();
    ensure!(
        matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ),
        "private database must be a plain file name: {file:?}"
    );
    let conn = open_private_path(&layout.state_dir.join(file), busy_timeout)?;
    check_version(&conn, supported_version)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(conn)
}

/// Secure creation and writable opening common to the project stores and the user-wide stats
/// ledger. The caller owns schema checks, migrations and journal policy.
pub fn open_private_path(path: &Path, busy_timeout: Duration) -> Result<Connection> {
    let parent = path
        .parent()
        .context("private database requires a parent directory")?;
    crate::platform::atomic::private_dir(parent)?;
    // Private directories may have trusted system aliases (e.g. /var on macOS). Resolve only
    // the validated parent, never the file: SQLite must still refuse a linked final component.
    let path = parent
        .canonicalize()?
        .join(path.file_name().context("missing database name")?);
    let file = platform_fs::open_owner_only(
        OpenOptions::new().create(true).read(true).write(true),
        &path,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    platform_fs::set_owner_only_file(&file)?;
    // Reopen the already-created regular file without creating or following a replacement link.
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(&path, flags)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(busy_timeout)?;
    Ok(conn)
}

#[cfg(test)]
mod tests;
