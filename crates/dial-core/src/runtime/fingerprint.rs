//! Streaming file identity for immutable artifacts and reproducible build/run manifests.
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Identity of the exact bytes an observation engine consumed, rather than a later reread.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub path: std::path::PathBuf,
    pub sha256: String,
    pub bytes: usize,
}
impl Observation {
    pub fn text(path: &Path, text: &str) -> Self {
        Self {
            path: path.to_owned(),
            sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
            bytes: text.len(),
        }
    }
}
