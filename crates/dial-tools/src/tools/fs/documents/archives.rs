//! Native archive and compressed-stream readers. Every container goes through one admission path
//! (`Walk`), so name safety, duplicate, entry-count, and declared-expansion limits apply the same way
//! whatever the format. Each format is read from a seekable source, either a file or an in-memory
//! download, so URL reads and local reads share one implementation.
use super::{
    MAX_CONTENT, Workspace, bounded,
    cache::{Identity, Opened},
    text,
};
use parking_lot::Mutex;
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

mod cab;
mod iso;
mod rar;
mod sevenz;
#[cfg(test)]
mod tests;

pub(super) const MAX_ENTRIES: usize = 10_000;
pub(super) const MAX_DECODED: u64 = 128 * 1024 * 1024;
const EXPANSION_MESSAGE: &str = "archive decoded stream exceeds 128 MiB";
const MAX_READERS: usize = 4;
const MAX_INDEX_ENTRIES: usize = 200_000;

struct CachedZip {
    path: PathBuf,
    identity: Identity,
    archive: zip::ZipArchive<File>,
}

static READERS: LazyLock<Mutex<VecDeque<CachedZip>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

pub(super) fn safe(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\0', '\\'])
        && !name.starts_with('/')
        && !name
            .split('/')
            .any(|part| part == ".." || part.contains(':'))
        && Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Reads a local archive or compressed stream. `format` is the name `documents::kind` reports.
pub(super) fn read(
    opened: &Opened,
    ws: &Workspace,
    format: &str,
    member: Option<&str>,
) -> Result<String, String> {
    check_member(member)?;
    if format == "zip" {
        return zip(opened, ws, member);
    }
    let file = opened.file.try_clone().map_err(|e| e.to_string())?;
    let text = read_source(format, &opened.path.to_string_lossy(), file, member)?;
    opened.validate(ws)?;
    Ok(text)
}

/// Reads an archive already held in memory (URL downloads). `name` supplies the file name, whose
/// compression suffix decides how a tar container is decoded.
pub(super) fn read_bytes(
    format: &str,
    name: &str,
    bytes: &[u8],
    member: Option<&str>,
) -> Result<String, String> {
    check_member(member)?;
    read_source(format, name, Cursor::new(bytes), member)
}

fn check_member(member: Option<&str>) -> Result<(), String> {
    if member.is_some_and(|s| !safe(s)) {
        return Err("archive member must be a relative safe path without .., backslashes, NUL, or drive prefixes".into());
    }
    Ok(())
}

fn read_source<R: Read + Seek>(
    format: &str,
    name: &str,
    mut source: R,
    member: Option<&str>,
) -> Result<String, String> {
    source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    match format {
        "zip" => {
            let mut archive =
                zip::ZipArchive::new(source).map_err(|e| format!("invalid ZIP archive: {e}"))?;
            validate_zip(&mut archive)?;
            zip_text(&mut archive, member)
        }
        "tar" => {
            let reader = decoder(tar_compression(name), source)?;
            tar(
                Expansion {
                    inner: reader,
                    remaining: MAX_DECODED,
                },
                member,
            )
        }
        "gzip" | "xz" | "bz2" | "zst" => {
            if member.is_some() {
                return Err("compressed streams have no member selector".into());
            }
            text(bounded(decoder(format, source)?)?)
        }
        "7z" => sevenz::read(source, member),
        "cab" => cab::read(source, member),
        "iso" => iso::read(source, member),
        "rar" => rar::read(source, member),
        other => Err(format!("unsupported archive format {other:?}")),
    }
}

/// Compression wrapping a tar container, chosen from the file name (`.tar.xz`, `.tbz2`, ...).
fn tar_compression(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "gzip"
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        "xz"
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz") || lower.ends_with(".tbz2") {
        "bz2"
    } else if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") {
        "zst"
    } else {
        "none"
    }
}

