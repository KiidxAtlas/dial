//! Session-local compact path identities; expansion never grants filesystem authority.
use super::Workspace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Maximum live mappings; exhaustion refuses registration rather than reusing an identifier.
const MAX_HANDLES: usize = 4096;
/// Bound retained path bytes as well as mapping count.
const MAX_PATH_BYTES: usize = 16 * 1024;

/// Serializable session state. Treat restored paths as untrusted and validate through Workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileHandleSnapshot {
    root: PathBuf,
    first_id: u64,
    paths: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Registry {
    first_id: u64,
    paths: Vec<String>,
    by_path: BTreeMap<String, u64>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            first_id: 1,
            paths: Vec::new(),
            by_path: BTreeMap::new(),
        }
    }
}

impl Registry {
    pub(super) fn retired(&self) -> Self {
        Self {
            first_id: self.first_id + self.paths.len() as u64,
            ..Self::default()
        }
    }

    fn register(&mut self, path: String) -> Result<String, String> {
        if let Some(id) = self.by_path.get(&path) {
            return Ok(format!("@f{id}"));
        }
        if self.paths.len() >= MAX_HANDLES {
            return Err("file handle registry is full; use a literal path".into());
        }
        let id = self
            .first_id
            .checked_add(self.paths.len() as u64)
            .filter(|id| *id < u64::MAX)
            .ok_or("file handle identifiers exhausted")?;
        self.by_path.insert(path.clone(), id);
        self.paths.push(path);
        Ok(format!("@f{id}"))
    }

    fn lookup(&self, token: &str) -> Result<String, String> {
        // Leading zeroes are recognized but are never aliases for issued identifiers.
        let id = token.strip_prefix("@f").and_then(|s| s.parse::<u64>().ok());
        let index = id
            .and_then(|id| id.checked_sub(self.first_id))
            .and_then(|n| usize::try_from(n).ok());
        let path = index.and_then(|index| self.paths.get(index));
        match (id, path) {
            (Some(id), Some(path)) if token == format!("@f{id}") => Ok(path.clone()),
            _ => Err(format!(
                "unknown or retired file handle {token:?}; use a literal path or inspect again"
            )),
        }
    }
}

fn split_handle(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix("@f")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let end = 2 + digits;
    let suffix = &input[end..];
    if suffix.is_empty() || (suffix.starts_with(':') && !suffix.chars().any(char::is_whitespace)) {
        Some((&input[..end], suffix))
    } else {
        None
    }
}

pub(super) fn is_handle_with_selector(input: &str) -> bool {
    split_handle(input).is_some_and(|(_, suffix)| !suffix.is_empty())
}

impl Workspace {
    /// Validate and register a path, returning its stable session-local `@fN` identity.
    /// Existing handles are accepted; read selectors are not. No access is granted by registration.
    pub fn file_handle(&self, path: &str) -> Result<String, String> {
        let absolute = self.resolve(path)?;
        let normalized = self.handle_path(&absolute)?;
        self.file_handles
            .lock()
            .expect("file handle registry poisoned")
            .register(normalized)
    }

    /// Register a local read target using the native selector parser, including its preference
    /// for literal filenames containing colons. Raw output and unavailable targets stay literal.
    pub fn read_file_handle(&self, input: &str) -> Result<String, String> {
        let (path, selector) = super::fs::select::split_target(self, input)?;
        if selector.raw || !self.resolve(path)?.exists() {
            return Err("read target does not support compact references".into());
        }
        self.file_handle(path)
    }

    /// Look up a registered path without allocating a handle or changing session state.
    pub fn existing_file_handle(&self, path: &str) -> Option<String> {
        let absolute = self.resolve(path).ok()?;
        let normalized = self.handle_path(&absolute).ok()?;
        let registry = self
            .file_handles
            .lock()
            .expect("file handle registry poisoned");
        registry
            .by_path
            .get(&normalized)
            .map(|id| format!("@f{id}"))
    }

