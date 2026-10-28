//! Project-local artifact imports for session relocation. Never follows embedded paths or roots.
use super::*;

impl ArtifactStore {
    /// Path of the blob for `uri`, after checking the identifier shape. The blob is not read.
    pub(crate) fn blob_path(&self, uri: &str) -> Result<std::path::PathBuf> {
        artifact_id(uri).map_err(anyhow::Error::msg)?;
        self.path(uri)
    }

    /// Imports a staged blob whose SHA-256 must equal the id in `uri`. An existing blob with the same
    /// id is kept after its content is verified; a corrupt existing blob is an error, never replaced.
    pub(crate) fn import_blob(&self, uri: &str, staged: &std::path::Path) -> Result<()> {
        let id = artifact_id(uri).map_err(anyhow::Error::msg)?;
        ensure!(
            file_sha256(staged)? == id,
            "staged artifact {uri} has corrupt content"
        );
        let mut tmp = tempfile::NamedTempFile::new_in(&self.root)?;
        crate::platform::fs::set_owner_only(tmp.path())?;
        std::io::copy(&mut File::open(staged)?, tmp.as_file_mut())?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(self.path(uri)?) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    file_sha256(&self.path(uri)?)? == id,
                    "target artifact {uri} has corrupt content"
                );
            }
            Err(error) => return Err(error.error.into()),
        }
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    /// Import one referenced blob and its provenance without rewriting existing immutable records.
    /// A failed relocation may leave reusable imports, but never removes or mutates source data.
    pub(crate) fn transfer_to(&self, target: &Self, uri: &str) -> Result<()> {
        let id = artifact_id(uri).map_err(anyhow::Error::msg)?;
        let mut source = self.open(uri)?;
        crate::platform::atomic::validate_private_path(&self.root)?;
        crate::platform::atomic::check_file(&source)?;
        let mut staged = tempfile::NamedTempFile::new_in(&target.root)?;
        crate::platform::fs::set_owner_only(staged.path())?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            staged.write_all(&buffer[..n])?;
        }
        ensure!(
            format!("{:x}", hash.finalize()) == id,
            "referenced artifact {uri} has corrupt content"
        );
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(target.path(uri)?) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing = target.open(uri)?;
                crate::platform::atomic::check_file(&existing)?;
                let mut hash = Sha256::new();
                loop {
                    let n = existing.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buffer[..n]);
                }
                ensure!(
                    format!("{:x}", hash.finalize()) == id,
                    "target artifact {uri} has corrupt content"
                );
            }
            Err(error) => return Err(error.error.into()),
        }
        #[cfg(unix)]
        File::open(&target.root)?.sync_all()?;
        if let Some(mut record) = self.retention(uri)? {
            let path = target.lifecycle_path(uri)?;
            crate::platform::atomic::private_dir(path.parent().unwrap())?;
            let _lock = crate::config::settings_store::locked(&path.with_extension("lock"))?;
            if let Some(existing) = target.retention(uri)? {
                ensure!(
                    existing.bytes == record.bytes,
                    "artifact lifecycle byte count mismatch"
                );
                record.first_retained_at = record.first_retained_at.min(existing.first_retained_at);
                record.last_retained_at = record.last_retained_at.max(existing.last_retained_at);
            }
            crate::config::settings_store::write_private(&path, &serde_json::to_vec(&record)?)?;
        }
        let annotations = self
            .root
            .parent()
            .context("missing artifact parent")?
            .join("annotations");
        let source_dir = annotations.join(&id);
        crate::platform::atomic::validate_private_path(&source_dir)?;
        if !source_dir.exists() {
            return Ok(());
        }
        // Validate every feedback record using the same bounded reader as artifact_read.
        let mut start = 1;
        loop {
            let page = crate::storage::annotations::read(&annotations, &id, start, 500)?;
            match page.next_start {
                Some(next) => start = next,
                None => break,
            }
        }
        let destination = target
            .root
            .parent()
            .context("missing artifact parent")?
            .join("annotations")
            .join(&id);
        crate::platform::atomic::private_dir(&destination)?;
        for entry in fs::read_dir(&source_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let bytes = crate::platform::atomic::read_private(&path)?
                .context("feedback disappeared during move")?;
            let note: crate::storage::annotations::Annotation = serde_json::from_slice(&bytes)?;
            ensure!(
                bytes.len() <= 20 * 1024
                    && path.file_stem().and_then(|stem| stem.to_str())
                        == Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
                    && note.schema_version == 1
                    && note.sha256 == id
                    && note.artifact == format!("artifact://{id}")
                    && note.origin == "human_cli",
                "source feedback identity mismatch"
            );
            let dest = destination.join(path.file_name().context("missing feedback filename")?);
            let mut staged = tempfile::NamedTempFile::new_in(&destination)?;
            crate::platform::fs::set_owner_only(staged.path())?;
            staged.write_all(&bytes)?;
            staged.as_file().sync_all()?;
            match staged.persist_noclobber(&dest) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        crate::platform::atomic::read_private(&dest)?.as_deref()
                            == Some(bytes.as_slice()),
                        "target feedback identity collision"
                    );
                }
                Err(error) => return Err(error.error.into()),
            }
        }
        #[cfg(unix)]
        File::open(&destination)?.sync_all()?;
        Ok(())
    }
}

/// Classify references in parsed journal values, never in arbitrary serialized bytes. Explicitly
/// rooted image snapshots retain their original root and do not need a project-local import.
pub(crate) fn collect_references(
    value: &Value,
    source_root: &Path,
    found: &mut std::collections::HashSet<String>,
) {
    match value {
        Value::Object(object) => {
            if object.get("root").is_some_and(Value::is_string)
                && object.get("path").is_some_and(Value::is_string)
                && object.get("sha256").is_some_and(Value::is_string)
            {
                return;
            }
            for value in object.values() {
                collect_references(value, source_root, found);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_references(value, source_root, found);
            }
        }
        Value::String(text) => {
            // Structured tool results are sometimes stored as JSON text inside message content.
            if (text.starts_with('{') || text.starts_with('['))
                && let Ok(value) = serde_json::from_str::<Value>(text)
            {
                collect_references(&value, source_root, found);
                return;
            }
            if let Ok(id) = artifact_id(text) {
                found.insert(format!("artifact://{id}"));
                return;
            }
            for (at, _) in text.match_indices("artifact://") {
                let rest = &text[at..];
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != ':' && c != '/')
                    .unwrap_or(rest.len());
                let Ok(id) = artifact_id(&rest[..end]) else {
                    continue;
                };
                let prefix = &text[..at];
                // These are Dial's retained-output pointers, not arbitrary prose examples. Missing
                // blobs behind these pointers must fail explicitly. Other prose only carries a
                // dependency if the exact identifier is already present in this local store.
                let owned_pointer = [
                    "[complete output: ",
                    "[SESSION MESSAGE TRUNCATED: full content stored as artifact ",
                    "[Complete tool evidence: ",
                    "complete task report: ",
                    "bytes retained at ",
                ]
                .iter()
                .any(|marker| prefix.ends_with(marker));
                if owned_pointer || source_root.join(&id).is_file() {
                    found.insert(format!("artifact://{id}"));
                }
            }
        }
        _ => {}
    }
}

/// Streams a file through SHA-256 and returns the lowercase hex digest.
pub(crate) fn file_sha256(path: &std::path::Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
