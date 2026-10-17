use super::*;
use crate::git_panel::parse_diff;

#[test]
fn whitespace_cycles_and_maps_to_git_args() {
    assert_eq!(Whitespace::Off.next(), Whitespace::IgnoreSpace);
    assert_eq!(Whitespace::IgnoreSpace.next(), Whitespace::Formatting);
    assert_eq!(Whitespace::Formatting.next(), Whitespace::Off);
    assert!(Whitespace::Off.git_args().is_empty());
    assert_eq!(Whitespace::IgnoreSpace.git_args(), &["-w"]);
    assert_eq!(
        Whitespace::Formatting.git_args(),
        &["-w", "--ignore-blank-lines"]
    );
    assert!(Whitespace::Off.allows_patch());
    assert!(!Whitespace::IgnoreSpace.allows_patch());
    assert!(!Whitespace::Formatting.allows_patch());
}

#[test]
fn wrap_splits_long_line_into_visual_rows() {
    assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    assert_eq!(wrap_text("", 4), vec![""]);

    let row = Row {
        hunk: None,
        kind: RowKind::Line {
            side: Side::Added,
            old: None,
            new: Some(1),
            text: "x".repeat(30),
        },
    };
    // A 32-column diff leaves 20 text columns (2 rows); a 20-column diff leaves 8 (4 rows).
    assert_eq!(segments(&row, 32).len(), 2);
    assert_eq!(segments(&row, 20).len(), 4);
}

#[test]
fn binary_doc_is_detected_from_fixture_diff() {
    let doc = parse_diff(
        "diff --git a/x.png b/x.png\nindex 1111111..2222222 100644\nBinary files a/x.png and b/x.png differ\n",
    );
    assert!(is_binary_doc(&doc));

    let text = parse_diff(
        "diff --git a/x.rs b/x.rs\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n",
    );
    assert!(!is_binary_doc(&text));
}

#[test]
fn binary_rows_falls_back_to_placeholder_text_without_native() {
    let binary = Binary {
        kind: "PNG image",
        size: 10,
        dims: Some((1, 1)),
        previews: None,
    };
    let text: Vec<String> = binary_rows(&binary)
        .into_iter()
        .filter_map(|row| match row.kind {
            RowKind::Meta(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text[0], " PNG image");
    assert_eq!(text[1], " 1 × 1 px");
    assert!(text.iter().any(|line| line.contains("No preview is shown")));
}

#[test]
fn sniff_reads_png_dimensions() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&640u32.to_be_bytes());
    png.extend_from_slice(&480u32.to_be_bytes());
    assert_eq!(sniff(&png), ("PNG image", Some((640, 480))));
    assert_eq!(sniff(b"\x00\x01"), ("Binary file", None));
}