    fn handle_path(&self, absolute: &std::path::Path) -> Result<String, String> {
        let relative = absolute.strip_prefix(self.root()).unwrap_or(absolute);
        let path = if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative
                .to_str()
                .ok_or("file handles require a UTF-8 path")?
                .to_string()
        };
        let path = if split_handle(&path).is_some() {
            format!("./{path}")
        } else {
            path
        };
        if path.len() > MAX_PATH_BYTES {
            return Err("file handle path exceeds 16 KiB".into());
        }
        Ok(path)
    }

    /// Expand only an entire handle token, preserving an optional read-selector suffix.
    /// Literal paths pass through unchanged; recognized unknown handles always fail closed.
    /// This does not interpolate shell commands, prose, edit bodies or arbitrary strings.
    pub fn expand_file_handle(&self, input: &str) -> Result<String, String> {
        let Some((token, suffix)) = split_handle(input) else {
            return Ok(input.to_string());
        };
        let path = self
            .file_handles
            .lock()
            .expect("file handle registry poisoned")
            .lookup(token)?;
        // Validate current confinement too: roots may have detached and symlinks may have changed.
        self.resolve_literal(&path)?;
        Ok(format!("{path}{suffix}"))
    }

    /// Return the actual path for a bare handle, suitable for a user-facing `@fN(path)` label.
    pub fn file_handle_path(&self, handle: &str) -> Result<String, String> {
        match split_handle(handle) {
            Some((_, "")) => self.expand_file_handle(handle),
            _ => Err("expected a bare @fN file handle".into()),
        }
    }

    /// Start a distinct session scope, leaving existing clones bound to their prior mappings.
    /// Call only when switching sessions and discarding that session's handle-bearing context.
    /// Root changes within one session must use `reroot`, which retires old identifiers instead.
    pub fn reset_file_handles(&mut self) {
        self.file_handles = std::sync::Arc::default();
    }

    /// Snapshot mappings for persistence with session messages, never as filesystem authority.
    pub fn file_handle_snapshot(&self) -> FileHandleSnapshot {
        let registry = self
            .file_handles
            .lock()
            .expect("file handle registry poisoned");
        FileHandleSnapshot {
            root: self.root().to_path_buf(),
            first_id: registry.first_id,
            paths: registry.paths.clone(),
        }
    }

    /// Transactionally restore compatible mapping prefixes, retaining the longer registry.
    /// Conflicting mappings, a different root, invalid paths and oversized snapshots are refused.
    pub fn restore_file_handles(&self, snapshot: &FileHandleSnapshot) -> Result<(), String> {
        if snapshot.root != self.root() {
            return Err("file handle snapshot belongs to a different workspace root".into());
        }
        if snapshot.first_id == 0
            || snapshot.paths.len() > MAX_HANDLES
            || snapshot
                .first_id
                .checked_add(snapshot.paths.len() as u64)
                .is_none()
        {
            return Err("invalid or oversized file handle snapshot".into());
        }
        let mut by_path = BTreeMap::new();
        for (index, path) in snapshot.paths.iter().enumerate() {
            if path.len() > MAX_PATH_BYTES {
                return Err("file handle path exceeds 16 KiB".into());
            }
            // Restoration preserves identities, not grants. A symlink, attached root or
            // sensitive grant may have changed since recording; refuse it on use rather
            // than preventing the whole session from resuming.
            let literal_handle = path
                .strip_prefix("./")
                .is_some_and(|path| split_handle(path).is_some());
            let normalized: PathBuf = std::path::Path::new(path).components().collect();
            if path.is_empty()
                || path.contains('\0')
                || std::path::Path::new(path)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                || (!literal_handle && normalized.as_os_str() != std::ffi::OsStr::new(path))
                || (path != "."
                    && !literal_handle
                    && std::path::Path::new(path)
                        .components()
                        .any(|part| matches!(part, std::path::Component::CurDir)))
            {
                return Err("file handle snapshot path is not normalized".into());
            }
            if by_path
                .insert(path.clone(), snapshot.first_id + index as u64)
                .is_some()
            {
                return Err("file handle snapshot contains duplicate paths".into());
            }
        }
        let mut registry = self
            .file_handles
            .lock()
            .expect("file handle registry poisoned");
        // Only a pristine registry can adopt an alternate starting id (e.g. after a saved reroot).
        let pristine = registry.first_id == 1 && registry.paths.is_empty();
        if !pristine && registry.first_id != snapshot.first_id {
            return Err("file handle snapshot conflicts with retired identifiers".into());
        }
        if !registry
            .paths
            .iter()
            .zip(&snapshot.paths)
            .all(|(a, b)| a == b)
        {
            return Err("file handle snapshot conflicts with existing mappings".into());
        }
        if pristine || snapshot.paths.len() > registry.paths.len() {
            *registry = Registry {
                first_id: snapshot.first_id,
                paths: snapshot.paths.clone(),
                by_path,
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
