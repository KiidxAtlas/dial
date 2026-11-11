use super::*;
use serde_json::json;

fn read(ws: &Workspace, path: &str) -> crate::tools::ToolOutput {
    crate::tools::execute(ws, "read", &json!({ "path": path }), 30)
}

/// A WAL database whose last transaction lives only in `main.db-wal` (copied while the writer was
/// still open, so closing it could not checkpoint the frames away).
fn wal_database(dir: &Path) {
    let db = rusqlite::Connection::open(dir.join("main.db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO items(name) VALUES ('a'),('b');
         PRAGMA wal_checkpoint(TRUNCATE);
         INSERT INTO items(name) VALUES ('c'),('d'),('e');",
    )
    .unwrap();
    fs::copy(dir.join("main.db"), dir.join("db.keep")).unwrap();
    fs::copy(dir.join("main.db-wal"), dir.join("wal.keep")).unwrap();
    drop(db);
    for name in ["main.db-wal", "main.db-shm"] {
        let _ = fs::remove_file(dir.join(name));
    }
    fs::rename(dir.join("db.keep"), dir.join("main.db")).unwrap();
    fs::rename(dir.join("wal.keep"), dir.join("main.db-wal")).unwrap();
}

#[test]
fn hex_dump_matches_xxd_layout() {
    assert_eq!(
        hex_dump(b"SQLite format 3\0\x01", 0x20),
        "00000020: 5351 4c69 7465 2066 6f72 6d61 7420 3300  SQLite format 3.\n\
         00000030: 01                                       .\n"
    );
}

#[test]
fn byte_selectors_parse_counts_ranges_and_hex() {
    assert_eq!(parse_bytes_spec("16").unwrap(), (16, None));
    assert_eq!(parse_bytes_spec("16+32").unwrap(), (16, Some(32)));
    assert_eq!(parse_bytes_spec("0x10-0x1f").unwrap(), (16, Some(16)));
    assert!(parse_bytes_spec("4+0").is_err());
    assert!(parse_bytes_spec("9-3").is_err());
    assert!(parse_bytes_spec("x").is_err());
}

#[test]
fn identifies_signatures_without_matching_text() {
    let name = |bytes: &[u8]| identify(bytes).map(|k| k.name);
    assert_eq!(identify(SQLITE_MAGIC).unwrap().format, Some("sqlite"));
    assert_eq!(
        name(&[0x37, 0x7f, 0x06, 0x82, 0, 0]),
        Some("SQLite write-ahead log (WAL)")
    );
    assert_eq!(identify(b"\x7fELF\x02\x01").unwrap().format, Some("object"));
    assert_eq!(identify(b"PAR1\x15\x04").unwrap().format, Some("parquet"));
    assert_eq!(name(b"BZh text that is not bzip2"), None);
    assert_eq!(name(b"MZ"), None);
    assert_eq!(name(b"fn main() {}\n"), None);
    // Latin-1 text has no NUL and no signature, so it stays an ordinary text read.
    assert!(!is_binary(b"caf\xe9 au lait\n", identify(b"caf\xe9")));
    assert!(is_binary(b"\x1f\x8b\x08\x00\xff", identify(b"\x1f\x8b")));
}

#[test]
fn wal_summary_validates_checksum_chain_and_reports_damage() {
    let dir = tempfile::tempdir().unwrap();
    wal_database(dir.path());
    let bytes = fs::read(dir.path().join("main.db-wal")).unwrap();
    let report = wal(&bytes);
    assert!(report.contains("header checksum valid"), "{report}");
    let total = (bytes.len() - 32) / (4096 + 24);
    assert!(
        report.contains(&format!(
            "Frames: {total} complete; {total} pass the salt and checksum chain; {total} committed"
        )),
        "{report}"
    );

    let mut torn = bytes.clone();
    let last = torn.len() - 1;
    torn[last] ^= 1;
    let report = wal(&torn);
    assert!(report.contains(&format!("{} pass", total - 1)), "{report}");
    assert!(report.contains("discards frames"), "{report}");

    let scrambled: Vec<u8> = bytes.iter().map(|b| b ^ 0x42).collect();
    let report = wal(&scrambled);
    assert!(report.contains("header NOT valid"), "{report}");
    assert!(
        report.contains(&format!("Size fits {total} frames of 4096-byte pages")),
        "{report}"
    );
}

#[test]
fn journal_summary_distinguishes_hot_and_finished() {
    let mut hot = JOURNAL_MAGIC.to_vec();
    hot.extend_from_slice(&[0, 0, 0, 3, 0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 2, 0, 0, 0, 16, 0]);
    let report = journal(&hot);
    assert!(
        report.contains("HOT") && report.contains("3 page records"),
        "{report}"
    );
    assert!(report.contains("page size 4096"), "{report}");
    assert!(journal(&[0; 28]).contains("zeroed"));
}

#[test]
fn reading_a_database_reports_a_damaged_wal_and_never_destroys_it() {
    let (dir, ws) = crate::tools::testkit::workspace();
    wal_database(dir.path());
    let wal_path = dir.path().join("main.db-wal");
    let original = fs::read(&wal_path).unwrap();
    let scrambled: Vec<u8> = original.iter().map(|b| b ^ 0x42).collect();
    fs::write(&wal_path, &scrambled).unwrap();

    let out = read(&ws, "main.db");
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("[sidecar main.db-wal:"), "{}", out.text);
    assert!(out.text.contains("header NOT valid"), "{}", out.text);
    assert!(out.text.contains("CREATE TABLE items"), "{}", out.text);
    assert_eq!(
        fs::read(&wal_path).unwrap(),
        scrambled,
        "read must not alter the WAL"
    );

    let view = read(&ws, "main.db-wal");
    assert!(!view.is_error, "{}", view.text);
    assert!(view.text.contains("header NOT valid"), "{}", view.text);
    assert!(view.text.contains("00000000: 753d 44c0"), "{}", view.text);
    assert!(
        view.text.contains("next: `main.db-wal:bytes=512+512`"),
        "{}",
        view.text
    );
    assert_eq!(fs::read(&wal_path).unwrap(), scrambled);

    // A valid WAL is decoded as such, and its committed rows are visible to the read-only open.
    fs::write(&wal_path, &original).unwrap();
    let out = read(&ws, "main.db:items");
    assert!(out.text.contains("header checksum valid"), "{}", out.text);
    assert!(out.text.contains("\"e\""), "{}", out.text);
}

