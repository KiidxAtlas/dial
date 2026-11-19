//! Windows names may address streams, devices, reparse points or DOS aliases. This admission
//! check is deliberately narrower than the file tools: uncertainty only costs a parallel slot.
use crate::platform::fs::{link_count, open_beneath};
use std::{io::ErrorKind, path::Path};

fn plain(name: &str) -> bool {
    // Limit scheduler case folding to ASCII and exclude the DOS short-name namespace.
    if !name.is_ascii()
        || name.contains(['~', ':', '*', '?', '"', '<', '>', '|'])
        || name.ends_with(['.', ' '])
        || name.bytes().any(|c| c.is_ascii_control())
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "AUX" | "CON" | "NUL" | "PRN" | "CONIN$" | "CONOUT$"
    ) && !(stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit())
}

pub(super) fn unambiguous(root: &Path, path: &Path, writing: bool) -> bool {
    // A second drive letter or network mount may address the primary root under another
    // spelling. Until such roots have a common identity, leave their operations serial.
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if !relative
        .components()
        .all(|part| part.as_os_str().to_str().is_some_and(plain))
    {
        return false;
    }
    let (existing, dir) = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() || (meta.is_dir() && !writing) => (path, meta.is_dir()),
        Err(e) if writing && e.kind() == ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return false;
            };
            (parent, true)
        }
        _ => return false,
    };
    // Require the on-disk long spelling as well as a no-reparse handle walk. For an absent
    // write check its *existing parent*, not the nonexistent leaf. The handle provides link
    // count and file type for the same object; inability to prove either stays a barrier.
    let Ok(relative) = existing.strip_prefix(root) else {
        return false;
    };
    std::fs::canonicalize(existing).is_ok_and(|p| p == existing)
        && open_beneath(root, relative, dir).is_ok_and(|file| {
            if dir {
                file.metadata().is_ok_and(|m| m.is_dir())
            } else {
                file.metadata().is_ok_and(|m| m.is_file()) && link_count(&file).ok() == Some(1)
            }
        })
}
