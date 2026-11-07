//! Initial-input receipts are separate from write checkpoints and ordinary undo coverage.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreservedInput {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub artifact: String,
}

impl Checkpoints {
    /// Retain already-authorized, bounded source bytes independently of the mutable original.
    /// This does not declare a workspace mutation or add the input to automatic rollback.
    pub fn preserve_input(
        &self,
        turn: u64,
        path: &Path,
        bytes: &[u8],
        artifacts: &crate::storage::artifact::ArtifactStore,
    ) -> Result<PreservedInput> {
        let (_, root, rel) = self.resolve(path)?;
        ensure!(
            root.is_none(),
            "automatic input preservation is limited to the project root"
        );
        ensure!(bytes.len() <= 1024 * 1024, "preserved input exceeds 1 MiB");
        let _guard = self.lock();
        let dir = turn_dir(&self.dir, turn);
        ensure!(
            dir.is_dir(),
            "input preservation needs an admitted conversation turn"
        );
        let manifest = dir.join("preserved-inputs.json");
        let mut records: Vec<PreservedInput> = match fs::read(&manifest) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if let Some(prior) = records.iter().find(|entry| entry.path == rel) {
            return Ok(prior.clone());
        }
        ensure!(
            records.len() < 64
                && records.iter().map(|entry| entry.bytes).sum::<u64>() + bytes.len() as u64
                    <= 4 * 1024 * 1024,
            "initial input preservation budget exhausted"
        );
        let record = PreservedInput {
            path: rel,
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            artifact: artifacts.retain_exact_bytes(bytes)?,
        };
        records.push(record.clone());
        atomic_write(&manifest, &serde_json::to_vec(&records)?, 0o600)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests;
