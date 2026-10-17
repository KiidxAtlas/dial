use super::*;

#[test]
fn utf16_offsets_count_surrogate_pairs_as_two() {
    let line = "a😀b";
    assert_eq!(utf16_offset(line, 0), 0);
    assert_eq!(utf16_offset(line, 1), 1);
    assert_eq!(utf16_offset(line, 2), 3);
    assert_eq!(utf16_offset(line, 3), 4);
    assert_eq!(utf16_offset(line, 99), 4);
}

#[test]
fn char_index_inverts_utf16_offset_and_clamps() {
    let line = "a😀b";
    assert_eq!(char_index(line, 0), 0);
    assert_eq!(char_index(line, 1), 1);
    assert_eq!(
        char_index(line, 2),
        1,
        "inside the surrogate pair maps to the emoji"
    );
    assert_eq!(char_index(line, 3), 2);
    assert_eq!(char_index(line, 4), 3);
    assert_eq!(char_index(line, 400), 3);
    assert_eq!(char_index("é", 1), 1, "BMP characters are one unit");
}

#[test]
fn locate_by_column_converts_to_utf16() {
    let text = "let s = \"😀\"; foo();\n";
    // `foo` is character index 13 (column 14) but UTF-16 offset 14: the emoji takes two units.
    let got = locate(text, 1, Some(14), None).unwrap();
    assert_eq!(
        got,
        Located {
            line: 0,
            character: 14,
            column: 14
        }
    );
    let got = locate(text, 1, None, Some("foo")).unwrap();
    assert_eq!(
        got,
        Located {
            line: 0,
            character: 14,
            column: 14
        }
    );
}

#[test]
fn locate_symbol_prefers_whole_words_and_honours_the_column_hint() {
    let text = "foobar foo foo\n";
    assert_eq!(
        locate(text, 1, None, Some("foo")).unwrap().column,
        8,
        "skips foo inside foobar"
    );
    assert_eq!(
        locate(text, 1, Some(9), Some("foo")).unwrap().column,
        12,
        "first at or after the hint"
    );
    assert_eq!(
        locate(text, 1, None, Some("oba")).unwrap().column,
        3,
        "falls back to a substring match"
    );
}

#[test]
fn locate_rejects_bad_positions_with_clear_messages() {
    let text = "ab\ncd";
    assert!(
        locate(text, 0, Some(1), None)
            .unwrap_err()
            .contains("out of range")
    );
    assert!(
        locate(text, 3, Some(1), None)
            .unwrap_err()
            .contains("2 lines")
    );
    assert!(
        locate(text, 1, Some(0), None)
            .unwrap_err()
            .contains("starts at 1")
    );
    assert!(
        locate(text, 1, Some(4), None)
            .unwrap_err()
            .contains("past the end")
    );
    assert!(
        locate(text, 1, Some(3), None).is_ok(),
        "one past the last character is a valid cursor"
    );
    assert!(
        locate(text, 1, None, Some("zz"))
            .unwrap_err()
            .contains("not found on line 1")
    );
    assert!(locate(text, 1, None, None).unwrap_err().contains("column"));
}

#[test]
fn split_lines_strips_carriage_returns() {
    assert_eq!(split_lines("a\r\nb\n"), ["a", "b", ""]);
    assert_eq!(split_lines("a\rb\r\nc"), ["a", "b", "c"]);
}

#[test]
fn uri_round_trips_awkward_paths() {
    for path in [
        "/tmp/a b/c#d.rs",
        "/tmp/ü/😀.rs",
        "/tmp/100%/x.rs",
        "/tmp/plain.rs",
    ] {
        let uri = uri_from_path(Path::new(path));
        assert!(uri.is_ascii() && uri.starts_with("file:///tmp/"), "{uri}");
        assert_eq!(path_from_uri(&uri), Some(PathBuf::from(path)), "{uri}");
    }
    assert_eq!(uri_from_path(Path::new("/tmp/a b")), "file:///tmp/a%20b");
}

#[test]
fn path_from_uri_rejects_non_file_and_malformed_uris() {
    assert_eq!(path_from_uri("https://example.com/a"), None);
    assert_eq!(path_from_uri("file://host/a"), None);
    assert_eq!(path_from_uri("file:///a%2"), None);
    assert_eq!(path_from_uri("file:///a%zz"), None);
    assert_eq!(path_from_uri("file:///a%+F"), None);
}

#[test]
fn read_source_rejects_non_utf8_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bin");
    fs::write(&bad, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(
        read_source(&bad).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        read_source(&dir.path().join("none")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}
