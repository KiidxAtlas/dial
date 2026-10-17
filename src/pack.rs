//! Portable, verified packs of learned artifacts: LoRA adapters and steering vectors.
//!
//! A pack carries weights and provenance only. Importing never activates anything: an adapter
//! enters as a pending candidate that must pass the local gate, and a steering vector is re-bound
//! to this checkout with its evaluation cleared, so it cannot steer until local evaluation passes.
use crate::{
    config::types::{Config, Version},
    hex, steering,
    storage::store::Store,
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const FORMAT: &str = "dial-pack";
const VERSION: u32 = 1;
const MAX_PACK_BYTES: u64 = 8 << 30;
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_JSON_HASH_BYTES: u64 = 16 << 20;
const ADAPTER_FILES: [&str; 2] = ["adapter_config.json", "adapters.safetensors"];
const STEERING_FILE: &str = "vector.json";

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    /// "lora" or "steering".
    kind: String,
    /// Portable fingerprint of the base checkpoint the artifact was produced against.
    base: String,
    /// Exporter provenance. Informational only; the importer re-derives everything it trusts.
    source: Value,
    files: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    name: String,
    sha256: String,
    bytes: u64,
}

fn file_sha(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

/// Portable base-checkpoint fingerprint: config and tokenizer contents plus weight file names
/// and sizes. Unlike the worker's machine-local identity (absolute path and mtime), this is equal
/// across machines holding the same checkpoint.
pub fn base_fingerprint(model: &Path) -> Result<String> {
    ensure!(
        model.is_dir(),
        "local model checkpoint is not pinned; run `dial improve doctor` first"
    );
    let mut paths = Vec::new();
    for entry in fs::read_dir(model)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(".safetensors")
            || matches!(
                name,
                "config.json" | "tokenizer_config.json" | "model.safetensors.index.json"
            )
        {
            paths.push(path);
        }
    }
    ensure!(!paths.is_empty(), "checkpoint has no weight files");
    paths.sort();
    let mut hasher = Sha256::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let meta = fs::metadata(&path)?;
        ensure!(
            meta.is_file(),
            "checkpoint entry {name} is not a regular file"
        );
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(meta.len().to_le_bytes());
        if !name.ends_with(".safetensors") && meta.len() <= MAX_JSON_HASH_BYTES {
            hasher.update(file_sha(&path)?.as_bytes());
        }
    }
    Ok(hex(&hasher.finalize()))
}

fn entries(sources: &[(String, PathBuf)]) -> Result<Vec<Entry>> {
    sources
        .iter()
        .map(|(name, path)| {
            ensure!(path.is_file(), "pack source {name} is missing");
            Ok(Entry {
                name: name.clone(),
                sha256: file_sha(path)?,
                bytes: fs::metadata(path)?.len(),
            })
        })
        .collect()
}

fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    tar.append_data(&mut header, name, bytes)?;
    Ok(())
}

fn write_pack(out: &Path, manifest: &Manifest, sources: &[(String, PathBuf)]) -> Result<()> {
    let partial = out.with_extension("partial");
    let encoder = GzEncoder::new(fs::File::create(&partial)?, Compression::default());
    let mut tar = tar::Builder::new(encoder);
    append_bytes(
        &mut tar,
        "manifest.json",
        &serde_json::to_vec_pretty(manifest)?,
    )?;
    for (name, path) in sources {
        tar.append_path_with_name(path, format!("files/{name}"))?;
    }
    tar.into_inner()?.finish()?.sync_all()?;
    fs::rename(&partial, out)?;
    Ok(())
}

fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.starts_with('.') && !name.contains(['/', '\\'])
}

