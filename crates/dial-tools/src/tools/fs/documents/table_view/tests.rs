use super::*;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[test]
fn columns_align_under_a_rule_and_ragged_rows_pad() {
    let header = strings(&["#", "name", "score"]);
    let rows = vec![strings(&["0", "alice", "3.5"]), strings(&["1", "bob"])];
    assert_eq!(
        render(&header, &rows),
        "# | name  | score\n--+-------+------\n0 | alice | 3.5\n1 | bob\n"
    );
}

#[test]
fn long_cells_are_whole_but_do_not_widen_their_column() {
    let long = "x".repeat(MAX_PAD + 10);
    let rows = vec![strings(&["0", &long]), strings(&["1", "y"])];
    let out = render(&strings(&["#", "v"]), &rows);
    assert!(out.contains(&format!("0 | {long}\n")), "{out}");
    assert!(
        out.contains(&format!("--+-{}\n", "-".repeat(MAX_PAD))),
        "{out}"
    );
}

#[test]
fn cells_escape_line_breaks_and_separators() {
    assert_eq!(cell("a|b\nc\td\r\u{7}"), "a\\|b\\nc\\td\\r\\u{7}");
}

#[test]
fn row_windows_and_paging_footer() {
    assert_eq!(window(None).unwrap(), (0, DEFAULT_ROWS));
    assert_eq!(window(Some("rows=5")).unwrap(), (5, 100));
    assert_eq!(window(Some("rows=5+2")).unwrap(), (5, 2));
    for bad in ["rows=0+0", "rows=x", "rows=0+10001", "sheet=1"] {
        assert!(window(Some(bad)).is_err(), "{bad}");
    }
    let mut out = String::new();
    page(&mut out, &strings(&["#"]), &[strings(&["5"])], 5, 1, 7);
    assert_eq!(out, "[rows 5..6 of 7]\n#\n-\n5\n[more rows: :rows=6+1]\n");
}
