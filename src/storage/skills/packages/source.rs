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
    ensure!(
        !url.is_empty(),
        "missing repository before '@' or '#' in {spec:?}"
    );
    ensure!(
        !url.starts_with('-') && !url.chars().any(char::is_whitespace),
        "invalid repository {url:?}"
    );
    let reference = match reference {
        Some(reference) => {
            ensure!(!reference.is_empty(), "empty ref after '@' in {spec:?}");
            ensure!(
                !reference.starts_with('-')
                    && !reference
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control()),
                "invalid ref {reference:?}"
            );
            Some(reference.to_owned())
        }
        None => None,
    };
    Ok(Source {
        url: url.to_owned(),
        reference,
        path,
    })
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
mod tests {
    use super::*;

    fn src(url: &str, reference: Option<&str>, path: Option<&str>) -> Source {
        Source {
            url: url.into(),
            reference: reference.map(Into::into),
            path: path.map(Into::into),
        }
    }

    #[test]
    fn parses_url_ref_and_path() {
        assert_eq!(
            parse("https://example.com/org/repo.git@v1.2#skills/pdf").unwrap(),
            src(
                "https://example.com/org/repo.git",
                Some("v1.2"),
                Some("skills/pdf")
            )
        );
        assert_eq!(
            parse("https://example.com/org/repo").unwrap(),
            src("https://example.com/org/repo", None, None)
        );
        assert_eq!(
            parse("https://example.com/org/repo#pdf").unwrap(),
            src("https://example.com/org/repo", None, Some("pdf"))
        );
    }

    #[test]
    fn keeps_scp_user_and_file_scheme_locations_intact() {
        assert_eq!(
            parse("git@github.com:owner/repo.git@main").unwrap(),
            src("git@github.com:owner/repo.git", Some("main"), None)
        );
        assert_eq!(
            parse("git@github.com:owner/repo.git").unwrap(),
            src("git@github.com:owner/repo.git", None, None)
        );
        assert_eq!(
            parse("file:///tmp/r@v1").unwrap(),
            src("file:///tmp/r", Some("v1"), None)
        );
        assert_eq!(
            parse("https://user:pw@host/r@v1").unwrap(),
            src("https://user:pw@host/r", Some("v1"), None)
        );
    }

    #[test]
    fn local_paths_split_refs_without_touching_the_path() {
        assert_eq!(
            parse("/tmp/repo@dev").unwrap(),
            src("/tmp/repo", Some("dev"), None)
        );
        assert_eq!(parse("./repo").unwrap(), src("./repo", None, None));
        assert_eq!(
            parse("../repo#skills").unwrap(),
            src("../repo", None, Some("skills"))
        );
    }

    #[test]
    fn rejects_unsafe_or_malformed_sources() {
        assert!(parse("").is_err());
        assert!(parse("@v1").is_err());
        assert!(parse("https://x/r@").is_err());
        assert!(parse("https://x/r#").is_err());
        assert!(parse("https://x/r#../secret").is_err());
        assert!(parse("https://x/r#/etc").is_err());
        assert!(parse("https://x/r#a\\b").is_err());
        assert!(parse("--upload-pack=evil").is_err());
        assert!(parse("https://x/r@--evil").is_err());
    }

    #[test]
    fn current_dir_components_are_allowed_in_paths() {
        assert!(validate_relative_path("./skills/pdf").is_ok());
        assert!(validate_relative_path("skills/../x").is_err());
    }

    #[test]
    fn derives_package_names() {
        assert_eq!(
            default_name(&src("https://x/org/repo.git", None, None)).as_deref(),
            Some("repo")
        );
        assert_eq!(
            default_name(&src("git@h:o/tools.git", None, None)).as_deref(),
            Some("tools")
        );
        assert_eq!(
            default_name(&src("https://x/r", None, Some("skills/pdf/"))).as_deref(),
            Some("pdf")
        );
        assert_eq!(default_name(&src("https://x/r", None, Some("."))), None);
    }

    #[test]
    fn package_name_rules() {
        assert!(valid_package_name("pdf-tools_2.0"));
        assert!(!valid_package_name(".hidden"));
        assert!(!valid_package_name("a b"));
        assert!(!valid_package_name(&"x".repeat(65)));
        assert!(!valid_package_name(""));
    }
}
