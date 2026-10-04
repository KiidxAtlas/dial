//! `Content-Length` framing for JSON-RPC over a byte stream, as the Language Server Protocol
//! specifies: header lines ending in `\r\n`, a blank line, then exactly that many body bytes.

use serde_json::Value;
use std::io::{self, BufRead, Read, Write};

/// Longest single header line accepted.
const MAX_HEADER_LINE: usize = 4096;
/// Most header lines accepted before the blank separator.
const MAX_HEADERS: usize = 32;
/// Largest message body accepted (workspace-wide symbol or reference lists can be big).
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// One header line without its terminator; `None` on clean EOF before any byte.
fn read_header_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut raw = Vec::new();
    let read = reader.take(MAX_HEADER_LINE as u64 + 1).read_until(b'\n', &mut raw)?;
    if read == 0 {
        return Ok(None);
    }
    if raw.last() != Some(&b'\n') {
        return Err(if raw.len() > MAX_HEADER_LINE {
            invalid("header line too long")
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, "stream ended inside a header")
        });
    }
    raw.pop();
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    String::from_utf8(raw).map(Some).map_err(|_| invalid("header is not valid UTF-8"))
}

/// Read one message. `Ok(None)` means the peer closed the stream between messages; any
/// malformed or truncated frame is an error, after which the stream cannot be resynchronised.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    for count in 0.. {
        let Some(line) = read_header_line(reader)? else {
            return if count == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stream ended inside the headers"))
            };
        };
        if line.is_empty() {
            break;
        }
        if count >= MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| invalid(format!("malformed header {line:?}")))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse().map_err(|_| invalid(format!("bad Content-Length {:?}", value.trim())))?);
        }
    }
    let length = length.ok_or_else(|| invalid("missing Content-Length header"))?;
    if length > MAX_MESSAGE {
        return Err(invalid(format!("message of {length} bytes exceeds the {MAX_MESSAGE} byte limit")));
    }
    // Read through `take` so a lying length cannot force a huge up-front allocation.
    let mut body = Vec::new();
    reader.take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stream ended inside a message body"));
    }
    serde_json::from_slice(&body).map(Some).map_err(|e| invalid(format!("body is not JSON: {e}")))
}

/// Write one framed message as a single `write_all`, then flush.
pub fn write_message(writer: &mut (impl Write + ?Sized), message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
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
        let mut reader = BufReader::with_capacity(1, Trickle { data: &wire, chunk: 1 });
        assert_eq!(read_message(&mut reader).unwrap(), Some(json!({"id":1,"v":"héllo"})));
        assert_eq!(read_message(&mut reader).unwrap(), Some(json!({"id":2})));
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn ignores_other_headers_and_header_name_case() {
        let wire = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
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
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{}", String::from_utf8_lossy(wire));
        }
    }

    #[test]
    fn oversized_header_line_and_header_flood_are_rejected() {
        let long = format!("X: {}\r\n\r\n", "a".repeat(MAX_HEADER_LINE * 2));
        assert_eq!(parse(long.as_bytes()).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let flood = format!("{}\r\n", "A: b\r\n".repeat(MAX_HEADERS + 1));
        assert_eq!(parse(flood.as_bytes()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncation_is_unexpected_eof_and_garbage_body_is_invalid() {
        assert_eq!(parse(b"Content-Length: 10\r\n\r\n{}").unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(parse(b"Content-Length: 10\r\n").unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(parse(b"Content-Len").unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(parse(&frame("not json")).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
