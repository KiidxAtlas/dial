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

#[test]
fn private_writable_database_is_owner_only_wal_and_refuses_newer_schema() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let path = layout.state_dir.join("sample.sqlite3");
    let conn = open_private_writable(&layout, "sample.sqlite3", Duration::from_secs(1), 1).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert!(platform_fs::is_owner_only(&std::fs::File::open(&path).unwrap()).unwrap());
    conn.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(conn);
    assert!(open_private_writable(&layout, "sample.sqlite3", Duration::from_secs(1), 1).is_err());
    assert!(open_private_writable(&layout, "sample.sqlite3", Duration::from_secs(1), 2).is_ok());
}

#[test]
fn private_writable_database_name_must_be_a_plain_file_name() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    for name in [
        "",
        ".",
        "..",
        "../escape.sqlite3",
        "nested/x.sqlite3",
        "/tmp/absolute.sqlite3",
    ] {
        assert!(
            open_private_writable(&layout, name, Duration::from_secs(1), 1).is_err(),
            "{name:?} must be refused"
        );
    }
    let parent = layout.state_dir.parent().unwrap();
    assert!(!parent.join("escape.sqlite3").exists());
}

#[cfg(unix)]
#[test]
fn private_writable_database_never_follows_a_planted_link() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    crate::platform::atomic::private_dir(&layout.state_dir).unwrap();
    let victim = home.path().join("victim.sqlite3");
    std::fs::write(&victim, b"").unwrap();
    std::os::unix::fs::symlink(&victim, layout.state_dir.join("sample.sqlite3")).unwrap();
    assert!(open_private_writable(&layout, "sample.sqlite3", Duration::from_secs(1), 1).is_err());
}
