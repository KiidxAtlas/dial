//! Read-only views of binary files: type identification from magic bytes, a paged hex dump
//! (`path:bytes=O+N`), and the on-disk structure of SQLite databases, write-ahead logs and
//! rollback journals. Nothing here opens a file through SQLite, so inspecting a damaged WAL or a
//! hot journal can never checkpoint, roll back or delete it.

use super::select::READ_FOOTER_RESERVE;
use crate::tools::{Workspace, io_message};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[cfg(test)]
mod tests;

/// Bytes sniffed to decide whether a file is binary and what it is.
const SNIFF: usize = 8 * 1024;
/// Bytes a whole-file read of a binary shows before the footer names the next range.
const DEFAULT_WINDOW: u64 = 512;
/// Rendered characters per 16-byte hex row (`00000000: ` + 8 groups + spacing + ASCII + newline).
const ROW_CHARS: usize = 68;
/// Largest SQLite WAL or journal whose frames are fully walked for a structure summary.
const MAX_STRUCTURE: u64 = 64 * 1024 * 1024;

const WAL_MAGIC_LE: u32 = 0x377f_0682;
const WAL_MAGIC_BE: u32 = 0x377f_0683;
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// What the leading bytes say a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools) struct Kind {
    /// Human description shown in the read header.
    pub name: &'static str,
    /// The rich reader (a `documents::kind` format) that can project it, if any.
    pub format: Option<&'static str>,
}

const fn kind(name: &'static str, format: Option<&'static str>) -> Option<Kind> {
    Some(Kind { name, format })
}

