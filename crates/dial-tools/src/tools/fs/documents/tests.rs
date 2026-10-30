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
    let handle = ws.file_handle("sample.ipynb").unwrap();
    assert!(read_path(&ws, &handle).unwrap().contains("safe prose"));
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
    let objects = ["<< /Type /Catalog /Pages 2 0 R >>".to_string(), "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 2000 1000] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".into(), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(), format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len())];
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
#[ignore = "requires operator-installed Poppler pdftotext and pdftoppm; external smoke enables this test"]
fn pdf_real_poppler_fixture() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("sample.pdf"), pdf_fixture()).unwrap();
    assert!(
        read_path(&ws.clone().without_sandbox(), "sample.pdf:page=1")
            .unwrap()
            .contains("PDF evidence fixture")
    );
    use base64::Engine as _;
    let config = crate::config::types::Config::default();
    let context = crate::tools::integrations::ext::ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, outside_shell| {
            assert!(outside_shell);
            Ok(())
        }),
    };
    let output = crate::tools::read_with_context(
        &context,
        &json!({"path":"sample.pdf:page=1:img"}),
        &AtomicBool::new(false),
        4096,
    );
    assert!(!output.is_error, "{}", output.text);
    let png = base64::engine::general_purpose::STANDARD
        .decode(
            output.data.as_ref().unwrap()["artifact"]["data"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(crate::llm::images::dimensions(&png).unwrap(), (2048, 1024));
    let evidence = output.evidence().unwrap().unwrap();
    assert_eq!(
        evidence.items[0].context.snapshot.as_ref().unwrap().sha256,
        crate::tools::knowledge::evidence::source::snapshot(&pdf_fixture())
            .unwrap()
            .sha256
    );
}
#[test]
fn extracted_output_retains_complete_artifact() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(state.path()).unwrap();
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
    let direct = archives::read_bytes("zip", "iso.zip", &opened.bytes, None).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let job = isolate::Job::Read {
        name: "iso.zip".into(),
        format: "zip".into(),
        member: None,
    };
    // Changing the original path after capture must not change the worker input.
    fs::write(&path, b"not a ZIP anymore").unwrap();
    let isolated = isolate::run(
        "archive",
        &job,
        &opened.bytes,
        std::time::Duration::from_secs(60),
        &cancel,
    )
    .unwrap();
    assert_eq!(isolated, direct);
    assert!(direct.contains("a.txt"));
    assert!(opened.validate(&ws).is_err());
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
    let job = isolate::Job::Read {
        format: "epub".into(),
        name: "iso.epub".into(),
        member: Some("chapter=1".into()),
    };
    let isolated = isolate::run(
        "epub",
        &job,
        &fs::read(&path).unwrap(),
        std::time::Duration::from_secs(60),
        &cancel,
    )
    .unwrap();
    assert_eq!(isolated, direct);
    assert!(direct.starts_with("[chapter 1: First & Last]"), "{direct}");
}

#[test]
fn source_snapshot_reads_documents_and_archives_from_added_roots() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let extra = tempfile::tempdir().unwrap();
    let ws = ws
        .with_additional_roots(&[extra.path().to_owned()])
        .unwrap();
    let notebook = extra.path().join("extra.ipynb");
    fs::write(
        &notebook,
        r#"{"nbformat":4,"cells":[{"cell_type":"markdown","source":"added root text"}]}"#,
    )
    .unwrap();
    assert!(
        read_path(&ws, notebook.to_str().unwrap())
            .unwrap()
            .contains("added root text")
    );
    let archive = extra.path().join("extra.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
    zip.start_file("note.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"added root archive").unwrap();
    zip.finish().unwrap();
    assert!(
        read_path(&ws, &format!("{}:note.txt", archive.display()))
            .unwrap()
            .contains("added root archive")
    );
}

#[test]
fn source_snapshot_cache_detects_same_size_rewrites_with_restored_mtime() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("revision.ipynb");
    let before = r#"{"nbformat":4,"cells":[{"cell_type":"markdown","source":"first revision"}]}"#;
    let after = before.replace("first", "other");
    fs::write(&path, before).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(
        read_path(&ws, "revision.ipynb")
            .unwrap()
            .contains("first revision")
    );
    fs::write(&path, after).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let result = read_path(&ws, "revision.ipynb").unwrap();
    assert!(
        result.contains("other revision") && !result.contains("first revision"),
        "{result}"
    );
}

#[test]
fn source_snapshot_cache_never_bypasses_current_read_denials() {
    use crate::tools::exec::sandbox::Policy;
    let (dir, mut ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("denied.ipynb");
    fs::write(
        &path,
        r#"{"nbformat":4,"cells":[{"cell_type":"markdown","source":"cached text"}]}"#,
    )
    .unwrap();
    assert!(
        read_path(&ws, "denied.ipynb")
            .unwrap()
            .contains("cached text")
    );
    for policy in [
        Policy::Dev {
            deny_read: vec![path.clone()],
            allow_network: false,
            extra_write: vec![],
        },
        Policy::Strict {
            deny_read: vec![path],
            allow_network: false,
            extra_write: vec![],
        },
    ] {
        ws.policy = Some(policy);
        let error = read_path(&ws, "denied.ipynb").unwrap_err();
        assert!(error.contains("denied by workspace read policy"), "{error}");
    }
}
