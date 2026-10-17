//! Image references: images a session or import has attached, stored content-addressed under
//! `<user_dir>/images` as `<sha256>` blobs with a JSON index (`index.json`) mapping each digest to
//! the sessions that reference it.
//!
//! Expiry uses Dial's own image reference window (`IMAGE_TTL_HOURS`, 72 hours): an entry expires
//! `IMAGE_TTL_HOURS` after its last registration. Registering the same bytes again, or finding them
//! in a session transcript, re-arms the window, so images still in use survive `purge`.

use crate::platform::fs as platform_fs;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Serving window measured from the last registration (default 72 hours).
pub const IMAGE_TTL_HOURS: u64 = 72;
const IMAGE_TTL_SECS: u64 = IMAGE_TTL_HOURS * 3600;
const STORE_DIR: &str = "images";
const INDEX_FILE: &str = "index.json";
const LOCK_FILE: &str = ".lock";
/// Subdirectory of a sessions directory holding transcripts moved out by `dial gc` (still on disk).
const SESSION_ARCHIVE_DIR: &str = "archive";

/// One referenced image: its content digest, type, size, referencing sessions and timestamps (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub sha256: String,
    pub mime: String,
    pub bytes: u64,
    pub sessions: Vec<String>,
    pub created: u64,
    pub touched: u64,
}

impl ImageRef {
    /// Whether the serving window has lapsed at `now` (unix seconds).
    pub fn expired_at(&self, now: u64) -> bool {
        now.saturating_sub(self.touched) > IMAGE_TTL_SECS
    }
}

/// Image bytes supplied directly, or a file to read them from.
#[derive(Debug, Clone, Copy)]
pub enum ImageInput<'a> {
    Bytes(&'a [u8]),
}

/// Outcome of `purge`. `selected` lists every candidate; `deleted` counts the ones removed when applied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    pub applied: bool,
    pub all: bool,
    pub selected: Vec<ImageRef>,
    pub deleted: usize,
    pub errors: Vec<String>,
}

impl PurgeReport {
    /// Total bytes of the selected candidates.
    pub fn bytes(&self) -> u64 {
        self.selected.iter().map(|r| r.bytes).sum()
    }
}

/// Result of checking the store against its index.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub entries: usize,
    pub problems: Vec<String>,
    /// Blob files on disk that no index entry references.
    pub orphans: Vec<String>,
}

/// Directory holding blobs and the index.
pub fn store_dir(user_dir: &Path) -> PathBuf {
    user_dir.join(STORE_DIR)
}