/// Identify a file from its first bytes. Only distinctive signatures are recognized, so ordinary
/// text never matches by accident.
pub(in crate::tools) fn identify(head: &[u8]) -> Option<Kind> {
    let starts = |magic: &[u8]| head.starts_with(magic);
    let be32 = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    if starts(SQLITE_MAGIC) {
        return kind("SQLite database", Some("sqlite"));
    }
    if matches!(be32(0), Some(WAL_MAGIC_LE | WAL_MAGIC_BE)) {
        return kind("SQLite write-ahead log (WAL)", None);
    }
    if starts(&JOURNAL_MAGIC) {
        return kind("SQLite rollback journal", None);
    }
    if starts(b"%PDF-") {
        return kind("PDF document", Some("pdf"));
    }
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        return kind(
            "ZIP archive (also DOCX/XLSX/JAR/APK containers)",
            Some("zip"),
        );
    }
    if starts(&[0x1f, 0x8b]) {
        return kind("gzip stream", Some("gzip"));
    }
    if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        return kind("xz stream", Some("xz"));
    }
    if starts(b"BZh") && head.get(3).is_some_and(|d| (b'1'..=b'9').contains(d)) {
        return kind("bzip2 stream", Some("bz2"));
    }
    if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return kind("Zstandard stream", Some("zst"));
    }
    if starts(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        return kind("7z archive", Some("7z"));
    }
    if starts(b"Rar!\x1a\x07") {
        return kind("RAR archive", Some("rar"));
    }
    if starts(b"MSCF\0\0\0\0") {
        return kind("Microsoft CAB archive", Some("cab"));
    }
    if head.get(257..262) == Some(b"ustar") {
        return kind("TAR archive", Some("tar"));
    }
    if starts(b"\x7fELF") {
        return kind("ELF executable or library", Some("object"));
    }
    if matches!(
        be32(0),
        Some(0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe)
    ) {
        return kind("Mach-O executable or library", Some("object"));
    }
    if starts(&[0xca, 0xfe, 0xba, 0xbe]) {
        // Fat Mach-O stores a small architecture count where a Java class stores its version.
        return if be32(4).is_some_and(|n| n > 0 && n < 45) {
            kind("Mach-O universal binary", Some("object"))
        } else {
            kind("Java class file", None)
        };
    }
    if starts(b"MZ") && head.len() >= 64 {
        return kind("Windows PE/DOS executable", Some("object"));
    }
    if starts(b"!<arch>\n") {
        return kind("ar archive (static library or .deb)", Some("object"));
    }
    if starts(b"\0asm") {
        return kind("WebAssembly module", None);
    }
    if starts(b"PAR1") {
        return kind("Apache Parquet file", Some("parquet"));
    }
    if starts(b"ARROW1") {
        return kind("Apache Arrow IPC file", Some("arrow"));
    }
    if starts(b"bplist00") {
        return kind("binary property list", Some("plist"));
    }
    if starts(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return kind(
            "OLE2 compound document (legacy .xls/.doc/.ppt/.msi)",
            Some("spreadsheet"),
        );
    }
    if starts(&[0x89, b'P', b'N', b'G']) {
        return kind("PNG image", None);
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return kind("JPEG image", None);
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return kind("GIF image", None);
    }
    if starts(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return kind("WebP image", None);
    }
    if starts(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        return kind("WAV audio", Some("wav"));
    }
    if starts(b"OggS") {
        return kind("Ogg media", None);
    }
    if starts(b"fLaC") {
        return kind("FLAC audio", None);
    }
    if starts(b"ID3") {
        return kind("MP3 audio (ID3)", None);
    }
    if head.get(4..8) == Some(b"ftyp") {
        return kind("ISO media (MP4/MOV/HEIC)", None);
    }
    if starts(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return kind("Matroska/WebM media", None);
    }
    if starts(&[0xd4, 0xc3, 0xb2, 0xa1]) || starts(&[0xa1, 0xb2, 0xc3, 0xd4]) {
        return kind("pcap packet capture", None);
    }
    if starts(&[0x0a, 0x0d, 0x0d, 0x0a]) {
        return kind("pcapng packet capture", None);
    }
    if starts(b"\x89HDF\r\n\x1a\n") {
        return kind("HDF5 file", None);
    }
    if starts(b"\x93NUMPY") {
        return kind("NumPy array (.npy)", Some("npy"));
    }
    if starts(b"GGUF") {
        return kind("GGUF model weights", None);
    }
    if starts(b"dex\n") {
        return kind("Android Dalvik executable", None);
    }
    if starts(&[0x00, 0x01, 0x00, 0x00, 0x00]) || starts(b"OTTO") || starts(b"ttcf") {
        return kind("TrueType/OpenType font", Some("font"));
    }
    if starts(b"wOFF") || starts(b"wOF2") {
        return kind("WOFF font", Some("font"));
    }
    if starts(&[0x30, 0x82]) && head.get(4..6) == Some(&[0x30, 0x82]) {
        return kind("DER-encoded ASN.1 (certificate or key)", Some("x509"));
    }
    if head.len() >= 16 && head.get(2..4) == Some(b"\r\n") && head[0] != 0 && head[1] < 0x10 {
        return kind("Python bytecode (.pyc)", None);
    }
    None
}

/// The first bytes of `path`, or `None` when it is not a readable regular file.
fn head(path: &Path) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = Vec::with_capacity(SNIFF);
    file.take(SNIFF as u64).read_to_end(&mut head).ok()?;
    Some(head)
}

/// Whether sniffed bytes are binary: a NUL byte, a recognized binary signature that is not valid
/// UTF-8 text, or mostly control bytes and invalid UTF-8. Latin-1 and other legacy text, whose
/// non-UTF-8 bytes are sparse, stays a text read.
fn is_binary(head: &[u8], kind: Option<Kind>) -> bool {
    if head.contains(&0) {
        return true;
    }
    let mut odd = 0usize;
    let mut rest = head;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                odd += control_bytes(text);
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                odd += control_bytes(std::str::from_utf8(valid).unwrap_or_default());
                // A sniff window may end inside a multi-byte character.
                let Some(bad) = e.error_len() else { break };
                odd += bad;
                rest = &after[bad..];
            }
        }
    }
    if kind.is_some() && odd > 0 {
        return true;
    }
    odd * 10 > head.len()
}

fn control_bytes(text: &str) -> usize {
    text.bytes()
        .filter(|b| b.is_ascii_control() && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b))
        .count()
}