#[test]
fn byte_ranges_page_any_local_file() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("blob.bin"), (0u8..=255).collect::<Vec<_>>()).unwrap();
    let out = read(&ws, "blob.bin:bytes=0x20+16");
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("00000020: 2021 2223"), "{}", out.text);
    assert!(
        out.text
            .contains("[bytes 32..48 of 256 shown; next: `blob.bin:bytes=48+16`]"),
        "{}",
        out.text
    );
    assert!(
        read(&ws, "blob.bin:bytes=300")
            .text
            .contains("past the end")
    );
    let lines = read(&ws, "blob.bin:1-5");
    assert!(
        lines.is_error && lines.text.contains(":bytes=O+N"),
        "{}",
        lines.text
    );
    fs::write(dir.path().join("notes.txt"), "crlf\r\n").unwrap();
    assert!(
        read(&ws, "notes.txt:bytes=0")
            .text
            .contains("6372 6c66 0d0a")
    );
}

#[test]
fn content_identifies_extensionless_databases() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let db = rusqlite::Connection::open(dir.path().join("state")).unwrap();
    db.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .unwrap();
    drop(db);
    let out = read(&ws, "state");
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("CREATE TABLE t"), "{}", out.text);
}

#[test]
fn the_read_tool_decodes_rich_binaries_by_content_and_extension() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut dict = plist::Dictionary::new();
    dict.insert("Name".into(), "dial".into());
    let mut bplist = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_binary(&mut bplist)
        .unwrap();
    fs::write(dir.path().join("Info"), &bplist).unwrap();
    let out = read(&ws, "Info");
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("<string>dial</string>"), "{}", out.text);

    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/tools/fs/documents/certificate/tests/fixtures/c.pem"),
        dir.path().join("server.crt"),
    )
    .unwrap();
    let out = read(&ws, "server.crt");
    assert!(out.text.contains("Subject: CN=dial.test"), "{}", out.text);

    let mut broken = b"PAR1".to_vec();
    broken.extend_from_slice(&[0u8; 40]);
    fs::write(dir.path().join("broken"), &broken).unwrap();
    let out = read(&ws, "broken");
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("[decoding as Apache Parquet file failed"),
        "{}",
        out.text
    );
    assert!(out.text.contains("5041 5231"), "{}", out.text);
}
