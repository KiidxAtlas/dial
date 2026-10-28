//! RAR reader backed by the pure-Rust `rars` decoder. Encrypted and link members are refused only when
//! selected, so listings still show them; the expansion and header limits apply to the whole archive.
use super::{MAX_DECODED, MAX_ENTRIES, Walk, select_text};
use crate::tools::fs::documents::MAX_CONTENT;
use rars::{ArchiveReadOptions, ArchiveReader};
use std::io::{Read, Seek};

const MAX_RAR_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn read<R: Read + Seek>(mut source: R, member: Option<&str>) -> Result<String, String> {
    let mut bytes = Vec::new();
    (&mut source)
        .take(MAX_RAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading RAR archive: {e}"))?;
    if bytes.len() as u64 > MAX_RAR_BYTES {
        return Err("RAR archive exceeds 64 MiB; RAR reads are limited to 64 MiB".into());
    }
    let archive = ArchiveReader::read_owned_with_options(bytes, options())
        .map_err(|e| format!("invalid RAR archive: {e}"))?;
    let mut walk = Walk::new(member);
    for (index, item) in archive.member_refs().enumerate() {
        let owned = item.to_owned();
        let meta = &owned.meta;
        let name = std::str::from_utf8(&meta.name).map_err(|_| "RAR member name is not UTF-8")?;
        if meta.is_split_before || meta.is_split_after {
            return Err("multi-volume RAR archives are not supported".into());
        }
        let size = if meta.is_directory {
            0
        } else {
            meta.unpacked_size
        };
        if !walk.admit(name, size)? {
            continue;
        }
        if meta.is_directory || meta.is_redirection {
            return Err(format!(
                "selected archive member {name:?} is not a regular file (directories and links are rejected)"
            ));
        }
        if meta.is_encrypted {
            return Err(format!("encrypted RAR member {name:?} is not supported"));
        }
        if size > MAX_CONTENT {
            return Err("archive member exceeds 8 MiB".into());
        }
        let data = archive
            .read_member_at_with_options(index, options())
            .map_err(|e| format!("reading RAR member {name:?}: {e}"))?
            .ok_or("selected archive member is not a regular file")?;
        select_text(&mut walk, true, size, &data[..])?;
    }
    walk.finish()
}

fn options() -> ArchiveReadOptions<'static> {
    ArchiveReadOptions::new()
        .with_max_member_output_bytes(MAX_DECODED)
        .with_max_total_output_bytes(MAX_DECODED)
        .with_max_header_count(MAX_ENTRIES as u64)
}
