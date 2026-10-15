use super::*;
use serde_json::json;
use std::io::Write;
fn read_path(ws: &Workspace, path: &str) -> Result<String, String> {
    read_streaming(
        ws,
        &json!({"path":path}),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .expect("rich dispatch")
}
#[test]
fn notebook_cells_ranges_and_text_output_are_not_executed() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let fixture = json!({"nbformat":4,"cells":[
        {"cell_type":"markdown","source":["# Heading\n","safe prose\n"]},
        {"cell_type":"code","execution_count":9,"source":"raise RuntimeError('must not execute')\n", "outputs":[{"output_type":"stream","text":["retained output\n"]},{"data":{"image/png":"binary payload","text/plain":["plain display\n"]}}]}
    ]});
    fs::write(dir.path().join("sample.ipynb"), fixture.to_string()).unwrap();
    let all = read_path(&ws, "sample.ipynb").unwrap();
    assert!(
        all.contains("Heading") && all.contains("retained output") && all.contains("plain display")
    );
    assert!(!all.contains("binary payload"));
    let cell = read_path(&ws, "sample.ipynb:cell=2:2-2").unwrap();
    assert!(cell.contains("raise RuntimeError") && !cell.contains("Heading"));
    assert!(read_path(&ws, "sample.ipynb:cell=0").is_err());
}
#[test]
fn zip_tar_and_gzip_read_members_without_extraction() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("sample.zip")).unwrap());
    zip.start_file("docs/readme.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"alpha\nbeta\ngamma\n").unwrap();
    zip.finish().unwrap();
    assert!(
        read_path(&ws, "sample.zip")
            .unwrap()
            .contains("docs/readme.txt")
    );
    let selected = read_path(&ws, "sample.zip:docs/readme.txt:2-2").unwrap();
    assert!(selected.contains("beta") && !selected.contains("alpha"));
    let mut tar = tar::Builder::new(fs::File::create(dir.path().join("sample.tar")).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(11);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "docs/readme.txt", &b"alpha\nbeta\n"[..])
        .unwrap();
    tar.finish().unwrap();
    assert!(
        read_path(&ws, "sample.tar:docs/readme.txt")
            .unwrap()
            .contains("alpha")
    );
    let mut gz = flate2::write::GzEncoder::new(
        fs::File::create(dir.path().join("sample.txt.gz")).unwrap(),
        flate2::Compression::default(),
    );
    gz.write_all(b"gzip contents\n").unwrap();
    gz.finish().unwrap();
    assert!(
        read_path(&ws, "sample.txt.gz")
            .unwrap()
            .contains("gzip contents")
    );
    assert!(!dir.path().join("docs").exists());
    assert!(read_path(&ws, "sample.zip:../escape").is_err());
    assert!(read_path(&ws, "sample.zip:/absolute").is_err());
}
#[test]
fn rejects_unsafe_archive_binary_and_expansion() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("unsafe.zip")).unwrap());
    zip.start_file("../escape.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"bad").unwrap();
    zip.finish().unwrap();
    assert!(read_path(&ws, "unsafe.zip").is_err());
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("binary.zip")).unwrap());
    zip.start_file("data.bin", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&[1, 0, 2]).unwrap();
    zip.finish().unwrap();
    assert!(
        read_path(&ws, "binary.zip:data.bin")
            .unwrap_err()
            .contains("binary")
    );
    let mut gz = flate2::write::GzEncoder::new(
        fs::File::create(dir.path().join("bomb.gz")).unwrap(),
        flate2::Compression::default(),
    );
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..9 {
        gz.write_all(&block).unwrap();
    }
    gz.finish().unwrap();
    assert!(read_path(&ws, "bomb.gz").unwrap_err().contains("8 MiB"));
}
#[test]
fn sqlite_tables_rows_primary_key_and_injection_boundaries() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let db = rusqlite::Connection::open(dir.path().join("sample.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, payload BLOB); INSERT INTO notes VALUES(1,'first',X'0001'),(2,'second',NULL);").unwrap();
    assert!(
        read_path(&ws, "sample.sqlite")
            .unwrap()
            .contains("CREATE TABLE notes")
    );
    let row = read_path(&ws, "sample.sqlite:notes:key=2").unwrap();
    assert!(row.contains("second") && !row.contains("first"));
    let first = read_path(&ws, "sample.sqlite:notes?offset=0&limit=1").unwrap();
    assert!(first.contains("first") && first.contains("blob 2 bytes") && !first.contains("second"));
    assert!(read_path(&ws, "sample.sqlite:notes;DROP TABLE notes").is_err());
    assert!(read_path(&ws, "sample.sqlite:notes?q=DELETE").is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM notes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[cfg(unix)]
#[test]
fn rich_dispatch_preserves_plain_reads_and_confines_symlinks() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("plain.rs"), "fn plain() {}\n").unwrap();
    assert!(
        read_streaming(
            &ws,
            &json!({"path":"plain.rs"}),
            &std::sync::atomic::AtomicBool::new(false)
        )
        .is_none()
    );
    assert!(
        crate::tools::execute(&ws, "read", &json!({"path":"plain.rs"}), 30)
            .text
            .contains("fn plain")
    );
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.ipynb"), "{}").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.ipynb"),
        dir.path().join("escape.ipynb"),
    )
    .unwrap();
    assert!(
        read_path(&ws, "escape.ipynb")
            .unwrap_err()
            .contains("symlink")
    );
}
fn pdf_fixture() -> Vec<u8> {
    let stream = "BT /F1 12 Tf 20 80 Td (PDF evidence fixture) Tj ET";
    let objects = ["<< /Type /Catalog /Pages 2 0 R >>".to_string(), "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(), format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len())];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}
#[test]
fn pdf_selectors_reject_invalid_pages_before_starting_parser() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("sample.pdf"), pdf_fixture()).unwrap();
    assert!(needs_approval(&json!({"path":"sample.pdf:page=1"})));
    assert!(
        read_path(&ws, "sample.pdf:page=0")
            .unwrap_err()
            .contains("pages must")
    );
    assert!(read_path(&ws, "sample.pdf:page=2-1").is_err());
}
#[test]
#[ignore = "requires operator-installed Poppler pdftotext; external smoke enables this test"]
fn pdf_real_poppler_fixture() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("sample.pdf"), pdf_fixture()).unwrap();
    assert!(
        read_path(&ws.without_sandbox(), "sample.pdf:page=1")
            .unwrap()
            .contains("PDF evidence fixture")
    );
}
#[test]
fn extracted_output_retains_complete_artifact() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let store = crate::tools::artifact::ArtifactStore::new(state.path()).unwrap();
    let ws = ws.with_artifacts(store.clone());
    fs::write(
        dir.path().join("sample.ipynb"),
        json!({"nbformat":4,"cells":[{"cell_type":"markdown","source":"first\nsecond\nthird\n"}]})
            .to_string(),
    )
    .unwrap();
    let result = read_path(&ws, "sample.ipynb:2-2").unwrap();
    let uri = result
        .split("[complete extracted text: ")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    assert!(store.retrieve(uri, 1, 100).unwrap().contains("third"));
}

