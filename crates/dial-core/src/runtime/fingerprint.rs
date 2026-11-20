//! Streaming file identity for immutable artifacts and reproducible build/run manifests.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

/// Streams `reader` through SHA-256 and returns the lowercase hex digest.
pub fn sha256_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Streams a file through SHA-256 and returns the lowercase hex digest. An open failure names the path.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_reader(&mut file)?)
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
