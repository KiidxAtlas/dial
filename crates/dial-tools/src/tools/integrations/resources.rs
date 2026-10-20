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
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Workspace,
    Remote,
    ImmutableArtifact,
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
                artifact_id(input)?;
                Ok(Scheme::Artifact)
            }
            _ => Err(format!(
                "unsupported resource scheme {prefix:?}; use a workspace path, ssh://, pr://, issue://, conflict:// or artifact://"
            )),
        };
    }
    Ok(Scheme::File)
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
