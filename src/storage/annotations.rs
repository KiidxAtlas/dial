//! Human feedback records bound to immutable artifact identities. Reading cannot create feedback.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    pub schema_version: u32,
    pub artifact: String,
    pub sha256: String,
    pub start: Option<u64>,
    pub end: Option<u64>,
    pub note: String,
    pub created_at_unix_ms: u128,
    pub origin: String,
}
#[derive(Debug, Serialize)]
pub struct Page {
    pub schema_version: u32,
    pub artifact: String,
    pub annotations: Vec<Annotation>,
    pub total: usize,
    pub next_start: Option<usize>,
}
pub fn read(root: &Path, id: &str, start: usize, count: usize) -> Result<Page> {
    ensure!(
        start > 0 && (1..=500).contains(&count),
        "annotation start/count must be positive; count is capped at 500"
    );
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid annotation artifact identity"
    );
    let root = root.join(id);
    if !root.exists() {
        return Ok(Page {
            schema_version: 1,
            artifact: format!("artifact://{id}"),
            annotations: vec![],
            total: 0,
            next_start: None,
        });
    }
    let mut paths = std::fs::read_dir(&root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        paths.len() <= 4096,
        "annotation listing exceeds 4096 entries"
    );
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();
    let mut notes = Vec::new();
    let mut bytes_seen = 0;
    let mut next = (start - 1).min(paths.len());
    for path in paths.iter().skip(next).take(count) {
        if let Some(bytes) = crate::platform::atomic::read_private(path)? {
            ensure!(bytes.len() <= 20 * 1024, "annotation exceeds record limit");
            if bytes_seen + bytes.len() > 64 * 1024 {
                break;
            }
            bytes_seen += bytes.len();
            ensure!(
                path.file_stem().and_then(|stem| stem.to_str())
                    == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
                "annotation record no longer matches its immutable identity"
            );
            let note: Annotation = serde_json::from_slice(&bytes)?;
            ensure!(
                note.schema_version == 1
                    && note.sha256 == id
                    && note.artifact == format!("artifact://{id}")
                    && note.origin == "human_cli",
                "annotation version, origin or identity mismatch"
            );
            notes.push(note);
        }
        next += 1;
    }
    Ok(Page {
        schema_version: 1,
        artifact: format!("artifact://{id}"),
        annotations: notes,
        total: paths.len(),
        next_start: (next < paths.len()).then_some(next + 1),
    })
}
