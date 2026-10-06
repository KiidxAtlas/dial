//! Immutable, content-addressed output outside the editable workspace.
use super::ToolOutput;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}
pub struct ArtifactSpool {
    root: PathBuf,
    path: PathBuf,
    file: File,
    hash: Sha256,
}
impl ArtifactStore {
    pub fn new(root: &Path) -> Result<Self> {
        ensure!(
            !fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()),
            "artifact root cannot be a symlink"
        );
        fs::create_dir_all(root).context("creating artifact store")?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            root: root.canonicalize()?,
        })
    }
    pub fn spool(&self) -> Result<ArtifactSpool> {
        let path = self.root.join(format!(
            ".spool-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(ArtifactSpool {
            root: self.root.clone(),
            path,
            file,
            hash: Sha256::new(),
        })
    }
    pub fn retain(&self, text: &str) -> Result<String> {
        let mut spool = self.spool()?;
        spool.write_all(text.as_bytes())?;
        spool.finish()
    }
    pub fn image_reference(&self, uri: &str) -> Result<Value> {
        let path = self.path(uri)?;
        let id = path
            .file_name()
            .context("missing artifact id")?
            .to_string_lossy();
        Ok(json!({"root":self.root,"path":id,"sha256":id}))
    }
    fn path(&self, uri: &str) -> Result<PathBuf> {
        let id = uri
            .strip_prefix("artifact://")
            .context("expected artifact:// identifier")?;
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid artifact identifier"
        );
        Ok(self.root.join(id.to_ascii_lowercase()))
    }
    pub fn retrieve_bytes(&self, uri: &str, offset: u64, count: usize) -> Result<String> {
        use std::io::{Seek, SeekFrom};
        ensure!((1..=16384).contains(&count), "bytes must be 1..16384");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(uri)?)?;
        ensure!(
            file.metadata()?.is_file(),
            "artifact must be a regular file"
        );
        let total = file.metadata()?.len();
        ensure!(offset <= total, "offset exceeds artifact size");
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::with_capacity(count);
        file.take(count as u64).read_to_end(&mut bytes)?;
        use base64::Engine;
        Ok(json!({"uri":uri,"offset":offset,"next_offset":offset+bytes.len() as u64,"total_bytes":total,"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(bytes)}).to_string())
    }
    pub fn retrieve(&self, uri: &str, start: usize, count: usize) -> Result<String> {
        ensure!(
            start >= 1 && (1..=500).contains(&count),
            "start must be >=1; count must be 1..500"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(uri)?)
            .context("opening retained artifact")?;
        ensure!(
            file.metadata()?.is_file(),
            "artifact must be a regular file"
        );
        let mut reader = BufReader::new(file);
        let mut result = format!(
            "[{uri}, lines {start}..{}]\n",
            start.saturating_add(count - 1)
        );
        let mut line = Vec::new();
        let mut number = 0usize;
        loop {
            // Bound one malicious/huge line without loading it in its entirety.
            line.clear();
            let read = reader
                .by_ref()
                .take(24 * 1024 + 1)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            number += 1;
            if line.last() != Some(&b'\n') {
                let mut discarded = Vec::new();
                loop {
                    discarded.clear();
                    let n = reader
                        .by_ref()
                        .take(8192)
                        .read_until(b'\n', &mut discarded)?;
                    if n == 0 || discarded.last() == Some(&b'\n') {
                        break;
                    }
                }
            }
            if number < start {
                continue;
            }
            let text = String::from_utf8_lossy(&line);
            if result.len() + text.len() > 24 * 1024 {
                result.push_str("[page byte limit reached; request fewer lines]\n");
                break;
            }
            result.push_str(&format!("{number}:{}", text));
            if !text.ends_with('\n') {
                result.push_str(" [line exceeds page limit]\n");
            }
            if number >= start.saturating_add(count - 1) {
                break;
            }
        }
        Ok(result)
    }
    pub fn call(&self, args: &Value) -> ToolOutput {
        let result = if let Some(offset) = args["offset"].as_u64() {
            self.retrieve_bytes(
                args["uri"].as_str().unwrap_or(""),
                offset,
                args["bytes"].as_u64().unwrap_or(8192) as usize,
            )
        } else {
            self.retrieve(
                args["uri"].as_str().unwrap_or(""),
                args["start"].as_u64().unwrap_or(1) as usize,
                args["count"].as_u64().unwrap_or(100) as usize,
            )
        };
        match result {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
            },
            Err(e) => ToolOutput {
                text: format!("{e:#}"),
                is_error: true,
            },
        }
    }
}
impl Write for ArtifactSpool {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl ArtifactSpool {
    pub fn finish(mut self) -> Result<String> {
        self.file.flush()?;
        self.file.sync_all()?;
        let id = format!("{:x}", self.hash.clone().finalize());
        let destination = self.root.join(&id);
        // hard_link is create-only: existing content-addressed blobs are never rewritten.
        match fs::hard_link(&self.path, &destination) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    fs::symlink_metadata(&destination)?.file_type().is_file(),
                    "existing artifact is not a regular file"
                );
            }
            Err(e) => return Err(e.into()),
        }
        fs::remove_file(&self.path)?;
        Ok(format!("artifact://{id}"))
    }
}
impl Drop for ArtifactSpool {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub fn schema() -> Value {
    json!({"type":"function","function":{"name":"artifact_read","description":"Retrieve immutable complete output: 1-based ranged lines, or exact bytes with offset/bytes (base64, including binary). Byte paging recovers huge lines and images without loss.","parameters":{"type":"object","properties":{"uri":{"type":"string"},"start":{"type":"integer","minimum":1},"count":{"type":"integer","minimum":1,"maximum":500},"offset":{"type":"integer","minimum":0},"bytes":{"type":"integer","minimum":1,"maximum":16384}},"required":["uri"],"additionalProperties":false}}})
}
/// Convert a tool's encoded screenshot to compact immutable evidence before context clipping.
pub fn capture_image(
    store: Option<&ArtifactStore>,
    output: &mut ToolOutput,
) -> Result<Option<Value>> {
    let Ok(mut value) = serde_json::from_str::<Value>(&output.text) else {
        return Ok(None);
    };
    let Some(data) = value["artifact"]["data"].as_str() else {
        return Ok(None);
    };
    ensure!(
        value["artifact"]["encoding"] == "base64" && value["artifact"]["media_type"] == "image/png",
        "unsupported screenshot encoding"
    );
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decoding screenshot")?;
    ensure!(
        bytes.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES
            && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "screenshot must be PNG no larger than the image input limit"
    );
    let store = store.context("screenshot artifact storage is unavailable")?;
    let mut spool = store.spool()?;
    spool.write_all(&bytes)?;
    let uri = spool.finish()?;
    value["artifact"] = json!({"uri":uri,"media_type":"image/png","bytes":bytes.len()});
    output.text = value.to_string();
    Ok(Some(store.image_reference(&uri)?))
}

pub fn uris(text: &str) -> Vec<&str> {
    text.match_indices("artifact://")
        .filter_map(|(at, _)| {
            let uri = text.get(at..at + 75)?;
            uri[11..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then_some(uri)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranged_and_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let uri = store.retain("a\nb\nc\n").unwrap();
        assert_eq!(uri, store.retain("a\nb\nc\n").unwrap());
        assert!(store.retrieve(&uri, 2, 1).unwrap().contains("2:b\n"));
        assert!(store.retrieve("artifact://../x", 1, 1).is_err());
    }
    #[test]
    fn abandoned_spool_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let spool = store.spool().unwrap();
        let path = spool.path.clone();
        drop(spool);
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    #[test]
    fn rejects_symlink_store_and_symlink_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        assert!(ArtifactStore::new(&link).is_err());
        let store = ArtifactStore::new(dir.path()).unwrap();
        let uri = store.retain("evidence").unwrap();
        let blob = store.path(&uri).unwrap();
        fs::remove_file(&blob).unwrap();
        std::os::unix::fs::symlink(real.path().join("secret"), &blob).unwrap();
        assert!(store.retrieve(&uri, 1, 1).is_err());
        assert!(store.retrieve_bytes(&uri, 0, 100).is_err());
    }
    #[test]
    fn byte_pages_recover_large_unbroken_binary_without_loss() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let bytes: Vec<u8> = (0..100_000).map(|n| (n % 256) as u8).collect();
        let mut spool = store.spool().unwrap();
        spool.write_all(&bytes).unwrap();
        let uri = spool.finish().unwrap();
        let mut recovered = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() as u64 {
            let value: Value =
                serde_json::from_str(&store.retrieve_bytes(&uri, offset, 8192).unwrap()).unwrap();
            recovered.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(value["data"].as_str().unwrap())
                    .unwrap(),
            );
            offset = value["next_offset"].as_u64().unwrap();
        }
        assert_eq!(recovered, bytes);
    }
    #[test]
    fn screenshots_become_compact_image_evidence_not_clipped_base64() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path()).unwrap();
        let png = b"\x89PNG\r\n\x1a\nreal-image-bytes";
        let mut output = ToolOutput { text: json!({"artifact":{"media_type":"image/png","encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(png)}}).to_string(), is_error:false };
        let image = capture_image(Some(&store), &mut output).unwrap().unwrap();
        assert_eq!(
            fs::read(store.root.join(image["path"].as_str().unwrap())).unwrap(),
            png
        );
        assert!(output.text.contains("artifact://") && !output.text.contains("\"data\""));
    }
}
