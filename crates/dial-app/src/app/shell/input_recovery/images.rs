//! Digest-authenticated private image sidecars keep recovery JSON within its atomic 8 MiB cap.
use super::{Record, RecoverySnapshot};
use crate::platform::atomic;
use anyhow::{Context, Result, ensure};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

const RECORD_LIMIT: usize = 8 * 1024 * 1024;
const PREFIX: &str = "sha256:";

fn directory(path: &Path) -> Result<std::path::PathBuf> {
    Ok(path
        .parent()
        .context("recovery record has no parent")?
        .join(format!(
            "{}-images",
            path.file_stem()
                .context("recovery record has no name")?
                .to_string_lossy()
        )))
}

fn snapshots(record: &mut Record) -> impl Iterator<Item = &mut RecoverySnapshot> {
    record
        .pending
        .iter_mut()
        .chain(std::iter::once(&mut record.current))
}

pub(super) fn hydrate(record: &mut Record, path: &Path) -> Result<()> {
    let dir = directory(path)?;
    for snapshot in snapshots(record) {
        snapshot.map_image_payloads(|payload| {
            let Some(digest) = payload.strip_prefix(PREFIX) else {
                return Ok(payload.to_owned());
            };
            ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid clipboard recovery digest"
            );
            let bytes = atomic::read_private(&dir.join(digest))?
                .context("recovered clipboard image is missing; saved record kept unchanged")?;
            ensure!(
                bytes.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES,
                "recovered clipboard image exceeds the canonical image limit"
            );
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == digest,
                "recovered clipboard image failed its integrity check"
            );
            Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
        })?;
    }
    Ok(())
}

struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).filter(|size| *size <= RECORD_LIMIT)
            .ok_or_else(|| io::Error::other("Recovery records exceed 8 MiB; use /draft discard to discard saved records or reduce the current input. Nothing was overwritten."))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn prepare(record: &Record, path: &Path) -> Result<Vec<u8>> {
    let mut persisted = record.clone();
    let mut blobs = BTreeMap::new();
    for snapshot in snapshots(&mut persisted) {
        snapshot.map_image_payloads(|payload| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(payload)?;
            ensure!(
                bytes.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES,
                "clipboard recovery image exceeds the canonical image limit"
            );
            let digest = format!("{:x}", Sha256::digest(&bytes));
            blobs.entry(digest.clone()).or_insert(bytes);
            Ok(format!("{PREFIX}{digest}"))
        })?;
    }
    // Count exact serialized bytes first, without allocating an oversized JSON buffer or writing
    // sidecars for a record we already know cannot be atomically published.
    let mut count = Count(0);
    serde_json::to_writer(&mut count, &persisted)?;
    let mut bytes = Vec::with_capacity(count.0);
    serde_json::to_writer(&mut bytes, &persisted)?;
    if !blobs.is_empty() {
        let dir = directory(path)?;
        atomic::private_dir(&dir)?;
        for (digest, payload) in blobs {
            let target = dir.join(digest);
            match atomic::read_private(&target)? {
                Some(existing) => ensure!(
                    existing == payload,
                    "clipboard recovery sidecar failed its integrity check"
                ),
                None => atomic::write_private(&target, &payload)?,
            }
        }
    }
    Ok(bytes)
}