/// Read and verify a pack. Members are extracted into a fresh directory under `root`; only
/// regular `files/<plain name>` members and one `manifest.json` are accepted.
fn open_pack(
    pack: &Path,
    root: &Path,
    kind: &str,
    base: &str,
    max_bytes: u64,
) -> Result<(Manifest, tempfile::TempDir)> {
    ensure!(
        fs::metadata(pack)?.len() <= max_bytes,
        "pack exceeds the size limit"
    );
    fs::create_dir_all(root)?;
    let dir = tempfile::Builder::new().prefix("pack-").tempdir_in(root)?;
    let mut archive = tar::Archive::new(GzDecoder::new(fs::File::open(pack)?));
    let mut total: u64 = 0;
    let mut manifest: Option<Manifest> = None;
    let mut members: Vec<String> = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        ensure!(
            entry.header().entry_type().is_file(),
            "pack contains a non-regular member"
        );
        let name = entry.path()?.to_string_lossy().into_owned();
        let size = entry.header().size()?;
        total = total.checked_add(size).context("pack size overflows")?;
        ensure!(total <= max_bytes, "pack exceeds the size limit");
        if name == "manifest.json" {
            ensure!(manifest.is_none(), "pack has more than one manifest");
            ensure!(size <= MAX_MANIFEST_BYTES, "pack manifest is too large");
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            manifest = Some(serde_json::from_slice(&bytes).context("invalid pack manifest")?);
        } else {
            let file = name
                .strip_prefix("files/")
                .filter(|n| plain_name(n))
                .context("pack contains an unexpected member")?;
            let mut out = fs::File::create_new(dir.path().join(file))?;
            std::io::copy(&mut entry, &mut out)?;
            members.push(file.to_owned());
        }
    }
    let manifest = manifest.context("pack has no manifest")?;
    ensure!(
        members.len() == manifest.files.len()
            && members
                .iter()
                .all(|m| manifest.files.iter().any(|f| &f.name == m)),
        "pack members do not match its manifest"
    );
    ensure!(
        manifest.format == FORMAT && manifest.version == VERSION,
        "unsupported pack format"
    );
    ensure!(
        manifest.kind == kind,
        "pack holds a {} artifact, not {kind}",
        manifest.kind
    );
    ensure!(
        manifest.base == base,
        "pack was made for a different base checkpoint (fingerprint mismatch)"
    );
    let expected: &[&str] = if kind == "lora" {
        &ADAPTER_FILES
    } else {
        &[STEERING_FILE]
    };
    ensure!(
        manifest.files.len() == expected.len()
            && manifest
                .files
                .iter()
                .all(|f| expected.contains(&f.name.as_str())),
        "pack file list does not match a {kind} artifact"
    );
    for file in &manifest.files {
        let path = dir.path().join(&file.name);
        ensure!(
            fs::metadata(&path)?.len() == file.bytes,
            "pack member {} has the wrong size",
            file.name
        );
        ensure!(
            file_sha(&path)? == file.sha256,
            "pack member {} failed its checksum",
            file.name
        );
    }
    Ok((manifest, dir))
}

fn pinned_base(config: &Config) -> Result<String> {
    base_fingerprint(Path::new(&config.model))
}

/// Export an adapter version as a verified pack (adapter weights and provenance only).
pub fn export_adapter(config: &Config, store: &Store, id: i64, out: &Path) -> Result<Value> {
    let version = store.version(id)?;
    ensure!(
        version.kind == "lora",
        "only LoRA adapter versions can be exported; version {id} is {}",
        version.kind
    );
    let adapter = version.adapter.as_ref().context("version has no adapter")?;
    let sources: Vec<(String, PathBuf)> = ADAPTER_FILES
        .iter()
        .map(|name| ((*name).to_owned(), adapter.join(name)))
        .collect();
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        kind: "lora".into(),
        base: pinned_base(config)?,
        source: json!({"version":id,"parent":version.parent,"status":version.status}),
        files: entries(&sources)?,
    };
    write_pack(out, &manifest, &sources)?;
    Ok(json!({"exported":id,"pack":out,"kind":"lora","base":manifest.base}))
}

