use super::*;
use std::io::Write;

/// A minimal OpenDocument package whose `content.xml` holds `body`.
pub(in crate::tools::fs::documents) fn package(mimetype: &str, body: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut bytes));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("mimetype", options).unwrap();
        zip.write_all(mimetype.as_bytes()).unwrap();
        zip.start_file("META-INF/manifest.xml", options).unwrap();
        write!(
            zip,
            r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="{mimetype}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#
        )
        .unwrap();
        zip.start_file("content.xml", options).unwrap();
        write!(
            zip,
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" office:version="1.3"><office:body>{body}</office:body></office:document-content>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }
    bytes
}

#[test]
fn text_documents_keep_paragraph_text_and_spacing() {
    let odt = package(
        "application/vnd.oasis.opendocument.text",
        "<office:text><text:h>Title &amp; intro</text:h><text:p>one<text:s/>two<text:tab/>three<text:line-break/>four</text:p></office:text>",
    );
    assert_eq!(read(&odt).unwrap(), "Title & intro\none two\tthree\nfour\n");
}

#[test]
fn presentations_label_slides() {
    let odp = package(
        "application/vnd.oasis.opendocument.presentation",
        "<office:presentation><draw:page><text:p>first</text:p></draw:page><draw:page><text:p>second</text:p></draw:page></office:presentation>",
    );
    assert_eq!(
        read(&odp).unwrap(),
        "\n[slide 1]\nfirst\n\n[slide 2]\nsecond\n"
    );
    assert!(read(b"not a zip").is_err());
}
