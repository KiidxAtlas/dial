use super::*;

#[test]
fn resolves_references() {
    let base = "https://example.com/a/b/page.html?x=1#frag";
    for (reference, expected) in [
        ("https://other.org/z", "https://other.org/z"),
        ("//cdn.example.com/lib.js", "https://cdn.example.com/lib.js"),
        ("/root", "https://example.com/root"),
        ("sibling.html", "https://example.com/a/b/sibling.html"),
        ("../up.html", "https://example.com/a/up.html"),
        ("./here/", "https://example.com/a/b/here/"),
        ("../../../../far", "https://example.com/far"),
        ("?y=2", "https://example.com/a/b/page.html?y=2"),
        ("", "https://example.com/a/b/page.html?x=1"),
        ("next#sec", "https://example.com/a/b/next"),
        ("mailto:a@b.c", "mailto:a@b.c"),
    ] {
        assert_eq!(
            resolve_url(base, reference).as_deref(),
            Some(expected),
            "{reference}"
        );
    }
}

#[test]
fn rejects_unsafe_urls() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "gopher://x",
        "/relative",
        "http://user:pw@example.com/",
        "http:///x",
        "not a url",
    ] {
        assert!(parse_url(url).is_err(), "{url}");
    }
    assert!(parse_url("https://example.com/path?q=1").is_ok());
    assert!(parse_url(&format!("https://example.com/{}", "a".repeat(MAX_URL_LEN))).is_err());
}

#[test]
fn percent_round_trip() {
    assert_eq!(
        percent_encode("rust async/await & more"),
        "rust%20async%2Fawait%20%26%20more"
    );
    assert_eq!(percent_decode("a%20b+c%2Fd%zz%E2%9C%93"), "a b c/d%zz✓");
}

#[test]
fn content_type_parts() {
    assert_eq!(
        split_content_type(Some("Text/HTML; Charset=\"ISO-8859-1\"")),
        (Some("text/html".into()), Some("iso-8859-1".into()))
    );
    assert_eq!(split_content_type(None), (None, None));
}