/// Import an adapter pack as a pending candidate on the active version. Never activates it.
pub fn import_adapter(config: &Config, store: &Store, pack: &Path) -> Result<Version> {
    let base = pinned_base(config)?;
    let (manifest, dir) = open_pack(
        pack,
        &config.state_dir.join("packs"),
        "lora",
        &base,
        MAX_PACK_BYTES,
    )?;
    localize_adapter_config(dir.path(), &config.model)?;
    let weights = manifest
        .files
        .iter()
        .find(|f| f.name == "adapters.safetensors")
        .context("pack has no adapter weights")?;
    let name = format!("imported-{}", &weights.sha256[..12]);
    let target = config.state_dir.join("adapters").join(&name);
    ensure!(!target.exists(), "adapter {name} was already imported");
    fs::create_dir_all(target.parent().context("adapter root")?)?;
    fs::rename(dir.keep(), &target)?;
    let registered = (|| -> Result<Version> {
        let active = store.active()?;
        let version =
            store.candidate(active.id, "lora", &active.harness, Some(target.as_path()))?;
        store.set_meta(
            &format!("adapter_digest:{}", version.id),
            &crate::learn::improve::adapter_digest(&target)?,
        )?;
        Ok(version)
    })();
    if registered.is_err() {
        let _ = fs::remove_dir_all(&target);
    }
    registered
}

/// Point the adapter at this checkout's base model. The base fingerprint already matched, so the
/// weights are the same checkpoint; only the exporter's machine-specific path is replaced.
fn localize_adapter_config(dir: &Path, model: &str) -> Result<()> {
    let path = dir.join("adapter_config.json");
    let mut adapter: Value = serde_json::from_slice(&fs::read(&path)?)
        .context("adapter_config.json is not valid JSON")?;
    ensure!(adapter.is_object(), "adapter_config.json must be an object");
    if adapter.get("base_model_name_or_path").is_some() {
        adapter["base_model_name_or_path"] = json!(model);
    }
    fs::write(&path, serde_json::to_vec_pretty(&adapter)?)?;
    Ok(())
}

/// Export the local steering vector as a verified pack.
pub fn export_steering(config: &Config, out: &Path) -> Result<Value> {
    let path = crate::llm::steering::vector_path(config);
    ensure!(path.is_file(), "no steering vector at {}", path.display());
    let bundle: Value =
        serde_json::from_slice(&fs::read(&path)?).context("steering vector is not valid JSON")?;
    let sources = vec![(STEERING_FILE.to_owned(), path)];
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        kind: "steering".into(),
        base: pinned_base(config)?,
        source: json!({"layer":bundle["layer"],"evaluation":bundle.get("evaluation")}),
        files: entries(&sources)?,
    };
    write_pack(out, &manifest, &sources)?;
    Ok(json!({"exported":"steering","pack":out,"base":manifest.base}))
}

/// Import a steering pack as a pending vector beside the active one. The active vector is never
/// touched. The pending vector is re-bound to this checkout with its evaluation cleared; it becomes
/// active only after `dial steering evaluate --pending` passes locally and `dial steering promote`.
pub fn import_steering(config: &Config, pack: &Path, replace: bool) -> Result<Value> {
    let base = pinned_base(config)?;
    let (manifest, dir) = open_pack(
        pack,
        &config.state_dir.join("packs"),
        "steering",
        &base,
        MAX_PACK_BYTES,
    )?;
    let raw = fs::read(dir.path().join(STEERING_FILE))?;
    let bundle: Value =
        serde_json::from_slice(&raw).context("steering pack vector is not valid JSON")?;
    ensure!(bundle.is_object(), "steering pack vector must be an object");
    let name = format!("imported-{}", &manifest.files[0].sha256[..12]);
    let pending = steering::pending_path(config, &name)?;
    let _lock = steering::lock_vector(&pending)?;
    ensure!(
        replace || !pending.exists(),
        "a pending import named {name} already exists; pass --replace to overwrite that pending file"
    );
    let staged = pending.with_extension("staging.json");
    crate::config::settings_store::write_private(&staged, &raw)?;
    let request = json!({"op":"steering_import","model":config.model,"model_name":config.model,"adapter":null,"output":staged});
    crate::learn::worker::call_controlled(config, &request, None)?;
    fs::rename(&staged, &pending)?;
    Ok(json!({
        "imported":"steering",
        "pending":name,
        "layer":bundle["layer"],
        "awaiting":format!("dial steering evaluate --pending {name} --cases <file>"),
        "then":format!("dial steering promote {name}")
    }))
}

#[cfg(test)]
mod tests;
