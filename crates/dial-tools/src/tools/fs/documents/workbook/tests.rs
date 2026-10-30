use super::*;

#[test]
fn observes_formulas_caches_and_errors_without_calculating() {
    let mut count = 0;
    let result = inspect_sheet("<worksheet><sheetData><row r='1'><c r='A1'><f>1+1</f><v>900</v></c><c r='B1' t='e'><v>#DIV/0!</v></c><c r='C1'><f t='shared' si='0'/><v>4</v></c></row></sheetData></worksheet>", &mut count).unwrap();
    assert_eq!(count, 2);
    assert_eq!(result["explicit_rows"], 1);
    assert_eq!(result["explicit_cells"], 3);
    assert_eq!(result["formulas"][0]["expression"], "1+1");
    assert_eq!(result["formulas"][0]["cached_lexical_value"], "900");
    assert_eq!(result["formulas"][0]["cached_value_verified"], false);
    assert_eq!(result["formulas"][1]["attributes"]["t"], "shared");
    assert_eq!(result["error_cells"][0]["cell"], "B1");
}

#[test]
fn rejects_ambiguous_addresses_and_formula_overflow() {
    for xml in [
        "<worksheet><row r='1'><c r='B1'/><c r='A1'/></row></worksheet>",
        "<worksheet><row r='1'><c r='A2'/></row></worksheet>",
        "<worksheet><row r='1'/><row r='1'/></worksheet>",
        "<worksheet><f>1+1</f></worksheet>",
        "<worksheet><c r='A1'/></worksheet>",
    ] {
        assert!(inspect_sheet(xml, &mut 0).is_err(), "accepted {xml}");
    }
    let mut count = MAX_FORMULAS;
    assert!(
        inspect_sheet(
            "<worksheet><row r='1'><c r='A1'><f>1+1</f></c></row></worksheet>",
            &mut count
        )
        .is_err()
    );
}

#[test]
fn isolated_workbook_inspection_lists_sheets_and_preserves_formulas() {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, xml) in [
        (
            "xl/workbook.xml",
            "<workbook xmlns:r='urn:r'><sheets><sheet name='Inputs' r:id='a'/><sheet name='Summary' state='hidden' r:id='b'/></sheets></workbook>",
        ),
        (
            "xl/_rels/workbook.xml.rels",
            "<Relationships><Relationship Id='a' Target='worksheets/sheet1.xml'/><Relationship Id='b' Target='worksheets/sheet2.xml'/></Relationships>",
        ),
        (
            "xl/worksheets/sheet1.xml",
            "<worksheet><row r='1'><c r='A1'><v>1</v></c></row></worksheet>",
        ),
        (
            "xl/worksheets/sheet2.xml",
            "<worksheet><row r='1'><c r='A1'><f>Inputs!A1+2</f><v>3</v></c></row></worksheet>",
        ),
    ] {
        writer
            .start_file(path, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let result = super::super::inspect_workbook_bytes(&bytes, &cancel).unwrap();
    assert_eq!(result["sheets"].as_array().unwrap().len(), 2);
    assert_eq!(result["sheets"][1]["state"], "hidden");
    assert_eq!(
        result["sheets"][1]["formulas"][0]["expression"],
        "Inputs!A1+2"
    );
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        super::super::inspect_workbook_bytes(&bytes, &cancel)
            .unwrap_err()
            .contains("cancel")
    );
}
