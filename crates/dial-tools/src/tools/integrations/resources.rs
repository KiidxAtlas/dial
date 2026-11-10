//! Resource identity and routing policy. Parsing is pure; dispatch retains each owner's authority.
use crate::storage::artifact::artifact_id;
use serde::Serialize;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    File,
    Ssh,
    PullRequest,
    Issue,
    Conflict,
    Artifact,
    System,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Workspace,
    Remote,
    ImmutableArtifact,
    /// Live, read-only state of the machine Dial runs on.
    Host,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Descriptor {
    pub scheme: Scheme,
    pub prefix: &'static str,
    pub provenance: Provenance,
    pub accepts_line_arguments: bool,
    pub requires_network: bool,
}
pub const DESCRIPTORS: &[Descriptor] = &[
    Descriptor {
        scheme: Scheme::File,
        prefix: "",
        provenance: Provenance::Workspace,
        accepts_line_arguments: true,
        requires_network: false,
    },
    Descriptor {
        scheme: Scheme::Ssh,
        prefix: "ssh://",
        provenance: Provenance::Remote,
        accepts_line_arguments: false,
        requires_network: true,
    },
    Descriptor {
        scheme: Scheme::PullRequest,
        prefix: "pr://",
        provenance: Provenance::Remote,
        accepts_line_arguments: false,
        requires_network: true,
    },
    Descriptor {
        scheme: Scheme::Issue,
        prefix: "issue://",
        provenance: Provenance::Remote,
        accepts_line_arguments: false,
        requires_network: true,
    },
    Descriptor {
        scheme: Scheme::Conflict,
        prefix: "conflict://",
        provenance: Provenance::Workspace,
        accepts_line_arguments: false,
        requires_network: false,
    },
    Descriptor {
        scheme: Scheme::Artifact,
        prefix: "artifact://",
        provenance: Provenance::ImmutableArtifact,
        accepts_line_arguments: true,
        requires_network: false,
    },
    Descriptor {
        scheme: Scheme::System,
        prefix: "sys://",
        provenance: Provenance::Host,
        accepts_line_arguments: false,
        requires_network: false,
    },
];

pub fn descriptor(scheme: Scheme) -> &'static Descriptor {
    DESCRIPTORS
        .iter()
        .find(|entry| entry.scheme == scheme)
        .expect("registered resource scheme")
}
pub fn scheme(input: &str) -> Result<Scheme, String> {
    if input.is_empty() || input.contains('\0') {
        return Err("resource target is empty or contains NUL".into());
    }
    if let Some((prefix, _)) = input.split_once("://") {
        return match prefix {
            "file" => {
                path_from_uri(input).ok_or("invalid or non-local file URI")?;
                Ok(Scheme::File)
            }
            "ssh" => Ok(Scheme::Ssh),
            "pr" => Ok(Scheme::PullRequest),
            "issue" => Ok(Scheme::Issue),
            "conflict" => Ok(Scheme::Conflict),
            "artifact" => {
                artifact_target(input)?;
                Ok(Scheme::Artifact)
            }
            "sys" => Ok(Scheme::System),
            _ => Err(format!(
                "unsupported resource scheme {prefix:?}; use a workspace path, ssh://, pr://, issue://, conflict://, artifact:// or sys://"
            )),
        };
    }
    Ok(Scheme::File)
}

/// What `read` returns for an `artifact://` target, chosen by a path suffix: lines (`:A-B`, `:A+C`,
/// `:A-`, `:A`; default from line 1), `:info` (retention and availability), `:annotations` (human
/// feedback bound to the artifact) or `:bytes=O[+N]` (exact base64 bytes from offset O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactView {
    Lines { start: u64, count: Option<u64> },
    Info,
    Annotations,
    Bytes { offset: u64, count: u64 },
}

/// Split an `artifact://` read target into its canonical URI and the selected view.
pub fn artifact_target(input: &str) -> Result<(String, ArtifactView), String> {
    let rest = input
        .strip_prefix("artifact://")
        .ok_or("expected artifact:// identifier")?;
    let (id, selector) = match rest.split_once(':') {
        Some((id, selector)) => (id, Some(selector)),
        None => (rest, None),
    };
    let uri = format!("artifact://{id}");
    artifact_id(&uri)?;
    let number = |text: &str| -> Result<u64, String> {
        text.parse()
            .map_err(|_| format!("invalid artifact selector `:{}`", selector.unwrap_or("")))
    };
    let view = match selector {
        None => ArtifactView::Lines {
            start: 1,
            count: None,
        },
        Some("info") => ArtifactView::Info,
        Some("annotations") => ArtifactView::Annotations,
        Some(selector) => {
            if let Some(bytes) = selector.strip_prefix("bytes=") {
                let (offset, count) = match bytes.split_once('+') {
                    Some((offset, count)) => (number(offset)?, number(count)?),
                    None => (number(bytes)?, 8192),
                };
                ArtifactView::Bytes { offset, count }
            } else {
                let (start, count) = if let Some((start, count)) = selector.split_once('+') {
                    (number(start)?, Some(number(count)?))
                } else if let Some((start, end)) = selector.split_once('-') {
                    let start = number(start)?;
                    if end.is_empty() {
                        (start, None)
                    } else {
                        let end = number(end)?;
                        if end < start {
                            return Err("artifact line range end precedes its start".into());
                        }
                        (start, Some(end - start + 1))
                    }
                } else {
                    (number(selector)?, Some(1))
                };
                if start == 0 || count == Some(0) {
                    return Err("artifact lines are 1-based and a count must be at least 1".into());
                }
                ArtifactView::Lines { start, count }
            }
        }
    };
    Ok((uri, view))
}

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') || encoded.contains(['?', '#']) {
        return None;
    }
    let raw = encoded.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let digits = raw.get(i + 1..i + 3)?;
            if !digits.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let hex = std::str::from_utf8(digits).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    if bytes.contains(&0) {
        return None;
    }
    #[cfg(unix)]
    let path = PathBuf::from(OsString::from_vec(bytes));
    #[cfg(windows)]
    let path = {
        let text = String::from_utf8(bytes).ok()?;
        PathBuf::from(
            if text.as_bytes().get(2) == Some(&b':') && text.starts_with('/') {
                &text[1..]
            } else {
                &text
            },
        )
    };
    Some(path)
}

#[cfg(test)]
mod tests;