/// SQLite sidecars are binary by definition, whatever their bytes look like: a damaged WAL can be
/// valid ASCII and still hold the newest committed rows.
fn sidecar_kind(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_str()?;
    let (base, kind) = [
        ("-wal", "SQLite write-ahead log (WAL) sidecar"),
        ("-journal", "SQLite rollback journal sidecar"),
        ("-shm", "SQLite shared-memory index sidecar"),
    ]
    .into_iter()
    .find_map(|(suffix, kind)| name.strip_suffix(suffix).map(|base| (base, kind)))?;
    let database = head(&path.with_file_name(base))?;
    database.starts_with(SQLITE_MAGIC).then_some(Kind {
        name: kind,
        format: None,
    })
}

/// A binary file found by sniffing a plain read target.
pub(in crate::tools) struct Binary {
    pub kind: Option<Kind>,
}

/// `Some` when `path_text` names a regular binary file (judged from its first 8 KiB, or a SQLite
/// sidecar name next to a database).
pub(in crate::tools) fn sniff(ws: &Workspace, path_text: &str) -> Option<Binary> {
    let path = ws.resolve_read(path_text).ok()?;
    let head = head(&path)?;
    if let Some(kind) = sidecar_kind(&path) {
        return Some(Binary { kind: Some(kind) });
    }
    let kind = identify(&head);
    is_binary(&head, kind).then_some(Binary { kind })
}

/// `path:bytes=O` or `path:bytes=O+N` on a local file. `None` when the suffix is absent, or the
/// whole input names an existing file literally.
pub(in crate::tools) fn split_bytes_selector<'a>(
    ws: &Workspace,
    input: &'a str,
) -> Option<(&'a str, &'a str)> {
    if ws.resolve_read(input).is_ok_and(|p| p.exists()) {
        return None;
    }
    let at = input.rfind(":bytes=")?;
    let base = &input[..at];
    ws.resolve_read(base)
        .is_ok_and(|p| p.is_file())
        .then(|| (base, &input[at + ":bytes=".len()..]))
}

fn parse_number(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("invalid byte offset or count {text:?}; use decimal or 0x hex"))
}

/// Parse `O`, `O+N` or `O-E` (inclusive end). `N` is `None` when only the offset is given.
pub(in crate::tools) fn parse_bytes_spec(spec: &str) -> Result<(u64, Option<u64>), String> {
    let usage = "byte selector must be `:bytes=O`, `:bytes=O+N` or `:bytes=O-E`";
    if spec.is_empty() {
        return Err(usage.into());
    }
    if let Some((offset, count)) = spec.split_once('+') {
        let count = parse_number(count)?;
        if count == 0 {
            return Err("`:bytes=O+N` needs N >= 1".into());
        }
        return Ok((parse_number(offset)?, Some(count)));
    }
    if let Some((offset, end)) = spec.split_once('-') {
        let (offset, end) = (parse_number(offset)?, parse_number(end)?);
        if end < offset {
            return Err(format!("byte range {spec:?} ends before it starts"));
        }
        return Ok((offset, Some(end - offset + 1)));
    }
    Ok((parse_number(spec)?, None))
}

/// Largest whole-row byte count whose hex rendering fits `budget` alongside a header.
fn fitting_bytes(budget: usize, header: usize) -> u64 {
    let rows = budget.saturating_sub(READ_FOOTER_RESERVE + header) / ROW_CHARS;
    (rows.max(1) * 16) as u64
}

/// xxd-style rows: offset, sixteen bytes in pairs, printable ASCII.
pub(in crate::tools) fn hex_dump(bytes: &[u8], start: u64) -> String {
    let mut out = String::with_capacity(bytes.len() / 16 * ROW_CHARS + ROW_CHARS);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}: ", start + row as u64 * 16));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x}")),
                None => out.push_str("  "),
            }
            if i % 2 == 1 {
                out.push(' ');
            }
        }
        out.push(' ');
        out.extend(chunk.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
        out.push('\n');
    }
    out
}

