//! Per-profile browser storage state (cookies and localStorage).
//!
//! Profiles live under the per-project state directory, never inside the
//! workspace, so they cannot be committed or read by sandboxed tools. Files are
//! written atomically; `tempfile` creates them with mode 0600.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Upper bound for a stored state file, so a corrupted profile cannot exhaust memory.
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

/// Profile names are a closed alphabet so they map to one file name safely.
pub(super) fn validate_profile_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err("browser profile names use 1-64 ASCII letters, digits, '_' or '-'".into())
    }
}

/// `<state>/browser/profiles/<name>.json`, after validating the name.
pub(super) fn profile_path(state_dir: &Path, name: &str) -> Result<PathBuf, String> {
    validate_profile_name(name)?;
    Ok(state_dir
        .join("browser")
        .join("profiles")
        .join(format!("{name}.json")))
}

/// Stored storage state, or `None` when the profile has never been saved.
pub(super) fn load(path: &Path) -> Result<Option<Value>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("stored browser profile exceeds 4 MiB".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("stored browser profile is corrupt: {error}"))
}

/// Atomically replace the profile file with `state`.
pub(super) fn save(path: &Path, state: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("browser profile path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(state).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("browser storage state exceeds 4 MiB; not saved".into());
    }
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    file.persist(path)
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_names_are_a_closed_alphabet() {
        assert!(validate_profile_name("work_1-b").is_ok());
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("../escape").is_err());
        assert!(validate_profile_name("a/b").is_err());
        assert!(validate_profile_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn profile_paths_stay_under_state_profiles_directory() {
        let path = profile_path(Path::new("/state"), "work").unwrap();
        assert_eq!(path, Path::new("/state/browser/profiles/work.json"));
        assert!(profile_path(Path::new("/state"), "../x").is_err());
    }

    #[test]
    fn save_then_load_round_trips_and_missing_profile_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = profile_path(dir.path(), "default").unwrap();
        assert_eq!(load(&path).unwrap(), None);
        let state = json!({"cookies": [{"name": "sid", "value": "1"}], "origins": []});
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), Some(state));
    }

    #[test]
    fn corrupt_profile_is_reported_not_silently_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
    }
}