#[test]
fn isolated_archive_parse_matches_in_process_parser() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("iso.zip")).unwrap());
    zip.start_file("a.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"one\ntwo\n").unwrap();
    zip.finish().unwrap();
    let path = ws.resolve("iso.zip").unwrap();
    let opened = cache::Opened::open(&ws, &path).unwrap();
    let direct = archives::read(&opened, &ws, "zip", None).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let job = isolate::Job::Archive {
        root: ws.root().to_owned(),
        path,
        format: "zip".into(),
        member: None,
    };
    let isolated = isolate::run(
        "archive",
        &job,
        &[],
        std::time::Duration::from_secs(60),
        &cancel,
    )
    .unwrap();
    assert_eq!(isolated, direct);
    assert!(direct.contains("a.txt"));
}

#[test]
fn in_memory_conversion_runs_in_the_worker_and_matches_path_read() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(dir.path().join("mem.zip")).unwrap());
    zip.start_file("b.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"alpha\nbeta\n").unwrap();
    zip.finish().unwrap();
    let bytes = fs::read(dir.path().join("mem.zip")).unwrap();
    let converted = convert_bytes(&ws, &bytes, "mem.zip").unwrap();
    assert!(converted.contains("alpha"), "{converted}");
    assert!(converted.contains("beta"), "{converted}");
}