/// Whether `sha` is a canonical SHA-256 hex digest (64 lowercase hex characters).
pub fn is_sha256_hex(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Register an image for `session_id`. Identical bytes share one blob and one entry; the session
/// is added to the entry's references and the serving window is re-armed.
#[cfg(test)]
pub fn register(
    user_dir: &Path,
    session_id: &str,
    input: ImageInput<'_>,
    mime: Option<&str>,
) -> Result<ImageRef> {
    register_at(user_dir, session_id, input, mime, now_secs())
}

fn register_at(
    user_dir: &Path,
    session_id: &str,
    input: ImageInput<'_>,
    mime: Option<&str>,
    now: u64,
) -> Result<ImageRef> {
    ensure!(!session_id.is_empty(), "image reference needs a session id");
    let ImageInput::Bytes(bytes) = input;
    let mime = resolve_mime(bytes, mime)?;
    let sha = sha256_hex(bytes);
    let dir = ensure_store(user_dir)?;
    with_lock(&dir, || {
        write_blob_if_needed(&dir, &sha, bytes)?;
        let mut refs = load_index(&dir)?;
        let entry = match refs.iter_mut().find(|r| r.sha256 == sha) {
            Some(existing) => {
                if !existing.sessions.iter().any(|s| s == session_id) {
                    existing.sessions.push(session_id.to_owned());
                }
                existing.touched = now;
                existing.clone()
            }
            None => {
                let entry = ImageRef {
                    sha256: sha.clone(),
                    mime: mime.clone(),
                    bytes: bytes.len() as u64,
                    sessions: vec![session_id.to_owned()],
                    created: now,
                    touched: now,
                };
                refs.push(entry.clone());
                entry
            }
        };
        save_index(&dir, &refs)?;
        Ok(entry)
    })
}

/// All references, most recently registered first.
pub fn list(user_dir: &Path) -> Result<Vec<ImageRef>> {
    let mut refs = load_index(&store_dir(user_dir))?;
    refs.sort_by(|a, b| {
        b.touched
            .cmp(&a.touched)
            .then_with(|| a.sha256.cmp(&b.sha256))
    });
    Ok(refs)
}

/// Path of the blob for a referenced digest. Rejects malformed digests and digests with no index entry.
pub fn resolve(user_dir: &Path, sha: &str) -> Result<PathBuf> {
    ensure!(is_sha256_hex(sha), "not a sha256 image digest");
    let dir = store_dir(user_dir);
    let refs = load_index(&dir)?;
    ensure!(refs.iter().any(|r| r.sha256 == sha), "unknown image {sha}");
    let path = dir.join(sha);
    ensure!(path.is_file(), "image blob {sha} is missing from the store");
    Ok(path)
}

/// Register every image block (`type":"image"` with base64 `data`, or an `image_url` data URL)
/// found in the `.jsonl` transcripts under `sessions_dir`. Returns the number of distinct images
/// registered across all transcripts (one per session, after in-file dedupe).
pub fn scan_sessions(user_dir: &Path, sessions_dir: &Path) -> Result<usize> {
    scan_sessions_at(user_dir, sessions_dir, now_secs())
}

/// Scan the session directory of every project under `user_dir` (`projects/*/sessions`), including
/// the cold `archive/` subdirectory that `dial gc` moves transcripts into. Archived transcripts stay
/// on disk and their images must keep their serving window too.
pub fn scan_all_sessions(user_dir: &Path) -> Result<usize> {
    let mut total = 0;
    for dir in session_dirs(user_dir)? {
        total += scan_sessions(user_dir, &dir)?;
        total += scan_sessions(user_dir, &dir.join(SESSION_ARCHIVE_DIR))?;
    }
    Ok(total)
}

/// Every `projects/<project>/sessions` directory that exists under `user_dir`, sorted.
pub fn session_dirs(user_dir: &Path) -> Result<Vec<PathBuf>> {
    let projects = user_dir.join("projects");
    let read = match fs::read_dir(&projects) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", projects.display())),
    };
    let mut dirs: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("sessions"))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn scan_sessions_at(user_dir: &Path, sessions_dir: &Path, now: u64) -> Result<usize> {
    let read = match fs::read_dir(sessions_dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", sessions_dir.display())),
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
        .collect();
    files.sort();
    let mut total = 0;
    for file in files {
        let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let session_id = stem.split_once('_').map_or(stem, |(_, id)| id).to_owned();
        // An unreadable transcript contributes nothing; it must not abort the scan.
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let mut found = Vec::new();
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                collect_images(&value, &mut found);
            }
        }
        let mut seen = HashSet::new();
        for (declared, data) in found {
            let Ok(bytes) = STANDARD.decode(data.trim()) else {
                continue;
            };
            if resolve_mime(&bytes, declared.as_deref()).is_err() {
                continue;
            }
            if !seen.insert(sha256_hex(&bytes)) {
                continue;
            }
            register_at(
                user_dir,
                &session_id,
                ImageInput::Bytes(&bytes),
                declared.as_deref(),
                now,
            )?;
            total += 1;
        }
    }
    Ok(total)
}

