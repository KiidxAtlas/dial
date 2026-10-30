use super::*;
#[test]
fn structured_cells_keep_sparse_addresses_scalars_and_actual_row_ids() {
    let rows=worksheet("<worksheet><sheetData><row r='4'><c r='A4' t='inlineStr'><is><t>name</t></is></c><c r='C4' t='s'><v>0</v></c></row><row r='7'><c r='A7'><v>12.5</v></c><c r='B7' t='b'><v>1</v></c></row></sheetData></worksheet>",&["shared".into()]).unwrap();
    assert_eq!(
        rows,
        vec![
            json!({"row":4,"cells":["name",null,"shared"]}),
            json!({"row":7,"cells":[12.5,true]})
        ]
    );
}
#[test]
fn formulas_errors_duplicate_coordinates_and_wide_rows_fail_explicitly() {
    for cell in [
        "<c r='A1'><f>1+1</f><v>2</v></c>",
        "<c r='A1' t='e'><v>#DIV/0!</v></c>",
        "<c r='A1'><v>1</v></c><c r='A1'><v>2</v></c>",
        "<c r='DY1'><v>3</v></c>",
    ] {
        assert!(
            worksheet(
                &format!("<worksheet><row r='1'>{cell}</row></worksheet>"),
                &[]
            )
            .is_err(),
            "accepted {cell}"
        );
    }
}
#[test]
fn blank_rows_omitted_and_shared_indices_validated() {
    assert!(
        worksheet("<worksheet><row r='1'><c r='A1'/></row></worksheet>", &[])
            .unwrap()
            .is_empty()
    );
    assert!(
        worksheet(
            "<worksheet><row r='1'><c r='A1' t='s'><v>99</v></c></row></worksheet>",
            &[]
        )
        .is_err()
    );
}
