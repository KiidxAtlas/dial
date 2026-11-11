use super::*;

fn ods() -> Vec<u8> {
    crate::tools::fs::documents::opendocument::tests::package(
        "application/vnd.oasis.opendocument.spreadsheet",
        r#"<office:spreadsheet><table:table table:name="Data"><table:table-row><table:table-cell office:value-type="string"><text:p>name</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>total</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>apples</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="42"><text:p>42</text:p></table:table-cell></table:table-row></table:table><table:table table:name="Notes"><table:table-row><table:table-cell office:value-type="string"><text:p>hello</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet>"#,
    )
}

#[test]
fn ods_sheets_render_as_aligned_tables() {
    let out = read(&ods(), None).unwrap();
    assert!(out.starts_with("Sheets: Data, Notes\n"), "{out}");
    assert!(out.contains("[sheet Data: 2 rows x 2 columns"), "{out}");
    assert!(
        out.contains("# | A      | B\n--+--------+------\n1 | name   | total\n2 | apples | 42\n"),
        "{out}"
    );
    let notes = read(&ods(), Some("sheet=2")).unwrap();
    assert!(
        notes.contains("hello") && !notes.contains("apples"),
        "{notes}"
    );
    assert!(read(&ods(), Some("sheet=Data")).unwrap().contains("apples"));
    assert!(
        read(&ods(), Some("sheet=Missing"))
            .unwrap_err()
            .contains("Data, Notes")
    );
}

#[test]
fn column_letters_roll_over_like_spreadsheets() {
    for (index, name) in [
        (0, "A"),
        (25, "Z"),
        (26, "AA"),
        (51, "AZ"),
        (52, "BA"),
        (701, "ZZ"),
        (702, "AAA"),
    ] {
        assert_eq!(column_name(index), name);
    }
}
