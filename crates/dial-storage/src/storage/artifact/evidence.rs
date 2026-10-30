//! Evidence manifests share the immutable artifact store and its lifecycle diagnostics.
use super::{ArtifactStore, artifact_id};
use anyhow::{Context, Result, ensure};
use dial_core::runtime::evidence::Evidence;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// A citation identifies this manifest, not the original source or an execution grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredEvidence {
    pub schema_version: u32,
    pub evidence: Evidence,
    pub representation_uri: String,
    pub captured_at_unix_ms: u64,
}

impl StoredEvidence {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported stored evidence schema version"
        );
        self.evidence.validate()?;
        let id = artifact_id(&self.representation_uri).map_err(anyhow::Error::msg)?;
        ensure!(
            id == self.evidence.representation.sha256,
            "evidence representation reference digest mismatch"
        );
        Ok(())
    }
}

impl ArtifactStore {
    /// Retain bytes without the truncation semantics of streaming tool output.
    pub fn retain_exact_bytes(&self, bytes: &[u8]) -> Result<String> {
        ensure!(
            self.limits.max_bytes == 0 || bytes.len() as u64 <= self.limits.max_bytes,
            "exact artifact exceeds configured spool maximum; refusing truncation"
        );
        let mut spool = self.spool()?;
        spool.write_all(bytes)?;
        let uri = spool.finish()?;
        ensure!(
            uri == format!("artifact://{:x}", Sha256::digest(bytes)),
            "exact artifact digest mismatch"
        );
        // A create-only spool may encounter an existing blob. Verify it rather than trusting its name.
        self.read_verified_evidence_blob(&uri, bytes.len())?;
        Ok(uri)
    }

    /// Store a validated representation and manifest; never fetch the source locator.
    pub fn retain_evidence(&self, evidence: &Evidence, bytes: &[u8]) -> Result<String> {
        evidence.validate()?;
        ensure!(
            format!("{:x}", Sha256::digest(bytes)) == evidence.representation.sha256,
            "evidence representation digest mismatch"
        );
        for parent in &evidence.context.parents {
            self.evidence_record(parent)
                .context("invalid evidence parent")?;
        }
        if let Some(snapshot) = &evidence.context.snapshot
            && let Some(uri) = &snapshot.artifact
        {
            self.verify_evidence_snapshot(uri, &snapshot.sha256, snapshot.bytes)?;
        }
        let record = StoredEvidence {
            schema_version: SCHEMA_VERSION,
            evidence: evidence.clone(),
            representation_uri: format!("artifact://{}", evidence.representation.sha256),
            captured_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
        };
        let manifest = serde_json::to_vec(&record)?;
        ensure!(
            manifest.len() <= MAX_MANIFEST_BYTES,
            "evidence manifest exceeds 1 MiB"
        );
        self.retain_exact_bytes(bytes)?;
        self.retain_exact_bytes(&manifest)
    }

    /// Read and authenticate a bounded manifest. Representations are read separately.
    pub fn evidence_record(&self, reference: &str) -> Result<StoredEvidence> {
        let bytes = self.read_verified_evidence_blob(reference, MAX_MANIFEST_BYTES)?;
        let record: StoredEvidence =
            serde_json::from_slice(&bytes).context("invalid stored evidence manifest")?;
        record.validate()?;
        // Discovery probes the canonical prefix without scanning arbitrary tool-output bodies.
        // Accept exactly the encoding retain_evidence emits so every readable manifest's
        // dependencies are also discoverable during pack, relocation and garbage collection.
        ensure!(
            serde_json::to_vec(&record)? == bytes,
            "stored evidence manifest is not canonical"
        );
        Ok(record)
    }

    /// Retrieve the complete authenticated representation or fail, never return a prefix.
    pub fn evidence_bytes(&self, record: &StoredEvidence, max_bytes: usize) -> Result<Vec<u8>> {
        record.validate()?;
        self.read_verified_evidence_blob(&record.representation_uri, max_bytes)
    }

    fn read_verified_evidence_blob(&self, uri: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let file = self.open(uri)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "artifact must be a regular file");
        ensure!(
            metadata.len() <= max_bytes as u64,
            "artifact exceeds evidence read limit"
        );
        let mut bytes = Vec::new();
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= max_bytes,
            "artifact exceeds evidence read limit"
        );
        let id = artifact_id(uri).map_err(anyhow::Error::msg)?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == id,
            "artifact content hash mismatch"
        );
        Ok(bytes)
    }

    fn verify_evidence_snapshot(&self, uri: &str, digest: &str, bytes: u64) -> Result<()> {
        let id = artifact_id(uri).map_err(anyhow::Error::msg)?;
        ensure!(id == digest, "source snapshot reference digest mismatch");
        let file = self.open(uri)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "artifact must be a regular file");
        ensure!(
            metadata.len() == bytes,
            "source snapshot byte length mismatch"
        );
        let mut reader = file.take(bytes.saturating_add(1));
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            hash.update(&buffer[..count]);
        }
        ensure!(total == bytes, "source snapshot byte length mismatch");
        ensure!(
            format!("{:x}", hash.finalize()) == digest,
            "source snapshot content hash mismatch"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