/// Collect `(declared mime, base64 data)` for every inline image in a transcript value.
fn collect_images(value: &Value, out: &mut Vec<(Option<String>, String)>) {
    match value {
        Value::Object(map) => {
            match map.get("type").and_then(Value::as_str) {
                Some("image") => {
                    if let Some(data) = map.get("data").and_then(Value::as_str) {
                        let mime = ["mimeType", "mime_type", "media_type"]
                            .iter()
                            .find_map(|k| map.get(*k).and_then(Value::as_str));
                        out.push((mime.map(str::to_owned), data.to_owned()));
                    } else if let Some(source) = map.get("source").and_then(Value::as_object)
                        && source.get("type").and_then(Value::as_str) == Some("base64")
                        && let Some(data) = source.get("data").and_then(Value::as_str)
                    {
                        let mime = source.get("media_type").and_then(Value::as_str);
                        out.push((mime.map(str::to_owned), data.to_owned()));
                    }
                }
                Some("image_url") => {
                    let url = map
                        .get("image_url")
                        .and_then(|v| v.as_str().or_else(|| v.get("url").and_then(Value::as_str)));
                    if let Some(url) = url
                        && let Some(rest) = url.strip_prefix("data:")
                        && let Some((mime, data)) = rest.split_once(";base64,")
                    {
                        out.push((Some(mime.to_owned()), data.to_owned()));
                    }
                }
                _ => {}
            }
            for child in map.values() {
                collect_images(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_images(item, out);
            }
        }
        _ => {}
    }
}

/// Remove references whose window has lapsed (or every reference with `all`) and their blobs.
/// Dry-run unless `apply`. Blob removal failures keep the index entry so the reference is not lost.
pub fn purge(user_dir: &Path, all: bool, apply: bool) -> Result<PurgeReport> {
    purge_at(user_dir, all, apply, now_secs())
}

fn purge_at(user_dir: &Path, all: bool, apply: bool, now: u64) -> Result<PurgeReport> {
    let dir = store_dir(user_dir);
    let mut report = PurgeReport {
        applied: apply,
        all,
        ..Default::default()
    };
    if !dir.is_dir() {
        return Ok(report);
    }
    let candidate = |r: &ImageRef| all || r.expired_at(now);
    if !apply {
        report.selected = load_index(&dir)?.into_iter().filter(candidate).collect();
        return Ok(report);
    }
    with_lock(&dir, || {
        let mut kept = Vec::new();
        for entry in load_index(&dir)? {
            if !candidate(&entry) {
                kept.push(entry);
                continue;
            }
            match fs::remove_file(dir.join(&entry.sha256)) {
                Ok(()) => report.deleted += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => report.deleted += 1,
                Err(e) => {
                    report.errors.push(format!("{}: {e}", entry.sha256));
                    kept.push(entry.clone());
                    continue;
                }
            }
            report.selected.push(entry);
        }
        save_index(&dir, &kept)?;
        Ok(())
    })?;
    Ok(report)
}

/// Check every index entry against its blob (existence, size, sha256) and list blobs no entry
/// references. A read-only operation: a missing store is an empty, healthy report.
pub fn verify(user_dir: &Path) -> Result<VerifyReport> {
    let dir = store_dir(user_dir);
    let mut report = VerifyReport::default();
    if !dir.is_dir() {
        return Ok(report);
    }
    let refs = load_index(&dir)?;
    report.entries = refs.len();
    for entry in &refs {
        if !is_sha256_hex(&entry.sha256) {
            report.problems.push(format!(
                "index entry has an invalid sha256: {}",
                entry.sha256
            ));
            continue;
        }
        match fs::read(dir.join(&entry.sha256)) {
            Ok(bytes) => {
                let actual = sha256_hex(&bytes);
                if actual != entry.sha256 {
                    report
                        .problems
                        .push(format!("{}: blob content hashes to {actual}", entry.sha256));
                } else if bytes.len() as u64 != entry.bytes {
                    report.problems.push(format!(
                        "{}: blob is {} bytes, index says {}",
                        entry.sha256,
                        bytes.len(),
                        entry.bytes
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.problems.push(format!(
                "{}: blob is missing (referenced by {} session(s))",
                entry.sha256,
                entry.sessions.len()
            )),
            Err(e) => report
                .problems
                .push(format!("{}: blob is unreadable: {e}", entry.sha256)),
        }
    }
    for item in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = item?.file_name().to_string_lossy().into_owned();
        if is_sha256_hex(&name) && !refs.iter().any(|r| r.sha256 == name) {
            report.orphans.push(name);
        }
    }
    report.orphans.sort();
    Ok(report)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

const SNIFFED_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Content type from magic bytes (authoritative), else the declared type if it is an image type.
fn resolve_mime(bytes: &[u8], declared: Option<&str>) -> Result<String> {
    let sniffed = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    };
    if let Some(mime) = sniffed {
        return Ok(mime.to_owned());
    }
    match declared {
        Some(mime) if SNIFFED_MIMES.contains(&mime) => {
            bail!("declared mime type {mime} does not match the image data")
        }
        Some(mime) if mime.starts_with("image/") => Ok(mime.to_owned()),
        Some(mime) => bail!("declared mime type {mime} is not an image type"),
        None => bail!("image data is not a supported PNG, JPEG, GIF or WebP image"),
    }
}

fn ensure_store(user_dir: &Path) -> Result<PathBuf> {
    let dir = store_dir(user_dir);
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    match platform_fs::create_dir_owner_only(&builder, &dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Tighten a pre-existing directory best effort; we may not own it.
            let _ = platform_fs::set_owner_only(&dir);
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    Ok(dir)
}

/// Run `f` holding an exclusive lock on the store, so index read-modify-write cycles from
/// concurrent processes do not lose updates.
fn with_lock<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock = platform_fs::open_owner_only(
        OpenOptions::new().create(true).write(true),
        &dir.join(LOCK_FILE),
    )
    .with_context(|| format!("opening image store lock in {}", dir.display()))?;
    lock.lock_exclusive().context("locking image store")?;
    f()
}

fn write_blob_if_needed(dir: &Path, sha: &str, bytes: &[u8]) -> Result<()> {
    let path = dir.join(sha);
    // A blob that already hashes to its name is reused; anything else (torn or corrupt) is rewritten.
    if fs::read(&path).is_ok_and(|existing| sha256_hex(&existing) == sha) {
        return Ok(());
    }
    write_atomic(&path, bytes)
}

/// Stage `data` in an owner-only temp file beside `target`, then rename it into place.
fn write_atomic(target: &Path, data: &[u8]) -> Result<()> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .context("store path has no file name")?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = target.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()));
    let mut file =
        platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), &tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, target)) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", target.display()));
    }
    Ok(())
}