#[test]
fn json_query_runs_jq_filters_with_paging_and_raw_strings() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("data.json"),
        r#"{"name":"alice","items":[{"n":1},{"n":2},{"n":3}]}"#,
    )
    .unwrap();
    let sum = read_path(&ws, "data.json?q=.items | map(.n) | add").unwrap();
    assert!(sum.contains("[result 0]\n6\n"), "{sum}");
    assert!(sum.contains("jq query"), "{sum}");
    let name = read_path(&ws, "data.json?q=.name&raw=true").unwrap();
    assert!(name.contains("[result 0]\nalice\n"), "{name}");
    let many = read_path(&ws, "data.json?q=.items[]&limit=2").unwrap();
    assert!(
        many.contains(r#"{"n":2}"#) && many.contains("1 more results"),
        "{many}"
    );
    let invalid = read_path(&ws, "data.json?q=.items[");
    assert!(invalid.unwrap_err().contains("invalid jq query"));
    let runtime = read_path(&ws, "data.json?q=.items.n");
    assert!(runtime.unwrap_err().contains("jq error"));
}
#[test]
fn jsonl_query_labels_results_by_source_line() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(
        dir.path().join("rows.jsonl"),
        "{\"ok\":false,\"id\":1}\n\n{\"ok\":true,\"id\":2}\n",
    )
    .unwrap();
    let out = read_path(&ws, "rows.jsonl?q=select(.ok) | .id").unwrap();
    assert!(out.contains("[result 0; source line 3]\n2\n"), "{out}");
    assert!(out.contains("1 of 1 results"), "{out}");
}
#[test]
fn jq_query_stops_an_unbounded_generator_at_its_deadline() {
    let (dir, _ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("one.json");
    fs::write(&path, "1").unwrap();
    let started = std::time::Instant::now();
    let err = json::read_with_deadline(
        &path,
        "one.json",
        "q=repeat(1)",
        std::time::Duration::from_millis(50),
    )
    .unwrap_err();
    assert!(err.contains("timed out"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn epub_reads_whole_book_and_single_chapter_through_the_tool_path() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("book.epub"), epub::tests::sample_epub()).unwrap();
    let all = read_path(&ws, "book.epub").unwrap();
    assert!(
        all.contains("[chapter 1: First & Last]") && all.contains("[chapter 2: Second chapter]"),
        "{all}"
    );
    assert!(!all.contains("evil()"), "{all}");
    let second = read_path(&ws, "book.epub:chapter=2").unwrap();
    assert!(
        second.contains("[chapter 2: Second chapter]") && second.contains("- alpha"),
        "{second}"
    );
    let past = read_path(&ws, "book.epub:chapter=9").unwrap_err();
    assert!(past.contains("chapter index outside 1..=2"), "{past}");
}

#[test]
fn isolated_epub_parse_matches_in_process_parser() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("iso.epub"), epub::tests::sample_epub()).unwrap();
    let path = ws.resolve("iso.epub").unwrap();
    let direct = epub::read(fs::File::open(&path).unwrap(), Some("chapter=1")).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let job = isolate::Job::Epub {
        path,
        member: Some("chapter=1".into()),
    };
    let isolated = isolate::run(
        "epub",
        &job,
        &[],
        std::time::Duration::from_secs(60),
        &cancel,
    )
    .unwrap();
    assert_eq!(isolated, direct);
    assert!(direct.starts_with("[chapter 1: First & Last]"), "{direct}");
}
