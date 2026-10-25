//! Skill package sources: `url[@ref][#path]`, package names and safe relative paths.

use anyhow::{Result, bail, ensure};
use std::path::{Component, Path};

/// Where a skill package comes from: a Git repository, an optional ref and an optional subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub url: String,
    pub reference: Option<String>,
    pub path: Option<String>,
}

impl Source {
    /// Validates structured sources too, including values read directly from a lock file.
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            !self.url.is_empty()
                && !self.url.starts_with('-')
                && !self.url.chars().any(char::is_control)
                && (!self.url.chars().any(char::is_whitespace)
                    || Path::new(&self.url).is_absolute()
                    || is_local_path(&self.url)),
            "invalid repository {:?}",
            self.url
        );
        if let Some(reference) = &self.reference {
            ensure!(
                !reference.is_empty()
                    && !reference.starts_with('-')
                    && !reference
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control()),
                "invalid ref {reference:?}"
            );
        }
        if let Some(path) = &self.path {
            validate_relative_path(path)?;
        }
        Ok(())
    }

    /// Canonical `url[@ref][#path]` spelling, as stored in the lock file and shown in lists.
    pub fn spec(&self) -> String {
        let mut out = self.url.clone();
        if let Some(reference) = &self.reference {
            out.push('@');
            out.push_str(reference);
        }
        if let Some(path) = &self.path {
            out.push('#');
            out.push_str(path);
        }
        out
    }
}

/// Parses `url[@ref][#path]`. The `#path` suffix is split first, so a fragment never reaches the URL.
pub fn parse(spec: &str) -> Result<Source> {
    let spec = spec.trim();
    ensure!(!spec.is_empty(), "empty skill package source");
    let (head, path) = match spec.split_once('#') {
        Some((head, path)) => {
            validate_relative_path(path)?;
            (head, Some(path.to_owned()))
        }
        None => (spec, None),
    };
    let (url, reference) = split_reference(head);
    let source = Source {
        url: url.to_owned(),
        reference: reference.map(str::to_owned),
        path,
    };
    source.validate()?;
    Ok(source)
}

/// Splits the last `@` that follows the repository location, so `user@host:repo` keeps its user part.
fn split_reference(head: &str) -> (&str, Option<&str>) {
    let start = reference_search_start(head);
    match head[start..].rfind('@') {
        Some(at) => (&head[..start + at], Some(&head[start + at + 1..])),
        None => (head, None),
    }
}

/// Byte offset where the repository location ends, so only text after it may hold a `@ref`.
fn reference_search_start(head: &str) -> usize {
    if let Some(scheme) = head.find("://") {
        let after = scheme + 3;
        return head[after..]
            .find('/')
            .map_or(head.len(), |slash| after + slash);
    }
    if is_local_path(head) {
        return 0;
    }
    // scp-like `user@host:owner/repo@ref`: the location ends at the first ':'.
    head.find(':').unwrap_or(0)
}

fn is_local_path(head: &str) -> bool {
    head.starts_with('/')
        || head.starts_with("./")
        || head.starts_with("../")
        || head.starts_with('~')
        || head.starts_with('\\')
        || head.as_bytes().get(1..3) == Some(b":\\")
}

/// Rejects anything but a relative path that stays inside the repository.
pub fn validate_relative_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty path after '#'");
    ensure!(
        !path.contains(['\\', '\0']),
        "unsafe path {path:?}: use '/' separators and no control characters"
    );
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!(
                "unsafe path {path:?}: use a relative path inside the repository, without '..'"
            ),
        }
    }
    Ok(())
}

/// Package and skill directory names: 1–64 ASCII letters, digits, `-`, `_` or `.`, not starting with `.`.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Package name derived from the `#path` basename, or from the repository basename without `.git`.
pub fn default_name(source: &Source) -> Option<String> {
    let raw = match &source.path {
        Some(path) => path.trim_end_matches('/').rsplit('/').next()?,
        None => {
            let last = source.url.trim_end_matches('/').rsplit(['/', ':']).next()?;
            last.strip_suffix(".git").unwrap_or(last)
        }
    };
    valid_package_name(raw).then(|| raw.to_owned())
}

#[cfg(test)]
mod tests;