/// Wraps `source` in the named decoder. Every decoder is streaming except xz, whose pure-Rust
/// decoder writes into memory; that output is capped at the same expansion limit.
fn decoder<'a, R: Read + 'a>(compression: &str, source: R) -> Result<Box<dyn Read + 'a>, String> {
    Ok(match compression {
        "none" => Box::new(source),
        "gzip" => Box::new(flate2::read::MultiGzDecoder::new(source)),
        "bz2" => Box::new(bzip2_rs::DecoderReader::new(source)),
        "zst" => Box::new(
            ruzstd::decoding::StreamingDecoder::new(source)
                .map_err(|e| format!("invalid zstd stream: {e}"))?,
        ),
        "xz" => Box::new(Cursor::new(xz(source)?)),
        other => return Err(format!("unsupported compression {other:?}")),
    })
}

fn xz(source: impl Read) -> Result<Vec<u8>, String> {
    let mut out = Capped::default();
    let result = lzma_rs::xz_decompress(&mut BufReader::new(source), &mut out);
    if out.exceeded {
        return Err(EXPANSION_MESSAGE.into());
    }
    result.map_err(|e| format!("invalid xz stream: {e}"))?;
    Ok(out.bytes)
}

/// In-memory sink that refuses to grow past the expansion limit.
#[derive(Default)]
struct Capped {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len() as u64 + buf.len() as u64 > MAX_DECODED {
            self.exceeded = true;
            return Err(io::Error::other(EXPANSION_MESSAGE));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Admission and selection shared by every container: name safety, duplicates, entry count,
/// declared expansion, and listing size are checked before any member content is decoded.
pub(super) struct Walk<'a> {
    member: Option<&'a str>,
    names: HashSet<String>,
    count: usize,
    total: u64,
    listing: String,
    selected: Option<String>,
}

impl<'a> Walk<'a> {
    pub(super) fn new(member: Option<&'a str>) -> Self {
        Self {
            member,
            names: HashSet::new(),
            count: 0,
            total: 0,
            listing: String::new(),
            selected: None,
        }
    }

    /// Validates one entry. Returns true when it is the selected member, whose content the caller
    /// must then pass to `select`.
    pub(super) fn admit(&mut self, name: &str, size: u64) -> Result<bool, String> {
        self.count += 1;
        if self.count > MAX_ENTRIES {
            return Err("archive exceeds 10000 member limit".into());
        }
        if !safe(name) || !self.names.insert(name.to_owned()) {
            return Err(format!(
                "unsafe or duplicate archive member {name:?}; archive rejected"
            ));
        }
        self.total = self
            .total
            .checked_add(size)
            .ok_or("archive size overflow")?;
        if self.total > MAX_DECODED {
            return Err("archive expansion exceeds 128 MiB".into());
        }
        if self.member.is_none() {
            let row = format!("{name}\t{size} bytes\n");
            if self.listing.len() + row.len() > 1024 * 1024 {
                return Err("archive listing exceeds 1 MiB; select a member".into());
            }
            self.listing.push_str(&row);
        }
        Ok(self.member == Some(name))
    }

    pub(super) fn select(&mut self, content: String) {
        self.selected = Some(content);
    }

