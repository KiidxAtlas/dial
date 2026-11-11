use super::*;
use std::io::Write as _;

#[test]
fn csv_renders_header_rows_and_quoted_cells() {
    let csv = b"name,score,note\nalice,3.5,\"says \"\"hi\"\"\"\nbob,4,\"two\nlines\"\ncarol,5\n";
    let out = read("csv", csv, None).unwrap();
    assert_eq!(
        out,
        "CSV: 3 rows x 3 columns; comma-delimited; first row is the header\n\
         [rows 0..3 of 3]\n\
         # | name  | score | note\n\
         --+-------+-------+-----------\n\
         0 | alice | 3.5   | says \"hi\"\n\
         1 | bob   | 4     | two\\nlines\n\
         2 | carol | 5\n"
    );
}

#[test]
fn rows_selector_pages_and_table_selects_the_first_page() {
    let mut csv = String::from("n\n");
    for i in 0..250 {
        csv.push_str(&format!("{i}\n"));
    }
    let out = read("csv", csv.as_bytes(), Some("rows=240+5")).unwrap();
    assert!(out.contains("[rows 240..245 of 250]\n"), "{out}");
    assert!(out.contains("\n240 | 240\n"), "{out}");
    assert!(out.ends_with("[more rows: :rows=245+5]\n"), "{out}");
    let first = read("csv", csv.as_bytes(), Some("table")).unwrap();
    assert!(first.contains("[rows 0..100 of 250]"), "{first}");
    assert!(read("csv", csv.as_bytes(), Some("sheet=1")).is_err());
}

#[test]
fn delimiters_are_sniffed_for_csv_and_fixed_for_tsv() {
    let semicolon = read("csv", b"a;b\n\"1,5\";2\n", None).unwrap();
    assert!(semicolon.contains("semicolon-delimited"), "{semicolon}");
    assert!(semicolon.contains("0 | 1,5 | 2\n"), "{semicolon}");
    let tsv = read("tsv", b"a\tb\nx|y\t2\n", None).unwrap();
    assert!(tsv.contains("tab-delimited"), "{tsv}");
    assert!(tsv.contains("0 | x\\|y | 2\n"), "{tsv}");
}

#[test]
fn compressed_input_is_decoded_and_binary_refused() {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(b"\xef\xbb\xbfid,v\n1,x\n").unwrap();
    let out = read("csv", &gz.finish().unwrap(), None).unwrap();
    assert!(out.contains("# | id | v\n"), "{out}");
    assert!(out.contains("0 | 1  | x\n"), "{out}");
    assert!(read("csv", b"a,b\n\0\0", None).is_err());
}

#[test]
fn names_with_compression_suffixes_are_recognised() {
    assert_eq!(format_of("data.CSV"), Some("csv"));
    assert_eq!(format_of("data.csv.gz"), Some("csv"));
    assert_eq!(format_of("data.tsv.zst"), Some("tsv"));
    assert_eq!(format_of("data.tab"), Some("tsv"));
    assert_eq!(format_of("data.json.gz"), None);
}