/// Read `count` bytes at `offset`, clamped to the file.
fn read_range(path: &Path, offset: u64, count: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(count).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Hex view of a byte range of any local file.
pub(in crate::tools) fn read_bytes(
    ws: &Workspace,
    path_text: &str,
    spec: &str,
    budget: usize,
) -> Result<String, String> {
    let (offset, count) = parse_bytes_spec(spec)?;
    let path = ws.resolve_read(path_text)?;
    let len = fs::metadata(&path)
        .map_err(|e| io_message("reading", path_text, &e))?
        .len();
    let kind = head(&path).and_then(|h| identify(&h));
    let header = header(path_text, len, kind);
    let fit = fitting_bytes(budget, header.len());
    let count = count.unwrap_or(fit).min(fit);
    if offset >= len && len > 0 {
        return Err(format!(
            "byte offset {offset} is past the end of {path_text} ({len} bytes)"
        ));
    }
    let bytes =
        read_range(&path, offset, count).map_err(|e| io_message("reading", path_text, &e))?;
    Ok(format!(
        "{header}{}{}",
        hex_dump(&bytes, offset),
        footer(path_text, offset, bytes.len() as u64, len)
    ))
}

fn header(path_text: &str, len: u64, kind: Option<Kind>) -> String {
    let what = kind.map_or("unrecognized binary data", |k| k.name);
    format!("[{path_text}: {len} bytes; {what}; read-only hex view]\n")
}

fn footer(path_text: &str, offset: u64, shown: u64, len: u64) -> String {
    let end = offset + shown;
    if end >= len {
        format!("[bytes {offset}..{end} of {len} shown; end of file]\n")
    } else {
        format!("[bytes {offset}..{end} of {len} shown; next: `{path_text}:bytes={end}+{shown}`]\n")
    }
}

impl Binary {
    /// Whole-file view: what the file is, its structure when known, then the first bytes.
    pub(in crate::tools) fn render(
        &self,
        ws: &Workspace,
        path_text: &str,
        budget: usize,
    ) -> Result<String, String> {
        let path = ws.resolve_read(path_text)?;
        let len = fs::metadata(&path)
            .map_err(|e| io_message("reading", path_text, &e))?
            .len();
        let mut out = header(path_text, len, self.kind);
        if let Some(structure) = structure(&path, len, self.kind) {
            out.push_str(&structure);
        }
        let fit = fitting_bytes(budget, out.len());
        let count = DEFAULT_WINDOW.min(fit);
        let bytes =
            read_range(&path, 0, count).map_err(|e| io_message("reading", path_text, &e))?;
        out.push_str(&hex_dump(&bytes, 0));
        out.push_str(&footer(path_text, 0, bytes.len() as u64, len));
        Ok(out)
    }
}

/// Structure summary for formats this module decodes itself. Sidecars named as such are decoded
/// even when their header is damaged, since that damage is the finding.
fn structure(path: &Path, len: u64, kind: Option<Kind>) -> Option<String> {
    let head = read_range(path, 0, 100).ok()?;
    if head.starts_with(SQLITE_MAGIC) {
        return Some(database_header(&head));
    }
    let name = kind.map_or("", |k| k.name);
    let is_wal = name.starts_with("SQLite write-ahead log");
    let is_journal = name.starts_with("SQLite rollback journal");
    if !is_wal && !is_journal {
        return None;
    }
    let bytes = read_range(path, 0, len.min(MAX_STRUCTURE)).ok()?;
    Some(if is_journal {
        journal(&bytes)
    } else {
        wal(&bytes)
    })
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The 100-byte SQLite database header.
pub(in crate::tools) fn database_header(head: &[u8]) -> String {
    if head.len() < 100 {
        return "SQLite header: truncated (under 100 bytes)\n".into();
    }
    let page_size = match be16(head, 16) {
        1 => 65_536,
        n => u32::from(n),
    };
    let mode = match (head[18], head[19]) {
        (2, 2) => "WAL",
        (1, 1) => "rollback journal",
        _ => "unknown",
    };
    let encoding = match be32(head, 56) {
        1 => "UTF-8",
        2 => "UTF-16le",
        3 => "UTF-16be",
        _ => "unset",
    };
    format!(
        "SQLite header: page size {page_size}; journal mode {mode} (format bytes {}/{}); {} pages; change counter {}; schema cookie {}; encoding {encoding}; user_version {}; application_id {}\n",
        head[18],
        head[19],
        be32(head, 28),
        be32(head, 24),
        be32(head, 40),
        be32(head, 60),
        be32(head, 68),
    )
}

/// SQLite's WAL checksum: pairs of 32-bit words accumulated into two running sums.
fn wal_checksum(data: &[u8], big_endian: bool, seed: (u32, u32)) -> (u32, u32) {
    let (mut s1, mut s2) = seed;
    for pair in data.as_chunks::<8>().0 {
        let word = |b: [u8; 4]| {
            if big_endian {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }
        };
        let [a, b, c, d, e, f, g, h] = *pair;
        s1 = s1.wrapping_add(word([a, b, c, d])).wrapping_add(s2);
        s2 = s2.wrapping_add(word([e, f, g, h])).wrapping_add(s1);
    }
    (s1, s2)
}

/// Structure of a WAL file, validating the header and the frame checksum chain the way SQLite's
/// recovery does. Frames after the first invalid one, and valid frames after the last commit, are
/// ignored by SQLite.
pub(in crate::tools) fn wal(bytes: &[u8]) -> String {
    wal_analysis(bytes).0
}

/// Whether a read-write SQLite open would discard data this WAL holds: an invalid header or
/// header checksum (every frame ignored), or frames past the first invalid frame or after the
/// last commit. An empty WAL holds nothing.
pub(in crate::tools) fn wal_discards_data(bytes: &[u8]) -> bool {
    !bytes.is_empty() && wal_analysis(bytes).1
}

/// [`wal`]'s report and whether SQLite would discard any of the file's data.
fn wal_analysis(bytes: &[u8]) -> (String, bool) {
    const NOTE: &str = "SQLite ignores an invalid WAL, and a read-write open (such as the plain `sqlite3` CLI) then truncates or deletes it with the data it holds. Copy it first, or open the database with `sqlite3 -readonly` or a `file:...?mode=ro` URI.";
    let len = bytes.len();
    if len < 32 {
        return (
            format!("SQLite WAL: {len} bytes, shorter than the 32-byte header. {NOTE}\n"),
            true,
        );
    }
    let magic = be32(bytes, 0);
    let page_size = be32(bytes, 8) as usize;
    if !matches!(magic, WAL_MAGIC_LE | WAL_MAGIC_BE) {
        let mut out = format!(
            "SQLite WAL: header NOT valid (magic {magic:08x}; expected 377f0682 or 377f0683). {NOTE}\n"
        );
        for size in [4096usize, 1024, 2048, 8192, 16_384, 32_768, 65_536, 512] {
            if len > 32 && (len - 32).is_multiple_of(size + 24) {
                out.push_str(&format!(
                    "Size fits {} frames of {size}-byte pages (32-byte header + 24-byte frame headers), so the layout may be intact and only the bytes altered.\n",
                    (len - 32) / (size + 24)
                ));
                break;
            }
        }
        return (out, true);
    }
    let big_endian = magic == WAL_MAGIC_BE;
    let stored = (be32(bytes, 24), be32(bytes, 28));
    let header_sum = wal_checksum(&bytes[..24], big_endian, (0, 0));
    let salts = (be32(bytes, 16), be32(bytes, 20));
    let mut out = format!(
        "SQLite WAL: magic {magic:08x} ({} checksums); format {}; page size {page_size}; checkpoint {}; salts {:08x}/{:08x}; header checksum {}\n",
        if big_endian {
            "big-endian"
        } else {
            "little-endian"
        },
        be32(bytes, 4),
        be32(bytes, 12),
        salts.0,
        salts.1,
        if header_sum == stored {
            "valid"
        } else {
            "INVALID"
        },
    );
    if header_sum != stored || !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two()
    {
        out.push_str(&format!(
            "The header does not validate, so SQLite treats every frame as absent. {NOTE}\n"
        ));
        return (out, true);
    }
    let frame = page_size + 24;
    let total = (len - 32) / frame;
    let trailing = (len - 32) % frame;
    let (mut sum, mut valid, mut committed, mut db_pages) = (stored, 0usize, 0usize, 0u32);
    let mut pages = std::collections::BTreeSet::new();
    for index in 0..total {
        let at = 32 + index * frame;
        let header = &bytes[at..at + 24];
        if (be32(header, 8), be32(header, 12)) != salts {
            break;
        }
        sum = wal_checksum(&header[..8], big_endian, sum);
        sum = wal_checksum(&bytes[at + 24..at + frame], big_endian, sum);
        if sum != (be32(header, 16), be32(header, 20)) {
            break;
        }
        valid += 1;
        pages.insert(be32(header, 0));
        if be32(header, 4) != 0 {
            committed = valid;
            db_pages = be32(header, 4);
        }
    }
    out.push_str(&format!(
        "Frames: {total} complete{}; {valid} pass the salt and checksum chain; {committed} committed",
        if trailing > 0 {
            format!(" (+{trailing} trailing bytes)")
        } else {
            String::new()
        }
    ));
    if committed > 0 {
        let shown: Vec<String> = pages.iter().take(32).map(u32::to_string).collect();
        out.push_str(&format!(
            " (database size {db_pages} pages after the last commit; pages written: {}{})",
            shown.join(","),
            if pages.len() > 32 { ",…" } else { "" }
        ));
    }
    out.push('\n');
    let discards = valid < total || committed < valid;
    if discards {
        out.push_str("SQLite discards frames after the first invalid frame and any uncommitted tail; a read-write open may delete them.\n");
    }
    (out, discards)
}

/// Structure of a rollback journal. A hot journal is replayed (the database is rolled back) by
/// the next read-write open.
pub(in crate::tools) fn journal(bytes: &[u8]) -> String {
    if bytes.len() < 28 || !bytes.starts_with(&JOURNAL_MAGIC) {
        let zeroed = bytes.iter().take(28).all(|&b| b == 0);
        return format!(
            "SQLite rollback journal: {} bytes; header {}\n",
            bytes.len(),
            if zeroed {
                "zeroed (finished transaction; not replayed)"
            } else {
                "not valid (not replayed)"
            }
        );
    }
    let records = be32(bytes, 8);
    format!(
        "SQLite rollback journal: HOT (valid header); {} page records; nonce {:08x}; original database size {} pages; sector size {}; page size {}. The next read-write open rolls the database back to its pre-transaction state using it; open read-only or copy both files first to inspect the uncommitted state.\n",
        if records == 0xffff_ffff {
            "unknown (until end of file)".to_owned()
        } else {
            records.to_string()
        },
        be32(bytes, 12),
        be32(bytes, 16),
        be32(bytes, 20),
        be32(bytes, 24),
    )
}

/// One status line per SQLite sidecar (`-wal`, `-journal`, `-shm`) next to `base`, so a database
/// read shows state SQLite itself would hide or destroy.
pub(in crate::tools) fn sqlite_sidecars(ws: &Workspace, base: &str) -> String {
    let mut out = String::new();
    for suffix in ["-wal", "-journal", "-shm"] {
        let name = format!("{base}{suffix}");
        let Ok(path) = ws.resolve_read(&name) else {
            continue;
        };
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let detail = match suffix {
            "-wal" if meta.len() == 0 => "empty".to_owned(),
            "-wal" => read_range(&path, 0, meta.len().min(MAX_STRUCTURE)).map_or_else(
                |e| format!("unreadable: {e}"),
                |b| wal(&b).trim_end().replace('\n', " "),
            ),
            "-journal" => read_range(&path, 0, 28).map_or_else(
                |e| format!("unreadable: {e}"),
                |b| journal(&b).trim_end().to_owned(),
            ),
            _ => "shared-memory index (rebuilt by SQLite; not data)".to_owned(),
        };
        out.push_str(&format!(
            "[sidecar {name}: {} bytes; {detail}]\n",
            meta.len()
        ));
    }
    if !out.is_empty() {
        out.push_str("[the rows below come from a read-only open, which uses only committed, valid WAL frames]\n");
    }
    out
}
