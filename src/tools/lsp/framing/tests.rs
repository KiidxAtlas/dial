use super::*;
use serde_json::json;
use std::io::{BufReader, Cursor};

/// Hands out at most `chunk` bytes per read, like a slow pipe.
struct Trickle<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.chunk.min(buf.len()).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn parse(bytes: &[u8]) -> io::Result<Option<Value>> {
    read_message(&mut Cursor::new(bytes.to_vec()))
}

#[test]
fn written_messages_read_back_and_count_bytes_not_chars() {
    let message = json!({"jsonrpc":"2.0","method":"x","params":{"s":"héllo 😀"}});
    let mut wire = Vec::new();
    write_message(&mut wire, &message).unwrap();
    let text = String::from_utf8(wire.clone()).unwrap();
    let body = serde_json::to_string(&message).unwrap();
    assert!(text.starts_with(&format!("Content-Length: {}\r\n\r\n", body.len())));
    assert!(body.len() > body.chars().count());
    assert_eq!(parse(&wire).unwrap(), Some(message));
}

#[test]
fn reads_consecutive_messages_then_reports_clean_eof() {
    let mut wire = frame(r#"{"id":1}"#);
    wire.extend(frame(r#"{"id":2}"#));
    let mut reader = Cursor::new(wire);
    assert_eq!(read_message(&mut reader).unwrap(), Some(json!({"id":1})));
    assert_eq!(read_message(&mut reader).unwrap(), Some(json!({"id":2})));
    assert_eq!(read_message(&mut reader).unwrap(), None);
}

#[test]
fn survives_one_byte_reads() {
    let mut wire = frame(r#"{"id":1,"v":"héllo"}"#);
    wire.extend(frame(r#"{"id":2}"#));
    let mut reader = BufReader::with_capacity(
        1,
        Trickle {
            data: &wire,
            chunk: 1,
        },
    );
    assert_eq!(
        read_message(&mut reader).unwrap(),
        Some(json!({"id":1,"v":"héllo"}))
    );
    assert_eq!(read_message(&mut reader).unwrap(), Some(json!({"id":2})));
    assert_eq!(read_message(&mut reader).unwrap(), None);
}

#[test]
fn ignores_other_headers_and_header_name_case() {
    let wire =
        b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
    assert_eq!(parse(wire).unwrap(), Some(json!({})));
}

#[test]
fn bad_headers_are_invalid_data() {
    for wire in [
        &b"Content-Type: x\r\n\r\n{}"[..],
        b"Content-Length: abc\r\n\r\n{}",
        b"Content-Length: -1\r\n\r\n{}",
        b"no colon here\r\n\r\n",
        b"Content-Length: 99999999999\r\n\r\n",
    ] {
        let error = parse(wire).unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::InvalidData,
            "{}",
            String::from_utf8_lossy(wire)
        );
    }
}

#[test]
fn oversized_header_line_and_header_flood_are_rejected() {
    let long = format!("X: {}\r\n\r\n", "a".repeat(MAX_HEADER_LINE * 2));
    assert_eq!(
        parse(long.as_bytes()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let flood = format!("{}\r\n", "A: b\r\n".repeat(MAX_HEADERS + 1));
    assert_eq!(
        parse(flood.as_bytes()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn truncation_is_unexpected_eof_and_garbage_body_is_invalid() {
    assert_eq!(
        parse(b"Content-Length: 10\r\n\r\n{}").unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        parse(b"Content-Length: 10\r\n").unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        parse(b"Content-Len").unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        parse(&frame("not json")).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
