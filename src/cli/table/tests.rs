use super::*;

#[test]
fn aligns_ascii_rows_without_padding_the_last_column() {
    let rows = [
        ["42".into(), "running".into(), "model".into()],
        ["1234".into(), "idle".into(), "project".into()],
    ];
    assert_eq!(
        render(["PID", "PHASE", "LABEL"], &rows),
        "PID   PHASE    LABEL\n42    running  model\n1234  idle     project\n"
    );
}

#[test]
fn aligns_wide_and_combining_characters_by_display_width() {
    let rows = [
        ["模型模型".into(), "wide".into()],
        ["e\u{301}".into(), "combining".into()],
        ["plain".into(), "ascii".into()],
    ];
    let table = render(["NAME", "KIND"], &rows);
    for (line, last) in table.lines().zip(["KIND", "wide", "combining", "ascii"]) {
        let prefix = line.strip_suffix(last).unwrap();
        assert_eq!(prefix.width(), 10, "{line:?}");
        assert!(!line.ends_with(' '));
    }
}

#[test]
fn empty_table_keeps_headers_and_single_columns_need_no_padding() {
    assert_eq!(render(["NAME", "KIND"], &[]), "NAME  KIND\n");
    assert_eq!(render(["NAME"], &[["model".into()]]), "NAME\nmodel\n");
}
