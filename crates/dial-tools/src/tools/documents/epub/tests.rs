use super::*;
use std::io::{Seek, Write};

const CONTAINER_XML: &str = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
/// Manifest order differs from spine order, so the tests prove that the spine sets reading order.
const OPF_XML: &str = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata/><manifest><item id="ch2" href="text/second.xhtml" media-type="application/xhtml+xml"/><item id="ch1" href="first.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="ch1"/><itemref idref="ch2"/></spine></package>"#;

fn chapter_xhtml(title: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><!DOCTYPE html><html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title><style>p {{ color: red; }}</style></head><body>{body}</body></html>"#
    )
}

fn package(files: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// A two-chapter EPUB whose chapter bodies are supplied by the caller.
fn book_with(first_body: &str, second_body: &str) -> Vec<u8> {
    let first = chapter_xhtml("ignored", first_body);
    let second = chapter_xhtml("Second", second_body);
    package(&[
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", CONTAINER_XML),
        ("OEBPS/content.opf", OPF_XML),
        ("OEBPS/first.xhtml", &first),
        ("OEBPS/text/second.xhtml", &second),
    ])
}

/// The fixture used by the tool-path tests: heading, markup, script/style and a list.
pub(in crate::tools::documents) fn sample_epub() -> Vec<u8> {
    book_with(
        "<h1>First &amp; Last</h1><p>Hello <em>world</em>.</p><script>evil()</script>",
        "<h2>Second chapter</h2><ul><li>alpha</li><li>beta</li></ul>",
    )
}

fn read_bytes(bytes: &[u8], selector: Option<&str>) -> Result<String, String> {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(bytes).unwrap();
    file.rewind().unwrap();
    read(file, selector)
}

#[test]
fn spine_order_drives_chapter_numbers_and_scripts_are_dropped() {
    let all = read_bytes(&sample_epub(), None).unwrap();
    let first = all
        .find("[chapter 1: First & Last]")
        .expect("chapter 1 header");
    let second = all
        .find("[chapter 2: Second chapter]")
        .expect("chapter 2 header");
    assert!(first < second, "{all}");
    assert!(all.contains("Hello world."), "{all}");
    assert!(all.contains("- alpha\n- beta"), "{all}");
    assert!(
        !all.contains("evil()") && !all.contains("color: red"),
        "{all}"
    );
    assert!(!all.contains("ignored"), "{all}");
}

#[test]
fn chapter_selector_returns_only_that_spine_position() {
    let second = read_bytes(&sample_epub(), Some("chapter=2")).unwrap();
    assert!(
        second.starts_with("[chapter 2: Second chapter]"),
        "{second}"
    );
    assert!(
        !second.contains("Hello") && !second.contains("[chapter 1"),
        "{second}"
    );
    let first = read_bytes(&sample_epub(), Some("chapter=1")).unwrap();
    assert!(first.starts_with("[chapter 1: First & Last]"), "{first}");
}

#[test]
fn out_of_range_and_malformed_selectors_are_actionable_errors() {
    let bytes = sample_epub();
    let past = read_bytes(&bytes, Some("chapter=3")).unwrap_err();
    assert!(past.contains("chapter index outside 1..=2"), "{past}");
    for bad in ["chapter=0", "chapter=x", "chapter="] {
        let error = read_bytes(&bytes, Some(bad)).unwrap_err();
        assert!(error.contains("invalid chapter index"), "{bad}: {error}");
    }
    let wrong = read_bytes(&bytes, Some("cell=1")).unwrap_err();
    assert!(wrong.contains("must be :chapter=N"), "{wrong}");
}

#[test]
fn missing_container_is_reported_clearly() {
    let bytes = package(&[("OEBPS/content.opf", OPF_XML)]);
    let error = read_bytes(&bytes, None).unwrap_err();
    assert!(error.contains("META-INF/container.xml"), "{error}");
}

#[test]
fn entity_heavy_chapter_decodes_html_and_numeric_entities() {
    let bytes = book_with(
        "<p>caf&eacute; &nbsp;&mdash; &#8212; &#x2014; &amp; &lt;tag&gt; &quot;q&quot; &hellip;</p>",
        "<p>plain</p>",
    );
    let all = read_bytes(&bytes, Some("chapter=1")).unwrap();
    assert!(
        all.lines().any(|line| line == "café — — — & <tag> \"q\" …"),
        "{all}"
    );
}

#[test]
fn unknown_named_entities_and_internal_dtds_are_rejected() {
    let error =
        read_bytes(&book_with("<p>&bogus;</p>", "<p>x</p>"), Some("chapter=1")).unwrap_err();
    assert!(error.contains("&bogus;"), "{error}");
    let dtd = convert(
        "<!DOCTYPE html [<!ENTITY x \"y\">]><html><body>x</body></html>",
        "t",
    )
    .unwrap_err();
    assert!(dtd.contains("DTDs"), "{dtd}");
}

#[test]
fn unsafe_member_names_and_escaping_hrefs_are_rejected() {
    let evil = package(&[
        ("META-INF/container.xml", CONTAINER_XML),
        ("OEBPS/content.opf", OPF_XML),
        ("../evil.xhtml", "x"),
    ]);
    let error = read_bytes(&evil, None).unwrap_err();
    assert!(error.contains("unsafe or duplicate"), "{error}");

    let escaping_opf = OPF_XML.replace("first.xhtml", "../../outside.xhtml");
    let escaping = package(&[
        ("META-INF/container.xml", CONTAINER_XML),
        ("OEBPS/content.opf", &escaping_opf),
    ]);
    let error = read_bytes(&escaping, None).unwrap_err();
    assert!(error.contains("escapes the package"), "{error}");
}

#[test]
fn package_paths_resolve_relative_hrefs_and_reject_the_rest() {
    assert_eq!(
        package_path("OEBPS", "text/a%20b.xhtml#frag").unwrap(),
        "OEBPS/text/a b.xhtml"
    );
    assert_eq!(
        package_path("OEBPS/text", "../ch.xhtml").unwrap(),
        "OEBPS/ch.xhtml"
    );
    assert_eq!(package_path("", "ch.xhtml").unwrap(), "ch.xhtml");
    assert!(package_path("OEBPS", "../../x.xhtml").is_err());
    assert!(package_path("OEBPS", "/abs.xhtml").is_err());
    assert!(package_path("OEBPS", "https://example.com/x.xhtml").is_err());
    assert!(package_path("OEBPS", "bad%zz.xhtml").is_err());
}

#[test]
fn entity_normalization_keeps_xml_entities_and_bare_ampersands() {
    assert_eq!(normalize_entities("a &amp; b").unwrap(), "a &amp; b");
    assert_eq!(normalize_entities("&nbsp;").unwrap(), "&#160;");
    assert_eq!(normalize_entities("x & y").unwrap(), "x & y");
    assert_eq!(normalize_entities("&#x2014;").unwrap(), "&#x2014;");
    assert!(normalize_entities("&nope;").is_err());
}

#[test]
fn heading_then_title_then_fallback_names_each_chapter() {
    let (title, body) = convert(
        "<html><head><title>Doc</title></head><body><p>x</p></body></html>",
        "fallback",
    )
    .unwrap();
    assert_eq!((title.as_str(), body.as_str()), ("Doc", "x"));
    let (title, _) = convert("<html><body><p>x</p></body></html>", "fallback").unwrap();
    assert_eq!(title, "fallback");
}