fn load_index(dir: &Path) -> Result<Vec<ImageRef>> {
    let text = match fs::read_to_string(dir.join(INDEX_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading image index"),
    };
    let value: Value = serde_json::from_str(&text).context("image index is corrupt")?;
    let items = value
        .get("images")
        .and_then(Value::as_array)
        .context("image index has no images array")?;
    items.iter().map(parse_ref).collect()
}

fn parse_ref(value: &Value) -> Result<ImageRef> {
    let text = |key: &str| -> Result<String> {
        Ok(value
            .get(key)
            .and_then(Value::as_str)
            .with_context(|| format!("image index entry lacks {key}"))?
            .to_owned())
    };
    let number = |key: &str| -> Result<u64> {
        value
            .get(key)
            .and_then(Value::as_u64)
            .with_context(|| format!("image index entry lacks {key}"))
    };
    Ok(ImageRef {
        sha256: text("sha256")?,
        mime: text("mime")?,
        bytes: number("bytes")?,
        sessions: value
            .get("sessions")
            .and_then(Value::as_array)
            .context("image index entry lacks sessions")?
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect(),
        created: number("created")?,
        touched: number("touched")?,
    })
}

fn save_index(dir: &Path, refs: &[ImageRef]) -> Result<()> {
    let images: Vec<Value> = refs
        .iter()
        .map(|r| {
            json!({
                "sha256": r.sha256,
                "mime": r.mime,
                "bytes": r.bytes,
                "sessions": r.sessions,
                "created": r.created,
                "touched": r.touched,
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({"version": 1, "images": images}))?;
    write_atomic(&dir.join(INDEX_FILE), text.as_bytes())
}

#[cfg(test)]
mod tests;
