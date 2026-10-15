use super::{
    MAX_CONTENT, Workspace, bounded,
    cache::{Identity, Opened},
    text,
};
use parking_lot::Mutex;
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};
const MAX_ENTRIES: usize = 10_000;
const MAX_DECODED: u64 = 128 * 1024 * 1024;
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
pub(super) fn read(
    opened: &Opened,
    ws: &Workspace,
    format: &str,
    member: Option<&str>,
) -> Result<String, String> {
    if member.is_some_and(|s| !safe(s)) {
        return Err("archive member must be a relative safe path without .., backslashes, NUL, or drive prefixes".into());
    }
    match format {
        "zip" => zip(opened, ws, member),
        "tar" => {
            let file = opened.file.try_clone().map_err(|e| e.to_string())?;
            let gzip = opened
                .path
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("gz");
            let reader: Box<dyn Read> = if gzip {
                Box::new(flate2::read::MultiGzDecoder::new(file))
            } else {
                Box::new(file)
            };
            tar(
                Expansion {
                    inner: reader,
                    remaining: MAX_DECODED,
                },
                member,
            )
        }
        "gzip" => {
            if member.is_some() {
                return Err("gzip streams have no member selector".into());
            }
            text(bounded(flate2::read::MultiGzDecoder::new(
                opened.file.try_clone().map_err(|e| e.to_string())?,
            ))?)
        }
        _ => unreachable!(),
    }
}
fn validate_zip(archive: &mut zip::ZipArchive<File>) -> Result<(), String> {
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
        let name = entry.name();
        if !safe(name) || !names.insert(name.to_owned()) {
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
    let mut cached = if let Some(i) = readers
        .iter()
        .position(|r| r.path == opened.path && r.identity == opened.identity)
    {
        readers.remove(i).unwrap()
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
fn zip_text(archive: &mut zip::ZipArchive<File>, member: Option<&str>) -> Result<String, String> {
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
        let row = format!("{}\t{} bytes\n", entry.name(), entry.size());
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
            return Err(io::Error::other("archive decoded stream exceeds 128 MiB"));
        }
        let len = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..len])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}
fn tar(reader: impl Read, member: Option<&str>) -> Result<String, String> {
    let mut archive = tar::Archive::new(reader);
    let mut out = String::new();
    let mut selected = None;
    let mut total = 0u64;
    let mut names = HashSet::new();
    let entries = archive
        .entries()
        .map_err(|e| format!("invalid TAR archive: {e}"))?;
    for (i, item) in entries.enumerate() {
        if i >= MAX_ENTRIES {
            return Err("archive exceeds 10000 member limit".into());
        }
        let mut entry = item.map_err(|e| format!("reading TAR member: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .into_owned()
            .into_os_string()
            .into_string()
            .map_err(|_| "archive member path is not UTF-8")?;
        if !safe(&name) || !names.insert(name.clone()) {
            return Err(format!(
                "unsafe or duplicate archive member {name:?}; archive rejected"
            ));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(format!(
                "nonregular archive member {name:?} is not supported (links/devices rejected)"
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or("archive size overflow")?;
        if total > MAX_DECODED {
            return Err("archive expansion exceeds 128 MiB".into());
        }
        if member == Some(name.as_str()) {
            if !kind.is_file() {
                return Err("selected archive member is not a regular file".into());
            }
            if entry.size() > MAX_CONTENT {
                return Err("archive member exceeds 8 MiB".into());
            }
            selected = Some(text(bounded(&mut entry)?)?);
        }
        if member.is_none() {
            let row = format!("{name}\t{} bytes\n", entry.size());
            if out.len() + row.len() > 1024 * 1024 {
                return Err("archive listing exceeds 1 MiB; select a member".into());
            }
            out.push_str(&row);
        }
        // Consume every entry so truncated/bomb data after a selected member is refused.
        io::copy(&mut entry, &mut io::sink()).map_err(|e| format!("reading TAR content: {e}"))?;
    }
    // Drain trailers too: compressed streams must reach and validate their checksum.
    io::copy(&mut archive.into_inner(), &mut io::sink())
        .map_err(|e| format!("reading TAR trailer: {e}"))?;
    if let Some(name) = member {
        selected.ok_or_else(|| format!("archive member {name:?} not found"))
    } else {
        Ok(out)
    }
}
