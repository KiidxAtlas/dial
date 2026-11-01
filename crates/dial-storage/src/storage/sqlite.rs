//! Ordered, transactional schema upgrades shared by the private SQLite stores.
use anyhow::{Result, ensure};
use rusqlite::{Connection, Transaction, TransactionBehavior};

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

#[cfg(test)]
mod tests;
