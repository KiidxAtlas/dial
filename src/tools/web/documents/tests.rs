use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Serve `body` once per connection on a loopback port; returns the URL of `path`.
fn serve(path: &'static str, content_type: &'static str, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    std::thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://{address}{path}")
}

/// A one-page PDF whose text is `Hello from the PDF fixture`.
fn pdf_fixture() -> Vec<u8> {
    let content = "BT /F1 24 Tf 72 720 Td (Hello from the PDF fixture) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    for (index, object) in objects.iter().enumerate() {
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    pdf.push_str("trailer\n<< /Root 1 0 R /Size 6 >>\n%%EOF\n");
    pdf.into_bytes()
}

#[test]
fn kind_detection_uses_magic_media_type_and_extension() {
    assert_eq!(kind_of("https://x.test/a", None, b"%PDF-1.7"), Some("pdf"));
    assert_eq!(
        kind_of(
            "https://x.test/a",
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            b"PK"
        ),
        Some("docx")
    );
    assert_eq!(
        kind_of("https://x.test/files/Report.XLSX?v=2", None, b"PK"),
        Some("xlsx")
    );
    assert_eq!(
        kind_of("https://x.test/page.html", Some("text/html"), b"<html>"),
        None
    );
}

#[test]
#[ignore = "requires operator-installed Poppler pdftotext; external smoke enables this test"]
fn pdf_served_from_loopback_is_converted() {
    let url = serve("/fixture.pdf", "application/pdf", pdf_fixture());
    let client = Client::allowing_loopback();
    let response = client
        .send(&url, DOCUMENT_ACCEPT, &[], None)
        .expect("fetch fixture");
    let kind = kind_of(
        &response.url,
        response.media_type.as_deref(),
        &response.body,
    )
    .expect("PDF detected");
    let (_dir, ws) = crate::tools::testkit::workspace();
    let text = convert(&ws, &client, response, kind).expect("PDF converts");
    assert!(text.contains("Hello from the PDF fixture"), "got: {text}");
}
