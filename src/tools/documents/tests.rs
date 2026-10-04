use super::*;
use serde_json::json;
use std::io::Write;
fn read_path(ws: &Workspace, path: &str) -> Result<String, String> { read(ws, &json!({"path":path})).expect("rich dispatch") }
#[test]
fn notebook_cells_ranges_and_text_output_are_not_executed() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let fixture = json!({"nbformat":4,"cells":[
        {"cell_type":"markdown","source":["# Heading\n","safe prose\n"]},
        {"cell_type":"code","execution_count":9,"source":"raise RuntimeError('must not execute')\n", "outputs":[{"output_type":"stream","text":["retained output\n"]},{"data":{"image/png":"binary payload","text/plain":["plain display\n"]}}]}
    ]});
    fs::write(dir.path().join("sample.ipynb"), fixture.to_string()).unwrap();
    let all = read_path(&ws, "sample.ipynb").unwrap();
    assert!(all.contains("Heading") && all.contains("retained output") && all.contains("plain display"));
    assert!(!all.contains("binary payload"));
    let cell = read_path(&ws, "sample.ipynb:cell=2:2-2").unwrap();
    assert!(cell.contains("raise RuntimeError") && !cell.contains("Heading"));
    assert!(read_path(&ws, "sample.ipynb:cell=0").is_err());
}
#[test]
fn zip_tar_and_gzip_read_members_without_extraction() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("sample.zip")).unwrap());
    zip.start_file("docs/readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"alpha\nbeta\ngamma\n").unwrap();
    zip.finish().unwrap();
    assert!(read_path(&ws, "sample.zip").unwrap().contains("docs/readme.txt"));
    let selected = read_path(&ws, "sample.zip:docs/readme.txt:2-2").unwrap();
    assert!(selected.contains("beta") && !selected.contains("alpha"));
    let mut tar = tar::Builder::new(fs::File::create(dir.path().join("sample.tar")).unwrap());
    let mut header = tar::Header::new_gnu(); header.set_size(11); header.set_mode(0o644); header.set_cksum();
    tar.append_data(&mut header, "docs/readme.txt", &b"alpha\nbeta\n"[..]).unwrap(); tar.finish().unwrap();
    assert!(read_path(&ws, "sample.tar:docs/readme.txt").unwrap().contains("alpha"));
    let mut gz = flate2::write::GzEncoder::new(fs::File::create(dir.path().join("sample.txt.gz")).unwrap(), flate2::Compression::default());
    gz.write_all(b"gzip contents\n").unwrap(); gz.finish().unwrap();
    assert!(read_path(&ws, "sample.txt.gz").unwrap().contains("gzip contents"));
    assert!(!dir.path().join("docs").exists());
    assert!(read_path(&ws, "sample.zip:../escape").is_err());
    assert!(read_path(&ws, "sample.zip:/absolute").is_err());
}
#[test]
fn rejects_unsafe_archive_binary_and_expansion() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("unsafe.zip")).unwrap());
    zip.start_file("../escape.txt", zip::write::SimpleFileOptions::default()).unwrap(); zip.write_all(b"bad").unwrap(); zip.finish().unwrap();
    assert!(read_path(&ws, "unsafe.zip").unwrap_err().contains("unsafe archive member"));
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("binary.zip")).unwrap());
    zip.start_file("data.bin", zip::write::SimpleFileOptions::default()).unwrap(); zip.write_all(&[1,0,2]).unwrap(); zip.finish().unwrap();
    assert!(read_path(&ws, "binary.zip:data.bin").unwrap_err().contains("binary"));
    let mut gz = flate2::write::GzEncoder::new(fs::File::create(dir.path().join("bomb.gz")).unwrap(), flate2::Compression::default());
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..9 { gz.write_all(&block).unwrap(); } gz.finish().unwrap();
    assert!(read_path(&ws, "bomb.gz").unwrap_err().contains("8 MiB"));
}
#[test]
fn sqlite_tables_rows_primary_key_and_injection_boundaries() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let db = rusqlite::Connection::open(dir.path().join("sample.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, payload BLOB); INSERT INTO notes VALUES(1,'first',X'0001'),(2,'second',NULL);").unwrap();
    assert!(read_path(&ws, "sample.sqlite").unwrap().contains("CREATE TABLE notes"));
    let row = read_path(&ws, "sample.sqlite:notes:key=2").unwrap();
    assert!(row.contains("second") && !row.contains("first"));
    let first = read_path(&ws, "sample.sqlite:notes?offset=0&limit=1").unwrap();
    assert!(first.contains("first") && first.contains("blob 2 bytes") && !first.contains("second"));
    assert!(read_path(&ws, "sample.sqlite:notes;DROP TABLE notes").is_err());
    assert!(read_path(&ws, "sample.sqlite:notes?q=DELETE").is_err());
    assert_eq!(db.query_row("SELECT count(*) FROM notes", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
}
#[test]
fn rich_dispatch_preserves_plain_reads_and_confines_symlinks() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("plain.rs"), "fn plain() {}\n").unwrap();
    assert!(read(&ws, &json!({"path":"plain.rs"})).is_none());
    assert!(crate::tools::execute(&ws, "read", &json!({"path":"plain.rs"}), 30).text.contains("fn plain"));
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.ipynb"), "{}").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.ipynb"), dir.path().join("escape.ipynb")).unwrap();
    assert!(read_path(&ws, "escape.ipynb").unwrap_err().contains("symlink"));
}
fn pdf_fixture() -> Vec<u8> {
    let stream = "BT /F1 12 Tf 20 80 Td (PDF evidence fixture) Tj ET";
    let objects = ["<< /Type /Catalog /Pages 2 0 R >>".to_string(), "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(), format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len())];
    let mut pdf = String::from("%PDF-1.4\n"); let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() { offsets.push(pdf.len()); pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i+1)); }
    let xref = pdf.len(); pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) { pdf.push_str(&format!("{offset:010} 00000 n \n")); }
    pdf.push_str(&format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")); pdf.into_bytes()
}
#[test]
fn pdf_selectors_reject_invalid_pages_before_starting_parser() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("sample.pdf"), pdf_fixture()).unwrap();
    assert!(needs_approval(&json!({"path":"sample.pdf:page=1"})));
    assert!(read_path(&ws, "sample.pdf:page=0").unwrap_err().contains("pages must"));
    assert!(read_path(&ws, "sample.pdf:page=2-1").is_err());
}
#[test]
#[ignore = "requires operator-installed Poppler pdftotext; external smoke enables this test"]
fn pdf_real_poppler_fixture() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("sample.pdf"), pdf_fixture()).unwrap();
    assert!(read_path(&ws.without_sandbox(), "sample.pdf:page=1").unwrap().contains("PDF evidence fixture"));
}
#[test]
fn extracted_output_retains_complete_artifact() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let store = crate::tools::artifact::ArtifactStore::new(state.path()).unwrap();
    let ws = ws.with_artifacts(store.clone());
    fs::write(dir.path().join("sample.ipynb"), json!({"nbformat":4,"cells":[{"cell_type":"markdown","source":"first\nsecond\nthird\n"}]}).to_string()).unwrap();
    let result = read_path(&ws, "sample.ipynb:2-2").unwrap();
    let uri = result.split("[complete extracted text: ").nth(1).unwrap().split(']').next().unwrap();
    assert!(store.retrieve(uri, 1, 100).unwrap().contains("third"));
}
