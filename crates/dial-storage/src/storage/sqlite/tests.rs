use super::*;

#[test]
fn legacy_rows_survive_ordered_upgrades_and_reopening() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO items VALUES(7,'legacy');").unwrap();
    let migrations = [
        "CREATE TABLE IF NOT EXISTS items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);",
        "ALTER TABLE items ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;",
        "UPDATE items SET revision=revision+1;",
    ];
    migrate(&conn, &migrations).unwrap();
    migrate(&conn, &migrations).unwrap();
    let row: (String, i64) = conn
        .query_row("SELECT value,revision FROM items WHERE id=7", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(row, ("legacy".into(), 2));
    assert_eq!(check_version(&conn, 3).unwrap(), 3);
}

#[test]
fn failed_upgrade_rolls_back_schema_and_version_together() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(
        migrate(
            &conn,
            &[
                "CREATE TABLE items(id INTEGER);",
                "ALTER TABLE missing ADD COLUMN value TEXT;"
            ]
        )
        .is_err()
    );
    assert_eq!(check_version(&conn, 2).unwrap(), 0);
    assert!(conn.prepare("SELECT * FROM items").is_err());
}

#[test]
fn newer_database_is_refused_without_mutation_including_readonly_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new.sqlite3");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA user_version=9; CREATE TABLE future(value TEXT); INSERT INTO future VALUES('keep');").unwrap();
    assert!(migrate(&conn, &["DROP TABLE future;"]).is_err());
    let reader =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(check_version(&reader, 1).is_err());
    let value: String = conn
        .query_row("SELECT value FROM future", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "keep");
    assert_eq!(check_version(&conn, 9).unwrap(), 9);
}
