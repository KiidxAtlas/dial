//! ISO 9660 reader for the primary volume descriptor tree. Rock Ridge and Joliet extensions are not
//! interpreted: names come from the ISO 9660 records, and Rock Ridge symlinks appear as regular files.
use super::{MAX_ENTRIES, Walk, select_text};
use crate::tools::fs::documents::MAX_CONTENT;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
};

const SECTOR: u64 = 2048;
const MAX_DEPTH: usize = 32;
const MAX_DIR_BYTES: u64 = 4 * 1024 * 1024;
const FLAG_DIRECTORY: u8 = 0x02;
const FLAG_MULTI_EXTENT: u8 = 0x80;

#[derive(Clone, Copy)]
struct Extent {
    sector: u64,
    size: u64,
}

struct File {
    path: String,
    extent: Extent,
}

pub(super) fn read<R: Read + Seek>(mut source: R, member: Option<&str>) -> Result<String, String> {
    let root = root_extent(&mut source)?;
    let files = collect(&mut source, root)?;
    let mut walk = Walk::new(member);
    for file in &files {
        if walk.admit(&file.path, file.extent.size)? {
            if file.extent.size > MAX_CONTENT {
                return Err("archive member exceeds 8 MiB".into());
            }
            let data = read_extent(&mut source, file.extent, &file.path)?;
            select_text(&mut walk, true, file.extent.size, &data[..])?;
        }
    }
    walk.finish()
}

fn io_error(e: std::io::Error) -> String {
    format!("reading ISO image: {e}")
}

/// Reads exactly one extent. A short read means the image is truncated.
fn read_extent<R: Read + Seek>(
    source: &mut R,
    extent: Extent,
    path: &str,
) -> Result<Vec<u8>, String> {
    source
        .seek(SeekFrom::Start(extent.sector * SECTOR))
        .map_err(io_error)?;
    let mut data = Vec::new();
    (&mut *source)
        .take(extent.size)
        .read_to_end(&mut data)
        .map_err(io_error)?;
    if data.len() as u64 != extent.size {
        return Err(format!("ISO member {path:?} is truncated"));
    }
    Ok(data)
}

/// Scans the volume descriptor set for the primary volume descriptor (type 1). The terminator
/// (type 255) ends the search.
fn root_extent<R: Read + Seek>(source: &mut R) -> Result<Extent, String> {
    let mut block = [0u8; SECTOR as usize];
    for sector in 16..64u64 {
        source
            .seek(SeekFrom::Start(sector * SECTOR))
            .map_err(io_error)?;
        if source.read_exact(&mut block).is_err() {
            break;
        }
        if &block[1..6] != b"CD001" {
            continue;
        }
        match block[0] {
            1 => return Ok(record_extent(&block[156..190])),
            255 => break,
            _ => {}
        }
    }
    Err("ISO image has no primary volume descriptor".into())
}

fn le32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn record_extent(record: &[u8]) -> Extent {
    Extent {
        sector: le32(&record[2..6]),
        size: le32(&record[10..14]),
    }
}

/// Walks the directory tree breadth-first with an explicit stack. A visited-extent set stops
/// cycles and hard aliases; the depth cap bounds recursion through deeply nested trees.
fn collect<R: Read + Seek>(source: &mut R, root: Extent) -> Result<Vec<File>, String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(root.sector);
    let mut stack = vec![(root, String::new(), 0usize)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let data = read_dir(source, dir)?;
        let mut pos = 0usize;
        while pos < data.len() {
            let len = usize::from(data[pos]);
            if len == 0 {
                // Records never cross a sector boundary; zero padding skips to the next sector.
                pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            let record = data
                .get(pos..pos + len)
                .filter(|r| r.len() >= 33)
                .ok_or("ISO directory record is truncated")?;
            pos += len;
            let name_len = usize::from(record[32]);
            let raw = record
                .get(33..33 + name_len)
                .ok_or("ISO directory record is truncated")?;
            if name_len == 1 && (raw[0] == 0 || raw[0] == 1) {
                continue; // "." and ".." entries
            }
            let flags = record[25];
            let extent = record_extent(record);
            let name = clean_name(raw)?;
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if flags & FLAG_DIRECTORY != 0 {
                if depth + 1 > MAX_DEPTH {
                    return Err(format!("ISO directory tree exceeds {MAX_DEPTH} levels"));
                }
                if seen.insert(extent.sector) {
                    stack.push((extent, path, depth + 1));
                }
            } else {
                if flags & FLAG_MULTI_EXTENT != 0 {
                    return Err("multi-extent ISO members are not supported".into());
                }
                files.push(File { path, extent });
                if files.len() > MAX_ENTRIES {
                    return Err("archive exceeds 10000 member limit".into());
                }
            }
        }
    }
    Ok(files)
}

fn read_dir<R: Read + Seek>(source: &mut R, dir: Extent) -> Result<Vec<u8>, String> {
    if dir.size > MAX_DIR_BYTES {
        return Err("ISO directory exceeds 4 MiB".into());
    }
    let data = read_extent(source, dir, "<directory>")
        .map_err(|_| "ISO directory is truncated".to_owned())?;
    Ok(data)
}

/// Strips the `;version` suffix and the trailing dot ISO 9660 uses for extensionless names.
fn clean_name(raw: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "ISO member name is not UTF-8")?;
    let base = match text.rfind(';') {
        Some(i) if i + 1 < text.len() && text[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &text[..i]
        }
        _ => text,
    };
    let base = base.trim_end_matches('.');
    if base.is_empty() {
        return Err("ISO directory record has an empty name".into());
    }
    Ok(base.to_owned())
}
