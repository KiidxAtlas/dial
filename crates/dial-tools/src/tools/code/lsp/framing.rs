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
    let read = reader
        .take(MAX_HEADER_LINE as u64 + 1)
        .read_until(b'\n', &mut raw)?;
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
    String::from_utf8(raw)
        .map(Some)
        .map_err(|_| invalid("header is not valid UTF-8"))
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
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside the headers",
                ))
            };
        };
        if line.is_empty() {
            break;
        }
        if count >= MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid(format!("malformed header {line:?}")))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse()
                    .map_err(|_| invalid(format!("bad Content-Length {:?}", value.trim())))?,
            );
        }
    }
    let length = length.ok_or_else(|| invalid("missing Content-Length header"))?;
    if length > MAX_MESSAGE {
        return Err(invalid(format!(
            "message of {length} bytes exceeds the {MAX_MESSAGE} byte limit"
        )));
    }
    // Read through `take` so a lying length cannot force a huge up-front allocation.
    let mut body = Vec::new();
    reader.take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stream ended inside a message body",
        ));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| invalid(format!("body is not JSON: {e}")))
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
mod tests;
