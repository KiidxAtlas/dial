//! Bounded, descriptor-based source snapshots; a citation never authorizes reopening its locator.
use crate::runtime::evidence::SourceSnapshot;
use crate::tools::Workspace;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_SOURCE: u64 = 64 * 1024 * 1024;

pub(crate) fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

/// Open only within currently authorized roots, checking deny-read policy before descriptor access.
pub(crate) fn read(ws: &Workspace, input: &str) -> Result<Vec<u8>> {
    read_bounded(ws, input, MAX_SOURCE)
}

/// Same source admission with a caller's smaller allocation bound.
pub(crate) fn read_bounded(ws: &Workspace, input: &str, limit: u64) -> Result<Vec<u8>> {
    let limit = limit.min(MAX_SOURCE);
    let bound = if limit == MAX_SOURCE {
        "64 MiB".to_owned()
    } else {
        format!("{limit} bytes")
    };
    let path = ws.resolve_read(input).map_err(anyhow::Error::msg)?;
    if let Some(
        crate::tools::exec::sandbox::Policy::Dev { deny_read, .. }
        | crate::tools::exec::sandbox::Policy::Strict { deny_read, .. },
    ) = ws.policy.as_ref().filter(|_| !ws.unsandboxed)
    {
        for denied in deny_read {
            let denied =
                crate::tools::exec::sandbox::resolve(denied).map_err(anyhow::Error::msg)?;
            ensure!(
                !path.starts_with(denied),
                "source denied by workspace read policy"
            );
        }
    }
    let (root, relative) = ws
        .read_roots()
        .filter_map(|root| path.strip_prefix(root).ok().map(|rel| (root, rel)))
        .max_by_key(|(root, _)| root.components().count())
        .context("source is outside authorized workspace roots")?;
    let mut file = crate::platform::fs::open_beneath(root, relative, false)
        .context("opening evidence source")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() <= limit,
        "source must be a regular file no larger than {bound}"
    );
    let identity = crate::platform::fs::identity(&file)?;
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "source grew beyond {bound}");
    let after = file.metadata()?;
    let reopened = crate::platform::fs::open_beneath(root, relative, false)?;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && identity == crate::platform::fs::identity(&reopened)?
            && ws.resolve_read(input).map_err(anyhow::Error::msg)? == path,
        "evidence source changed while capturing; retry the read"
    );
    Ok(bytes)
}

pub(crate) fn snapshot(bytes: &[u8]) -> Result<SourceSnapshot> {
    Ok(SourceSnapshot {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
        captured_at_unix_ms: now_ms()?,
        artifact: None,
    })
}