    pub(super) fn finish(self) -> Result<String, String> {
        match self.member {
            Some(name) => self
                .selected
                .ok_or_else(|| format!("archive member {name:?} not found")),
            None => Ok(self.listing),
        }
    }
}

/// Reads the selected member once admitted: it must be a regular file within the content cap.
pub(super) fn select_text(
    walk: &mut Walk<'_>,
    regular: bool,
    size: u64,
    data: impl Read,
) -> Result<(), String> {
    if !regular {
        return Err("selected archive member is not a regular file".into());
    }
    if size > MAX_CONTENT {
        return Err("archive member exceeds 8 MiB".into());
    }
    walk.select(text(bounded(data)?)?);
    Ok(())
}

fn validate_zip<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<(), String> {
    if archive.len() > MAX_ENTRIES {
        return Err("archive exceeds 10000 member limit".into());
    }
    let mut total = 0u64;
    let mut names = HashSet::new();
    // Validate the entire directory before returning any member, not just its prefix.
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("reading ZIP member: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("unreadable ZIP member name: {e}"))?;
        if !safe(&name) || !names.insert(name.to_string()) {
            return Err(format!(
                "unsafe or duplicate archive member {name:?}; archive rejected"
            ));
        }
        if entry
            .unix_mode()
            .is_some_and(|m| !matches!(m & 0o170000, 0 | 0o100000 | 0o040000))
        {
            return Err(format!(
                "nonregular archive member {name:?} is not supported (links/devices rejected)"
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or("archive size overflow")?;
        if total > MAX_DECODED {
            return Err("archive declared expansion exceeds 128 MiB".into());
        }
    }
    Ok(())
}

fn zip(opened: &Opened, ws: &Workspace, member: Option<&str>) -> Result<String, String> {
    let mut readers = READERS.lock();
    readers.retain(|r| r.path != opened.path || r.identity == opened.identity);
    let cached_index = readers
        .iter()
        .position(|r| r.path == opened.path && r.identity == opened.identity);
    let mut cached = if let Some(cached) = cached_index.and_then(|i| readers.remove(i)) {
        cached
    } else {
        // Evict before opening a fifth retained reader, including the reader in use.
        while readers.len() >= MAX_READERS {
            readers.pop_front();
        }
        let mut archive = zip::ZipArchive::new(opened.file.try_clone().map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid ZIP archive: {e}"))?;
        validate_zip(&mut archive)?;
        CachedZip {
            path: opened.path.clone(),
            identity: opened.identity,
            archive,
        }
    };
    // The retained FD pins its inode; matching the freshly opened dev/ino and
    // full revision cannot accidentally reuse an unlinked/replaced inode.
    opened.validate(ws)?;
    let result = zip_text(&mut cached.archive, member)?;
    opened.validate(ws)?;
    let count = cached.archive.len();
    while readers.len() >= MAX_READERS
        || readers.iter().map(|r| r.archive.len()).sum::<usize>() + count > MAX_INDEX_ENTRIES
    {
        if readers.pop_front().is_none() {
            break;
        }
    }
    readers.push_back(cached);
    Ok(result)
}

fn zip_text<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    member: Option<&str>,
) -> Result<String, String> {
    if let Some(name) = member {
        let entry = archive
            .by_name(name)
            .map_err(|e| format!("archive member {name:?}: {e}"))?;
        if !entry.is_file() {
            return Err("selected archive member is not a regular file".into());
        }
        if entry.size() > MAX_CONTENT {
            return Err("archive member exceeds 8 MiB".into());
        }
        return text(bounded(entry)?);
    }
    let mut out = String::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("reading ZIP member: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("unreadable ZIP member name: {e}"))?;
        let row = format!("{}\t{} bytes\n", name, entry.size());
        if out.len() + row.len() > 1024 * 1024 {
            return Err("archive listing exceeds 1 MiB; select a member".into());
        }
        out.push_str(&row);
    }
    Ok(out)
}

// Unlike Read::take, exceeding the container expansion limit is an error, never EOF.
struct Expansion<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for Expansion<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut byte = [0];
            if self.inner.read(&mut byte)? == 0 {
                return Ok(0);
            }
            return Err(io::Error::other(EXPANSION_MESSAGE));
        }
        let len = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..len])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn tar(reader: impl Read, member: Option<&str>) -> Result<String, String> {
    let mut archive = tar::Archive::new(reader);
    let mut walk = Walk::new(member);
    let entries = archive
        .entries()
        .map_err(|e| format!("invalid TAR archive: {e}"))?;
    for item in entries {
        let mut entry = item.map_err(|e| format!("reading TAR member: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .into_owned()
            .into_os_string()
            .into_string()
            .map_err(|_| "archive member path is not UTF-8")?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(format!(
                "nonregular archive member {name:?} is not supported (links/devices rejected)"
            ));
        }
        let size = entry.size();
        if walk.admit(&name, size)? {
            select_text(&mut walk, kind.is_file(), size, &mut entry)?;
        }
        // Consume every entry so truncated/bomb data after a selected member is refused.
        io::copy(&mut entry, &mut io::sink()).map_err(|e| format!("reading TAR content: {e}"))?;
    }
    // Drain trailers too: compressed streams must reach and validate their checksum.
    io::copy(&mut archive.into_inner(), &mut io::sink())
        .map_err(|e| format!("reading TAR trailer: {e}"))?;
    walk.finish()
}
