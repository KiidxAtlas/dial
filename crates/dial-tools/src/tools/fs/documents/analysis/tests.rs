use super::*;
use crate::{storage::artifact::ArtifactStore, tools::testkit};
use std::{fs, io::Write};

fn write_doc(path: &std::path::Path, style: &str, text: &str) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    zip.start_file(
        "word/document.xml",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    write!(zip,"<document><body><p><pPr><pStyle val='{style}'/></pPr><r><t>{text}</t></r></p></body></document>").unwrap();
    zip.finish().unwrap();
}
#[test]
fn inspect_expands_document_handles_and_preserves_evidence_source() {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    write_doc(&dir.path().join("a.DOCX"), "Heading1", "Title");
    let handle = ws.file_handle("a.DOCX").unwrap();
    let out = Document.call(&ctx, &json!({"action":"inspect","path":handle}));
    assert!(!out.is_error, "{}", out.text);
    let data = out.data.unwrap();
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(record.evidence.source, handle);
    let layout: Layout = serde_json::from_slice(
        &ws.artifacts()
            .unwrap()
            .evidence_bytes(&record, 1024 * 1024)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(layout.content(), vec!["Title"]);
}

#[test]
fn tool_retains_replayable_revisions_and_separates_content_from_layout() {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    write_doc(&dir.path().join("a.docx"), "Heading1", "Title");
    let a = Document.call(&ctx, &json!({"action":"layout","path":"a.docx"}));
    assert!(!a.is_error, "{}", a.text);
    let reference = a.data.unwrap()["evidence_references"][0]
        .as_str()
        .unwrap()
        .to_owned();
    write_doc(&dir.path().join("a.docx"), "Caption", "Title");
    let compared = Document.call(
        &ctx,
        &json!({"action":"compare","reference":reference,"other":"a.docx"}),
    );
    assert!(!compared.is_error, "{}", compared.text);
    let data = compared.data.unwrap();
    assert_eq!(data["content_changed"], false);
    assert_eq!(data["layout_changed"], true);
    let citation = data["evidence_references"][0].as_str().unwrap();
    let store = ws.artifacts().unwrap();
    let record = store.evidence_record(citation).unwrap();
    assert_eq!(record.evidence.context.parents.len(), 2);
    fs::remove_file(dir.path().join("a.docx")).unwrap();
    for parent in record.evidence.context.parents {
        let parent = store.evidence_record(&parent).unwrap();
        let layout: Layout =
            serde_json::from_slice(&store.evidence_bytes(&parent, 1024 * 1024).unwrap()).unwrap();
        assert_eq!(layout.content(), vec!["Title"]);
    }
}
#[test]
fn rejects_outside_paths_bad_actions_and_unconfirmed_pdf_without_starting_parser() {
    let (_dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    for args in [
        json!({"action":"layout","path":"../escape.docx"}),
        json!({"action":"unknown","path":"a.docx"}),
        json!({"action":"compare","path":"a.docx"}),
        json!({"action":"layout","path":"a.pdf"}),
    ] {
        assert!(Document.call(&ctx, &args).is_error, "accepted {args}");
    }
    assert!(
        Document
            .call(&ctx, &json!({"action":"layout","path":"a.pdf"}))
            .text
            .contains("confirmation")
    );
}
#[test]
fn structured_worksheet_bytes_are_isolated_and_sheet_names_explicit() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut zip = zip::ZipWriter::new(fs::File::create(file.path()).unwrap());
    for (name, body) in [
        (
            "xl/workbook.xml",
            "<workbook xmlns:r='urn:r'><sheets><sheet name='Data' r:id='a'/></sheets></workbook>",
        ),
        (
            "xl/_rels/workbook.xml.rels",
            "<Relationships><Relationship Id='a' Target='worksheets/sheet1.xml'/></Relationships>",
        ),
        (
            "xl/worksheets/sheet1.xml",
            "<worksheet><sheetData><row r='1'><c r='A1' t='inlineStr'><is><t>value</t></is></c></row><row r='4'><c r='A4'><v>42</v></c></row></sheetData></worksheet>",
        ),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    let bytes = fs::read(file.path()).unwrap();
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    fs::write(dir.path().join("sheet.XLSX"), &bytes).unwrap();
    let handle = ws.file_handle("sheet.XLSX").unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let out = Document.call(&ctx, &json!({"action":"inspect","path":handle}));
    assert!(!out.is_error, "{}", out.text);
    let data = out.data.unwrap();
    assert_eq!(data["sheets"][0]["name"], "Data");
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(record.evidence.source, handle);
    let cancel = AtomicBool::new(false);
    let value = super::super::table_bytes(&bytes, Some("Data"), &cancel).unwrap();
    assert_eq!(value["rows"][1], json!({"row":4,"cells":[42]}));
    assert!(super::super::table_bytes(&bytes, Some("Missing"), &cancel).is_err());
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let error = super::super::table_bytes(&bytes, Some("Data"), &cancel).unwrap_err();
    assert!(error.contains("cancel"), "{error}");
}
