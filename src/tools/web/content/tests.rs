use super::*;

#[test]
fn classifies_by_media_type_then_by_content() {
    assert_eq!(classify(Some("text/html"), b""), Ok(Kind::Html));
    assert_eq!(
        classify(Some("application/vnd.api+json"), b""),
        Ok(Kind::Json)
    );
    assert_eq!(classify(Some("text/csv"), b""), Ok(Kind::Text));
    assert_eq!(classify(None, b"  <!DOCTYPE html><p>x"), Ok(Kind::Html));
    assert_eq!(
        classify(Some("application/octet-stream"), b"plain"),
        Ok(Kind::Text)
    );
    let image = classify(Some("image/png"), &[0x89, b'P', b'N', b'G']).unwrap_err();
    assert!(
        image.contains("image/png") && image.contains("4 bytes"),
        "{image}"
    );
    assert!(classify(None, &[1, 0, 2, 0]).is_err());
}

#[test]
fn decodes_declared_charsets() {
    assert_eq!(
        decode(&[b'c', b'a', b'f', 0xE9], Some("iso-8859-1"), Kind::Text),
        "café"
    );
    assert_eq!(
        decode(b"<meta charset=\"ISO-8859-1\"><p>\xE9", None, Kind::Html),
        "<meta charset=\"ISO-8859-1\"><p>é"
    );
    assert_eq!(decode("café".as_bytes(), None, Kind::Text), "café");
}

#[test]
fn truncation_marks_what_was_dropped() {
    assert_eq!(truncate_chars("héllo".into(), 5), "héllo");
    let cut = truncate_chars("héllo wörld".into(), 5);
    assert!(
        cut.starts_with("héllo\n[truncated: showing 5 of 11 characters"),
        "{cut}"
    );
}
