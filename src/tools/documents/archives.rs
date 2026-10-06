use super::{MAX_CONTENT, bounded, text};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path},
};
const MAX_ENTRIES: usize = 10_000;
const MAX_DECODED: u64 = 128 * 1024 * 1024;
fn safe(name: &str) -> bool {
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
pub(super) fn read(path: &Path, format: &str, member: Option<&str>) -> Result<String, String> {
    if member.is_some_and(|s| !safe(s)) {
        return Err("archive member must be a relative safe path without .., backslashes, NUL, or drive prefixes".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    match format {
        "zip" => zip(file, member),
        "tar" => {
            let gzip = path.to_string_lossy().to_ascii_lowercase().ends_with("gz");
            let reader: Box<dyn Read> = if gzip {
                Box::new(flate2::read::GzDecoder::new(file))
            } else {
                Box::new(file)
            };
            tar(reader.take(MAX_DECODED + 1), member)
        }
        "gzip" => {
            if member.is_some() {
                return Err("gzip streams have no member selector".into());
            }
            text(bounded(flate2::read::GzDecoder::new(file))?)
        }
        _ => unreachable!(),
    }
}
fn zip(file: File, member: Option<&str>) -> Result<String, String> {
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid ZIP archive: {e}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err("archive exceeds 10000 member limit".into());
    }
    let mut out = String::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("reading ZIP member: {e}"))?;
        let name = entry.name();
        if !safe(name) {
            return Err(format!("unsafe archive member {name:?}; archive rejected"));
        }
        let is_link = entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000);
        if is_link {
            return Err(format!("archive symlink {name:?} is not supported"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or("archive size overflow")?;
        if total > MAX_DECODED {
            return Err("archive declared expansion exceeds 128 MiB".into());
        }
        if member == Some(name) {
            if !entry.is_file() {
                return Err("selected archive member is not a regular file".into());
            }
            if entry.size() > MAX_CONTENT {
                return Err("archive member exceeds 8 MiB".into());
            }
            return text(bounded(entry)?);
        }
        if member.is_none() {
            if out.len() > 1024 * 1024 {
                return Err("archive listing exceeds 1 MiB; select a member".into());
            }
            out.push_str(&format!("{name}\t{} bytes\n", entry.size()));
        }
    }
    if let Some(name) = member {
        Err(format!("archive member {name:?} not found"))
    } else {
        Ok(out)
    }
}
fn tar(reader: impl Read, member: Option<&str>) -> Result<String, String> {
    let mut archive = tar::Archive::new(reader);
    let mut out = String::new();
    let mut total = 0u64;
    let entries = archive
        .entries()
        .map_err(|e| format!("invalid TAR archive: {e}"))?;
    for (i, item) in entries.enumerate() {
        if i >= MAX_ENTRIES {
            return Err("archive exceeds 10000 member limit".into());
        }
        let entry = item.map_err(|e| format!("reading TAR member: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        if !safe(&name) {
            return Err(format!("unsafe archive member {name:?}; archive rejected"));
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
            return text(bounded(entry)?);
        }
        if member.is_none() {
            if out.len() > 1024 * 1024 {
                return Err("archive listing exceeds 1 MiB; select a member".into());
            }
            out.push_str(&format!("{name}\t{} bytes\n", entry.size()));
        }
    }
    if let Some(name) = member {
        Err(format!("archive member {name:?} not found"))
    } else {
        Ok(out)
    }
}
