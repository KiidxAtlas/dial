//! `dial gc`: reclaim disk space held by Dial state.
//!
//! Sweeps, in the order they run: `stale` (dangling artifact lifecycle records), `blobs` (unreferenced
//! content-addressed artifacts), `archive` (cold sessions moved to `sessions/archive/`), and `wal`
//! (SQLite WAL checkpoints). With no sweep flag, blobs, archive and wal run; stale is opt-in.
//! Dry-run is the default and touches nothing; `--apply` takes `gc.sqlite3.lock` first.

use fs2::FileExt;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use regex::bytes::Regex;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::storage::{
    paths::Layout,
    retention::{self, ArchiveRecord, Candidate, RetentionPolicy, Store, Verdict},
    settings_store,
};

pub const DEFAULT_COLD_ARCHIVE_AFTER_DAYS: u64 = 30;
pub const DEFAULT_RETAIN_NEWEST_GLOBAL: usize = 20;
pub const DEFAULT_RETAIN_NEWEST_PER_CWD: usize = 10;
pub const DEFAULT_STALE_RETAIN_NEWEST: usize = 20;
pub const DEFAULT_STALE_RETAIN_DAYS: u64 = 30;
const ARTIFACTS_DIR: &str = "artifacts";
const LIFECYCLE_DIR: &str = ".lifecycle";
const ARCHIVE_DIR: &str = "archive";
const SPOOL_PREFIX: &str = ".spool-";
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Session header lines are read with a cap so a corrupt file cannot exhaust memory.
const HEADER_LIMIT: u64 = 64 * 1024;

/// Arguments for `dial gc [flags]`.
#[derive(Debug, Clone, clap::Args)]
pub struct GcArgs {
    /// Perform the sweeps. Without this flag gc only reports what it would do.
    #[arg(long)]
    pub apply: bool,
    /// Emit the report as JSON.
    #[arg(long)]
    pub json: bool,
    /// Sweep unreferenced content-addressed artifacts.
    #[arg(long)]
    pub blobs: bool,
    /// Archive cold sessions into `sessions/archive/`.
    #[arg(long)]
    pub archive: bool,
    /// Checkpoint SQLite WAL files.
    #[arg(long)]
    pub wal: bool,
    /// Prune dangling artifact lifecycle records (opt-in).
    #[arg(long)]
    pub stale: bool,
    /// Sessions untouched for this many days are archived (0 archives every unprotected one).
    #[arg(long, value_name = "DAYS")]
    pub cold_archive_after_days: Option<u64>,
    /// Newest sessions protected globally from archiving.
    #[arg(long, value_name = "N")]
    pub retain_newest_global: Option<usize>,
    /// Newest sessions protected per working directory.
    #[arg(long, value_name = "N")]
    pub retain_newest_per_cwd: Option<usize>,
    /// Newest stale records kept regardless of age.
    #[arg(long, value_name = "N")]
    pub stale_retain_newest: Option<usize>,
    /// Stale records older than this many days are pruned.
    #[arg(long, value_name = "DAYS")]
    pub stale_retain_days: Option<u64>,
}

/// Retention settings an apply run used; stored with the run record.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedSettings {
    pub cold_archive_after_days: u64,
    pub retain_newest_global: usize,
    pub retain_newest_per_cwd: usize,
    pub stale_retain_newest: usize,
    pub stale_retain_days: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlobResult {
    pub referenced: usize,
    pub candidates: usize,
    pub would_delete: usize,
    pub deleted: usize,
    pub bytes: u64,
    /// Referenced artifacts whose bodies are absent. They hold nothing to keep.
    pub missing_references: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveResult {
    pub scanned: usize,
    pub skipped_recent: usize,
    pub kept_newest_global: usize,
    pub kept_newest_per_cwd: usize,
    pub not_cold: usize,
    pub would_archive: usize,
    pub archived: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaleResult {
    pub dangling_records: usize,
    pub kept_newest: usize,
    pub would_delete: usize,
    pub deleted: usize,
    pub bytes: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WalDatabase {
    pub db_path: String,
    pub wal_bytes: u64,
    pub would_checkpoint: bool,
    pub checkpointed: bool,
    pub busy: i64,
    pub log: i64,
    pub checkpointed_frames: i64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WalResult {
    pub databases: Vec<WalDatabase>,
    pub wal_bytes: u64,
    pub would_checkpoint: bool,
    pub checkpointed: bool,
    pub errors: Vec<String>,
}

/// Full gc report. Serialized as camelCase JSON, with `stateDir` as the Dial state directory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub state_dir: String,
    pub lock_path: String,
    pub apply: bool,
    pub settings: AppliedSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<StaleResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<BlobResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal: Option<WalResult>,
}

impl GcReport {
    /// Every error from every sweep that ran, in sweep order.
    pub fn errors(&self) -> Vec<&str> {
        let mut out = Vec::new();
        if let Some(s) = &self.stale {
            out.extend(s.errors.iter().map(String::as_str));
        }
        if let Some(b) = &self.blobs {
            out.extend(b.errors.iter().map(String::as_str));
        }
        if let Some(a) = &self.archive {
            out.extend(a.errors.iter().map(String::as_str));
        }
        if let Some(w) = &self.wal {
            out.extend(w.errors.iter().map(String::as_str));
        }
        out
    }
}

/// Which sweeps a set of flags selects. With no sweep flag: blobs, archive and wal; stale is
/// opt-in. With any sweep flag: exactly the flagged ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub stale: bool,
    pub blobs: bool,
    pub archive: bool,
    pub wal: bool,
}

pub fn select(args: &GcArgs) -> Selection {
    let any = args.blobs || args.archive || args.wal || args.stale;
    if any {
        Selection {
            stale: args.stale,
            blobs: args.blobs,
            archive: args.archive,
            wal: args.wal,
        }
    } else {
        Selection {
            stale: false,
            blobs: true,
            archive: true,
            wal: true,
        }
    }
}

pub fn applied_settings(args: &GcArgs) -> AppliedSettings {
    AppliedSettings {
        cold_archive_after_days: args
            .cold_archive_after_days
            .unwrap_or(DEFAULT_COLD_ARCHIVE_AFTER_DAYS),
        retain_newest_global: args
            .retain_newest_global
            .unwrap_or(DEFAULT_RETAIN_NEWEST_GLOBAL),
        retain_newest_per_cwd: args
            .retain_newest_per_cwd
            .unwrap_or(DEFAULT_RETAIN_NEWEST_PER_CWD),
        stale_retain_newest: args
            .stale_retain_newest
            .unwrap_or(DEFAULT_STALE_RETAIN_NEWEST),
        stale_retain_days: args.stale_retain_days.unwrap_or(DEFAULT_STALE_RETAIN_DAYS),
    }
}

/// Run gc against `layout`, print the report, and return the exit code (1 when any sweep
/// reported errors, else 0).
pub fn run(layout: &Layout, args: GcArgs) -> Result<i32> {
    let report = execute(layout, &args, retention::now_ms())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(if report.errors().is_empty() { 0 } else { 1 })
}

/// Perform the selected sweeps. Takes the gc lock only when applying; dry-run writes nothing.
pub fn execute(layout: &Layout, args: &GcArgs, now_ms: i64) -> Result<GcReport> {
    let selection = select(args);
    let settings = applied_settings(args);
    let policy = RetentionPolicy {
        cold_archive_after_days: settings.cold_archive_after_days,
        retain_newest_global: settings.retain_newest_global,
        retain_newest_per_cwd: settings.retain_newest_per_cwd,
    };
    let db_path = layout.user_dir.join(retention::DB_FILE);
    let lock_path = db_path.with_extension("sqlite3.lock");
    let _lock = if args.apply {
        settings_store::private_dir(&layout.user_dir)?;
        Some(settings_store::locked(&db_path)?)
    } else {
        None
    };

    let stale = selection
        .stale
        .then(|| sweep_stale(layout, &settings, args.apply, now_ms));
    let blobs = selection
        .blobs
        .then(|| sweep_blobs(layout, args.apply, now_ms));
    let archive = selection
        .archive
        .then(|| sweep_archive(layout, &policy, &settings, args.apply, now_ms));
    let wal = selection.wal.then(|| sweep_wal(layout, args.apply));

    Ok(GcReport {
        state_dir: layout.state_dir.display().to_string(),
        lock_path: lock_path.display().to_string(),
        apply: args.apply,
        settings,
        stale,
        blobs,
        archive,
        wal,
    })
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Select the entries in a newest-first list that are not protected by `keep_newest` and are
/// older than `cutoff_ms`. Input is `(key, modified_ms)` in any order.
pub fn expired_beyond_keep<T: Clone>(
    mut items: Vec<(T, i64)>,
    keep_newest: usize,
    cutoff_ms: i64,
) -> (usize, Vec<T>) {
    items.sort_by(|a, b| b.1.cmp(&a.1));
    let kept = keep_newest.min(items.len());
    let expired = items[kept..]
        .iter()
        .filter(|(_, mtime)| *mtime < cutoff_ms)
        .map(|(item, _)| item.clone())
        .collect();
    (kept, expired)
}

fn regular_file_entries(dir: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_file() {
            out.push((entry.path(), meta));
        }
    }
    Ok(out)
}

fn remove_file_counted(path: &Path, bytes: u64) -> Result<u64> {
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------
// Stale: dangling artifact lifecycle records
// ---------------------------------------------------------------------------------------------

/// Dial's only stale category: `artifacts/.lifecycle/<hash>.json` records whose artifact file is
/// gone. Ranked newest first; the newest `stale_retain_newest` are kept, and the rest are pruned
/// once older than `max(stale_retain_days, write grace)`.
fn sweep_stale(
    layout: &Layout,
    settings: &AppliedSettings,
    apply: bool,
    now_ms: i64,
) -> StaleResult {
    let mut result = StaleResult::default();
    let artifacts = layout.state_dir.join(ARTIFACTS_DIR);
    let lifecycle = artifacts.join(LIFECYCLE_DIR);
    if !lifecycle.is_dir() {
        return result;
    }
    let entries = match regular_file_entries(&lifecycle) {
        Ok(e) => e,
        Err(e) => {
            result.errors.push(format!("{e:#}"));
            return result;
        }
    };
    let mut dangling = Vec::new();
    for (path, meta) in entries {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(hash) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_artifact_hash(hash) || artifacts.join(hash).exists() {
            continue;
        }
        result.dangling_records += 1;
        dangling.push(((path, meta.len()), retention::modified_ms(&meta)));
    }
    let age_ms = i64::try_from(
        settings
            .stale_retain_days
            .saturating_mul(retention::DAY_MS as u64),
    )
    .unwrap_or(i64::MAX);
    let cutoff = now_ms.saturating_sub(age_ms.max(retention::WRITE_GRACE_MS));
    let (kept, expired) = expired_beyond_keep(dangling, settings.stale_retain_newest, cutoff);
    result.kept_newest = kept;
    result.would_delete = expired.len();
    result.bytes = expired.iter().map(|(_, len)| *len).sum();
    if apply {
        for (path, len) in expired {
            match remove_file_counted(&path, len) {
                Ok(_) => result.deleted += 1,
                Err(e) => result.errors.push(format!("{e:#}")),
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Blobs: unreferenced content-addressed artifacts
// ---------------------------------------------------------------------------------------------

fn is_artifact_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn artifact_ref_regex() -> Regex {
    Regex::new(r"artifact://([0-9a-fA-F]{64})").expect("static regex")
}

/// Result of a complete reference scan. `missing` lists referenced artifacts whose bodies are
/// absent: there is nothing to keep for them, so they are reported rather than treated as failures.
#[derive(Debug, Default)]
pub struct ReferenceScan {
    pub refs: HashSet<String>,
    pub missing: Vec<String>,
}

/// Collect every artifact hash referenced from state files and top-level user-dir files, then
/// transitively from the bodies of referenced artifacts. Scanning raw bytes is deliberately
/// conservative: a reference anywhere keeps the blob. Any read failure other than a missing
/// referenced body fails the whole scan, because a partial reference set is not safe to sweep.
pub fn collect_references(layout: &Layout) -> Result<ReferenceScan> {
    let re = artifact_ref_regex();
    let artifacts = layout.state_dir.join(ARTIFACTS_DIR);
    let mut refs: HashSet<String> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    if layout.user_dir.is_dir() {
        for (path, _) in regular_file_entries(&layout.user_dir)? {
            stack.push(path);
        }
    }
    let mut dirs = vec![layout.state_dir.clone()];
    while let Some(dir) = dirs.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                if path != artifacts {
                    dirs.push(path);
                }
            } else if meta.is_file() {
                stack.push(path);
            }
        }
    }
    let scan = |bytes: &[u8], refs: &mut HashSet<String>, pending: &mut Vec<String>| {
        for cap in re.captures_iter(bytes) {
            let hash = String::from_utf8_lossy(&cap[1]).to_ascii_lowercase();
            if refs.insert(hash.clone()) {
                pending.push(hash);
            }
        }
    };
    let mut pending: Vec<String> = Vec::new();
    for path in stack {
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        scan(&bytes, &mut refs, &mut pending);
    }
    // Bodies of referenced artifacts may themselves reference other artifacts.
    let mut missing = Vec::new();
    while let Some(hash) = pending.pop() {
        let body = artifacts.join(&hash);
        match fs::read(&body) {
            Ok(bytes) => scan(&bytes, &mut refs, &mut pending),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(hash),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "reading referenced artifact {hash} ({})",
                    body.display()
                )));
            }
        }
    }
    Ok(ReferenceScan { refs, missing })
}

/// Claim a spool for deletion without blocking. The owning spool holds an exclusive lock on its
/// file and tail ring for its whole lifetime, so a lock that cannot be taken at once means the
/// spool is live. Returns the held claim, or `None` when the spool is live or cannot be opened.
fn claim_spool(path: &Path) -> Option<fs::File> {
    let file = crate::platform::fs::open_nofollow_nonblocking(
        &fs::OpenOptions::new().read(true).write(true),
        path,
    )
    .ok()?;
    FileExt::try_lock_exclusive(&file).ok()?;
    Some(file)
}

fn sweep_blobs(layout: &Layout, apply: bool, now_ms: i64) -> BlobResult {
    let mut result = BlobResult::default();
    let artifacts = layout.state_dir.join(ARTIFACTS_DIR);
    if !artifacts.is_dir() {
        return result;
    }
    let scan = match collect_references(layout) {
        Ok(s) => s,
        Err(e) => {
            // Without a complete reference set nothing is safe to delete.
            result
                .errors
                .push(format!("reference scan incomplete, nothing deleted: {e:#}"));
            return result;
        }
    };
    result.referenced = scan.refs.len();
    result.missing_references = scan.missing.len();
    let refs = scan.refs;
    let entries = match regular_file_entries(&artifacts) {
        Ok(e) => e,
        Err(e) => {
            result.errors.push(format!("{e:#}"));
            return result;
        }
    };
    let cutoff = now_ms - retention::WRITE_GRACE_MS;
    // Each doomed spool carries its claim until its file is removed.
    let mut doomed: Vec<(PathBuf, u64, Option<fs::File>)> = Vec::new();
    for (path, meta) in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_blob = is_artifact_hash(name);
        let is_spool = name.starts_with(SPOOL_PREFIX);
        if !is_blob && !is_spool {
            continue;
        }
        if is_blob && refs.contains(name) {
            continue;
        }
        // A live spool is not a candidate at all, however old its head looks.
        let claim = if is_spool {
            match claim_spool(&path) {
                Some(claim) => Some(claim),
                None => continue,
            }
        } else {
            None
        };
        result.candidates += 1;
        if retention::modified_ms(&meta) < cutoff {
            result.would_delete += 1;
            result.bytes += meta.len();
            doomed.push((path, meta.len(), claim));
        }
    }
    if apply {
        for (path, len, _claim) in doomed {
            match remove_file_counted(&path, len) {
                Ok(_) => {
                    result.deleted += 1;
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if is_artifact_hash(name) {
                            let record = artifacts.join(LIFECYCLE_DIR).join(format!("{name}.json"));
                            match fs::remove_file(&record) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                Err(e) => result.errors.push(format!("{}: {e}", record.display())),
                            }
                        }
                    }
                }
                Err(e) => result.errors.push(format!("{e:#}")),
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Archive: cold sessions into sessions/archive/
// ---------------------------------------------------------------------------------------------

/// Top-level `<ts>_<id>.jsonl` session files in `sessions_dir`, with cwd read from the header.
pub fn session_candidates(sessions_dir: &Path) -> Result<Vec<Candidate>> {
    if !sessions_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (path, meta) in regular_file_entries(sessions_dir)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some((_, session_id)) = stem.split_once('_') else {
            continue;
        };
        if session_id.is_empty() {
            continue;
        }
        out.push(Candidate {
            cwd: header_cwd(&path),
            session_id: session_id.to_string(),
            modified_ms: retention::modified_ms(&meta),
            path,
        });
    }
    Ok(out)
}

/// The `cwd` field of a session's header line, or empty when unreadable.
fn header_cwd(path: &Path) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut line = Vec::new();
    if BufReader::new(file.take(HEADER_LIMIT))
        .read_until(b'\n', &mut line)
        .is_err()
    {
        return String::new();
    }
    serde_json::from_slice::<Value>(&line)
        .ok()
        .and_then(|v| v.get("cwd").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn sweep_archive(
    layout: &Layout,
    policy: &RetentionPolicy,
    settings: &AppliedSettings,
    apply: bool,
    now_ms: i64,
) -> ArchiveResult {
    let mut result = ArchiveResult::default();
    let sessions_dir = layout.sessions_dir();
    let candidates = match session_candidates(&sessions_dir) {
        Ok(c) => c,
        Err(e) => {
            result.errors.push(format!("{e:#}"));
            return result;
        }
    };
    result.scanned = candidates.len();
    let mut to_archive = Vec::new();
    for (candidate, verdict) in retention::classify(candidates, policy, now_ms) {
        match verdict {
            Verdict::Recent => result.skipped_recent += 1,
            Verdict::KeptNewestGlobal => result.kept_newest_global += 1,
            Verdict::KeptNewestPerCwd => result.kept_newest_per_cwd += 1,
            Verdict::Young => result.not_cold += 1,
            Verdict::Archive => {
                result.would_archive += 1;
                to_archive.push((candidate, verdict));
            }
        }
    }
    if !apply || to_archive.is_empty() {
        return result;
    }
    let archive_dir = sessions_dir.join(ARCHIVE_DIR);
    let store = match open_archive_store(layout, settings) {
        Ok(s) => s,
        Err(e) => {
            result.errors.push(format!("{e:#}"));
            return result;
        }
    };
    let (store, run_id) = store;
    for (candidate, _) in to_archive {
        let Some(name) = candidate.path.file_name() else {
            continue;
        };
        // Retention never relocates an active actor's claimed inode.
        let claim = match crate::platform::fs::open_nofollow_nonblocking(
            &std::fs::OpenOptions::new().read(true).write(true),
            &candidate.path,
        ) {
            Ok(file) => file,
            Err(error) => {
                result.errors.push(format!(
                    "claiming {} for archive: {error}",
                    candidate.path.display()
                ));
                continue;
            }
        };
        if FileExt::try_lock_exclusive(&claim).is_err() {
            result.errors.push(format!(
                "session is still owned by an active writer; archive skipped: {}",
                candidate.path.display()
            ));
            continue;
        }
        let dest = archive_dir.join(name);
        if dest.exists() {
            result
                .errors
                .push(format!("archive destination exists: {}", dest.display()));
            continue;
        }
        if let Err(e) = fs::rename(&candidate.path, &dest) {
            result.errors.push(format!(
                "moving {} to {}: {e}",
                candidate.path.display(),
                dest.display()
            ));
            continue;
        }
        if let Err(error) = crate::runtime::stream_journal::relocate(&candidate.path, &dest) {
            // The stream journal did not move, so only the main file needs to come back.
            let note = match fs::rename(&dest, &candidate.path) {
                Ok(()) => String::new(),
                Err(re) => format!(
                    "; rollback failed: the session stays archived while its stream journal stays live: {re}"
                ),
            };
            result.errors.push(format!(
                "moving stream journal for {}: {error}{note}",
                candidate.path.display()
            ));
            continue;
        }
        let record = ArchiveRecord {
            session_id: candidate.session_id.clone(),
            original_path: candidate.path.display().to_string(),
            archived_path: dest.display().to_string(),
            archived_at_ms: now_ms,
            cwd: candidate.cwd.clone(),
            reason: format!(
                "cold: untouched for more than {} days, outside newest {} global / {} per cwd",
                settings.cold_archive_after_days,
                settings.retain_newest_global,
                settings.retain_newest_per_cwd
            ),
        };
        if let Err(e) = store.record_archive(run_id, &record) {
            // Without the record the move cannot be reversed by name, so put the pair back.
            let note = match restore_archived(&dest, &candidate.path) {
                Ok(()) => "rolled back".to_string(),
                Err(re) => format!("rollback failed: {re}"),
            };
            result.errors.push(format!(
                "recording archive of {} failed ({e:#}); {note}",
                candidate.path.display()
            ));
            continue;
        }
        result.archived += 1;
    }
    result
}

/// Moves an archived session and its stream journal back. The main file returns first and the
/// stream journal only after that succeeds, so a failed main rename leaves the pair together in the
/// archive instead of splitting it.
fn restore_archived(dest: &Path, original: &Path) -> Result<(), String> {
    fs::rename(dest, original).map_err(|e| format!("restoring {}: {e}", original.display()))?;
    crate::runtime::stream_journal::relocate(dest, original).map_err(|e| {
        format!(
            "restoring stream journal of {}: {e}; the session file is back",
            original.display()
        )
    })
}

/// Open the gc store, make sure the archive directory exists, and begin a run.
fn open_archive_store(layout: &Layout, settings: &AppliedSettings) -> Result<(Store, i64)> {
    let archive_dir = layout.sessions_dir().join(ARCHIVE_DIR);
    settings_store::private_dir(&layout.sessions_dir())?;
    settings_store::private_dir(&archive_dir)?;
    let store = Store::open(&layout.user_dir)?;
    let run_id = store.begin_run(settings)?;
    Ok((store, run_id))
}

// ---------------------------------------------------------------------------------------------
// WAL: checkpoint SQLite write-ahead logs
// ---------------------------------------------------------------------------------------------

/// Databases gc checkpoints: the user-level stats db and each learn trajectory store
/// (`state_dir/models/<id>/state.sqlite3`) that exists.
pub fn wal_databases(layout: &Layout) -> Vec<PathBuf> {
    let mut out = vec![layout.user_dir.join("stats.sqlite3")];
    if let Ok(entries) = fs::read_dir(layout.state_dir.join("models")) {
        for entry in entries.flatten() {
            let db = entry.path().join("state.sqlite3");
            if db.is_file() {
                out.push(db);
            }
        }
    }
    out.retain(|p| p.is_file());
    out
}

fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn wal_size(db: &Path) -> u64 {
    fs::metadata(wal_path(db)).map(|m| m.len()).unwrap_or(0)
}

fn sweep_wal(layout: &Layout, apply: bool) -> WalResult {
    let mut result = WalResult::default();
    for db in wal_databases(layout) {
        let wal_bytes = wal_size(&db);
        let mut entry = WalDatabase {
            db_path: db.display().to_string(),
            wal_bytes,
            would_checkpoint: wal_bytes > 0,
            ..Default::default()
        };
        result.wal_bytes += wal_bytes;
        if wal_bytes > 0 {
            result.would_checkpoint = true;
        }
        if apply {
            match checkpoint(&db) {
                Ok((busy, log, frames)) => {
                    entry.busy = busy;
                    entry.log = log;
                    entry.checkpointed_frames = frames;
                    let remaining = wal_size(&db);
                    entry.checkpointed = busy == 0 && remaining == 0;
                    if busy > 0 {
                        result.errors.push(format!(
                            "{}: WAL checkpoint busy (another connection is active)",
                            db.display()
                        ));
                    } else if remaining > 0 {
                        result.errors.push(format!(
                            "{}: WAL still {remaining} bytes after TRUNCATE checkpoint",
                            db.display()
                        ));
                    }
                }
                Err(e) => result.errors.push(format!("{e:#}")),
            }
            result.checkpointed |= entry.checkpointed;
        }
        result.databases.push(entry);
    }
    result
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`, returning (busy, log, checkpointed frames).
fn checkpoint(db: &Path) -> Result<(i64, i64, i64)> {
    let conn = Connection::open(db).with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let row = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    Ok(row)
}

// ---------------------------------------------------------------------------------------------
// Text rendering
// ---------------------------------------------------------------------------------------------

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn render_text(report: &GcReport) -> String {
    let mode = if report.apply { "apply" } else { "dry-run" };
    let mut out = format!("dial gc ({mode}) state: {}\n", report.state_dir);
    if let Some(s) = &report.stale {
        out.push_str(&format!(
            "stale: {} dangling record(s), kept {}, {} {} {}\n",
            s.dangling_records,
            s.kept_newest,
            if report.apply {
                "deleted"
            } else {
                "would delete"
            },
            if report.apply {
                s.deleted
            } else {
                s.would_delete
            },
            format_bytes(s.bytes),
        ));
    }
    if let Some(b) = &report.blobs {
        out.push_str(&format!(
            "blobs: {} referenced, {} unreferenced, {} {} ({})\n",
            b.referenced,
            b.candidates,
            if report.apply {
                "deleted"
            } else {
                "would delete"
            },
            if report.apply {
                b.deleted
            } else {
                b.would_delete
            },
            format_bytes(b.bytes),
        ));
    }
    if let Some(a) = &report.archive {
        out.push_str(&format!(
            "archive: {} scanned, kept {} global / {} per-cwd, {} recent, {} not cold, {} {}\n",
            a.scanned,
            a.kept_newest_global,
            a.kept_newest_per_cwd,
            a.skipped_recent,
            a.not_cold,
            if report.apply {
                "archived"
            } else {
                "would archive"
            },
            if report.apply {
                a.archived
            } else {
                a.would_archive
            },
        ));
    }
    if let Some(w) = &report.wal {
        out.push_str(&format!(
            "wal: {} database(s), {} in WAL{}\n",
            w.databases.len(),
            format_bytes(w.wal_bytes),
            if report.apply { ", checkpointed" } else { "" },
        ));
    }
    for error in report.errors() {
        out.push_str(&format!("error: {error}\n"));
    }
    if !report.apply {
        out.push_str("dry-run: nothing changed; pass --apply to perform these sweeps\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const NOW: i64 = 1_800_000_000_000;

    fn layout(root: &Path) -> Layout {
        let cwd = root.join("project");
        let user = root.join("user");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&user).unwrap();
        Layout::with_user_dir(&cwd, user).unwrap()
    }

    fn args() -> GcArgs {
        GcArgs {
            apply: false,
            json: false,
            blobs: false,
            archive: false,
            wal: false,
            stale: false,
            cold_archive_after_days: None,
            retain_newest_global: None,
            retain_newest_per_cwd: None,
            stale_retain_newest: None,
            stale_retain_days: None,
        }
    }

    fn set_age(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn write_session(layout: &Layout, file: &str, cwd: &str, age: Duration) -> PathBuf {
        let dir = layout.sessions_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        let header = serde_json::json!({"type": "session", "id": "x", "cwd": cwd});
        fs::write(&path, format!("{header}\n{{\"type\":\"message\"}}\n")).unwrap();
        set_age(&path, age);
        path
    }

    #[test]
    fn default_selection_is_blobs_archive_wal_without_stale() {
        let s = select(&args());
        assert_eq!(
            s,
            Selection {
                stale: false,
                blobs: true,
                archive: true,
                wal: true
            }
        );
    }

    #[test]
    fn any_sweep_flag_selects_only_flagged_sweeps() {
        let mut a = args();
        a.stale = true;
        assert_eq!(
            select(&a),
            Selection {
                stale: true,
                blobs: false,
                archive: false,
                wal: false
            }
        );
        let mut b = args();
        b.wal = true;
        assert_eq!(
            select(&b),
            Selection {
                stale: false,
                blobs: false,
                archive: false,
                wal: true
            }
        );
    }

    #[test]
    fn default_settings_match_omp() {
        let s = applied_settings(&args());
        assert_eq!(s.cold_archive_after_days, 30);
        assert_eq!(s.retain_newest_global, 20);
        assert_eq!(s.retain_newest_per_cwd, 10);
        assert_eq!(s.stale_retain_newest, 20);
        assert_eq!(s.stale_retain_days, 30);
    }

    #[test]
    fn expired_beyond_keep_protects_newest_and_respects_cutoff() {
        let items = vec![("old", 10), ("new", 100), ("mid", 50)];
        let (kept, expired) = expired_beyond_keep(items, 1, 60);
        assert_eq!(kept, 1);
        assert_eq!(expired, vec!["mid", "old"]);
    }

    #[test]
    fn artifact_hash_requires_lowercase_64_hex() {
        let good = "a".repeat(64);
        assert!(is_artifact_hash(&good));
        assert!(!is_artifact_hash(&"A".repeat(64)));
        assert!(!is_artifact_hash("abc"));
    }

    #[test]
    fn dry_run_archive_leaves_sessions_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let path = write_session(
            &l,
            "20200101T000000000Z_old1.jsonl",
            "/p",
            Duration::from_secs(90 * 86_400),
        );
        let mut a = args();
        a.archive = true;
        a.cold_archive_after_days = Some(30);
        a.retain_newest_global = Some(0);
        a.retain_newest_per_cwd = Some(0);
        let report = execute(&l, &a, NOW).unwrap();
        let archive = report.archive.unwrap();
        assert_eq!(archive.scanned, 1);
        assert_eq!(archive.would_archive, 1);
        assert_eq!(archive.archived, 0);
        assert!(path.exists());
        assert!(!l.user_dir.join(retention::DB_FILE).exists());
    }

    #[test]
    fn apply_archive_moves_session_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let path = write_session(
            &l,
            "20200101T000000000Z_old2.jsonl",
            "/proj",
            Duration::from_secs(90 * 86_400),
        );
        let mut a = args();
        a.apply = true;
        a.archive = true;
        a.retain_newest_global = Some(0);
        a.retain_newest_per_cwd = Some(0);
        let report = execute(&l, &a, retention::now_ms()).unwrap();
        let archive = report.archive.unwrap();
        assert_eq!(archive.archived, 1, "errors: {:?}", archive.errors);
        assert!(!path.exists());
        let dest = l
            .sessions_dir()
            .join(ARCHIVE_DIR)
            .join(path.file_name().unwrap());
        assert!(dest.exists());
        let store = Store::open(&l.user_dir).unwrap();
        let record = store.archived("old2").unwrap().expect("record stored");
        assert_eq!(record.cwd, "/proj");
        assert_eq!(record.archived_path, dest.display().to_string());
        assert!(store.last_run_settings().unwrap().is_some());
    }

    #[test]
    fn restore_archived_returns_the_pair_or_keeps_it_together_in_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let sessions = l.sessions_dir();
        let archive = sessions.join(ARCHIVE_DIR);
        fs::create_dir_all(&archive).unwrap();
        let archived_pair = |name: &str| {
            let original = sessions.join(name);
            let dest = archive.join(name);
            fs::write(&dest, "{}\n").unwrap();
            let streams = crate::runtime::stream_journal::path(&dest);
            fs::write(&streams, "stream\n").unwrap();
            (original, dest, streams)
        };
        let (original, dest, streams) = archived_pair("20200101T000000000Z_back.jsonl");
        restore_archived(&dest, &original).unwrap();
        assert!(original.exists() && !dest.exists());
        assert!(crate::runtime::stream_journal::path(&original).exists());
        assert!(!streams.exists());

        // A directory at the original path makes the main rename fail; the stream journal must
        // then stay beside the archived main file rather than return alone.
        let (blocked, dest, streams) = archived_pair("20200101T000000000Z_blocked.jsonl");
        fs::create_dir(&blocked).unwrap();
        let error = restore_archived(&dest, &blocked).unwrap_err();
        assert!(error.starts_with("restoring"), "{error}");
        assert!(dest.exists());
        assert!(
            streams.exists(),
            "the stream journal must stay with its archived session"
        );
        assert!(!crate::runtime::stream_journal::path(&blocked).exists());
    }

    #[test]
    fn recent_and_protected_sessions_are_not_archived() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        write_session(
            &l,
            "20260101T000000000Z_fresh.jsonl",
            "/p",
            Duration::from_secs(10),
        );
        write_session(
            &l,
            "20200101T000000000Z_keep.jsonl",
            "/p",
            Duration::from_secs(90 * 86_400),
        );
        let mut a = args();
        a.archive = true;
        a.retain_newest_global = Some(1);
        a.retain_newest_per_cwd = Some(0);
        let archive = execute(&l, &a, retention::now_ms())
            .unwrap()
            .archive
            .unwrap();
        assert_eq!(archive.skipped_recent, 1);
        assert_eq!(archive.kept_newest_global, 1);
        assert_eq!(archive.would_archive, 0);
    }

    #[test]
    fn blobs_keep_referenced_and_remove_old_unreferenced() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(artifacts.join(LIFECYCLE_DIR)).unwrap();
        let kept = "b".repeat(64);
        let orphan = "c".repeat(64);
        let transitive = "d".repeat(64);
        fs::write(artifacts.join(&kept), "body").unwrap();
        fs::write(artifacts.join(&orphan), "body").unwrap();
        fs::write(artifacts.join(&transitive), "body").unwrap();
        fs::write(
            artifacts.join(LIFECYCLE_DIR).join(format!("{orphan}.json")),
            "{}",
        )
        .unwrap();
        for name in [&kept, &orphan, &transitive] {
            set_age(&artifacts.join(name), Duration::from_secs(3600));
        }
        // Referenced from a session; the referenced blob itself mentions `transitive`.
        let session_dir = l.sessions_dir();
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("20260101T000000000Z_s.jsonl"),
            format!("artifact://{kept}\n"),
        )
        .unwrap();
        fs::write(
            artifacts.join(&kept),
            format!("see artifact://{transitive}"),
        )
        .unwrap();

        let mut a = args();
        a.apply = true;
        a.blobs = true;
        let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
        assert_eq!(blobs.candidates, 1, "errors: {:?}", blobs.errors);
        assert_eq!(blobs.deleted, 1);
        assert!(artifacts.join(&kept).exists());
        assert!(artifacts.join(&transitive).exists());
        assert!(!artifacts.join(&orphan).exists());
        assert!(
            !artifacts
                .join(LIFECYCLE_DIR)
                .join(format!("{orphan}.json"))
                .exists()
        );
    }

    #[test]
    fn live_spool_is_kept_until_its_owner_finishes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(&artifacts).unwrap();
        let store = crate::tools::artifact::ArtifactStore::new(&artifacts)
            .unwrap()
            .with_limits(&crate::tools::artifact::ArtifactConfig {
                max_bytes: 64,
                head_bytes: 16,
            });
        let mut spool = store.spool().unwrap();
        // Past the head budget: the head stops changing while the tail keeps being written.
        spool.write_all(&[b'x'; 1000]).unwrap();
        let spool_files: Vec<PathBuf> = fs::read_dir(&artifacts)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(SPOOL_PREFIX))
            })
            .collect();
        assert_eq!(spool_files.len(), 2, "head and tail: {spool_files:?}");
        for path in &spool_files {
            set_age(path, Duration::from_secs(3600));
        }
        let mut a = args();
        a.apply = true;
        a.blobs = true;
        let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
        assert_eq!(blobs.deleted, 0, "errors: {:?}", blobs.errors);
        for path in &spool_files {
            assert!(path.exists(), "live spool file removed: {}", path.display());
        }
        spool.write_all(b"tail").unwrap();
        let uri = spool.finish().unwrap();
        let id = uri.trim_start_matches("artifact://");
        assert!(artifacts.join(id).exists());
    }

    #[test]
    fn abandoned_unclaimed_spool_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(&artifacts).unwrap();
        let head = artifacts.join(format!("{SPOOL_PREFIX}999-0"));
        let tail = head.with_extension("tail");
        fs::write(&head, "head").unwrap();
        fs::write(&tail, "tail").unwrap();
        set_age(&head, Duration::from_secs(3600));
        set_age(&tail, Duration::from_secs(3600));
        let mut a = args();
        a.apply = true;
        a.blobs = true;
        let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
        assert_eq!(blobs.deleted, 2, "errors: {:?}", blobs.errors);
        assert!(!head.exists() && !tail.exists());
    }

    #[test]
    fn failed_transitive_read_deletes_nothing_and_reports_incomplete_scan() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(&artifacts).unwrap();
        let unreadable = "a".repeat(64);
        let orphan = "c".repeat(64);
        // A directory where the referenced body should be: reading it fails with a real error,
        // not NotFound, so the scan must fail closed.
        fs::create_dir(artifacts.join(&unreadable)).unwrap();
        fs::write(artifacts.join(&orphan), "body").unwrap();
        set_age(&artifacts.join(&orphan), Duration::from_secs(3600));
        let session_dir = l.sessions_dir();
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("20260101T000000000Z_s.jsonl"),
            format!("artifact://{unreadable}\n"),
        )
        .unwrap();
        let mut a = args();
        a.apply = true;
        a.blobs = true;
        let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
        assert_eq!(blobs.deleted, 0);
        assert!(
            blobs
                .errors
                .iter()
                .any(|e| e.contains("reference scan incomplete") && e.contains(&unreadable)),
            "errors: {:?}",
            blobs.errors
        );
        assert!(artifacts.join(&orphan).exists());
    }

    #[test]
    fn referenced_body_that_is_absent_is_missing_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(&artifacts).unwrap();
        let absent = "9".repeat(64);
        let session_dir = l.sessions_dir();
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("20260101T000000000Z_m.jsonl"),
            format!("artifact://{absent}\n"),
        )
        .unwrap();
        let scan = collect_references(&l).unwrap();
        assert!(scan.refs.contains(&absent));
        assert_eq!(scan.missing, vec![absent]);
    }

    #[test]
    fn blobs_skip_fresh_unreferenced_files_inside_grace() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let artifacts = l.state_dir.join(ARTIFACTS_DIR);
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("e".repeat(64)), "new").unwrap();
        let mut a = args();
        a.blobs = true;
        let blobs = execute(&l, &a, retention::now_ms()).unwrap().blobs.unwrap();
        assert_eq!(blobs.candidates, 1);
        assert_eq!(blobs.would_delete, 0);
    }

    #[test]
    fn stale_prunes_dangling_records_beyond_keep_window() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let lifecycle = l.state_dir.join(ARTIFACTS_DIR).join(LIFECYCLE_DIR);
        fs::create_dir_all(&lifecycle).unwrap();
        let old = "f".repeat(64);
        let fresh_keep = "1".repeat(64);
        fs::write(lifecycle.join(format!("{old}.json")), "{}").unwrap();
        fs::write(lifecycle.join(format!("{fresh_keep}.json")), "{}").unwrap();
        set_age(
            &lifecycle.join(format!("{old}.json")),
            Duration::from_secs(90 * 86_400),
        );
        let mut a = args();
        a.apply = true;
        a.stale = true;
        a.stale_retain_newest = Some(1);
        let stale = execute(&l, &a, retention::now_ms()).unwrap().stale.unwrap();
        assert_eq!(stale.dangling_records, 2);
        assert_eq!(stale.kept_newest, 1);
        assert_eq!(stale.deleted, 1, "errors: {:?}", stale.errors);
        assert!(!lifecycle.join(format!("{old}.json")).exists());
    }

    #[test]
    fn wal_reports_size_in_dry_run_and_checkpoints_on_apply() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let db = l.user_dir.join("stats.sqlite3");
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        drop(conn);
        // Dry-run never checkpoints.
        let mut a = args();
        a.wal = true;
        let dry = execute(&l, &a, retention::now_ms()).unwrap().wal.unwrap();
        assert_eq!(dry.databases.len(), 1);
        assert!(!dry.checkpointed);
        a.apply = true;
        let applied = execute(&l, &a, retention::now_ms()).unwrap().wal.unwrap();
        assert!(applied.errors.is_empty(), "errors: {:?}", applied.errors);
        assert!(applied.databases[0].checkpointed);
        assert_eq!(wal_size(&db), 0);
    }

    #[test]
    fn json_report_uses_camel_case_keys() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        let report = execute(&l, &args(), NOW).unwrap();
        let json: Value = serde_json::to_value(&report).unwrap();
        assert!(json.get("stateDir").is_some());
        assert!(json.get("lockPath").is_some());
        assert_eq!(json["apply"], Value::Bool(false));
        assert!(json.get("archive").is_some());
        assert!(json.get("stale").is_none());
        assert!(json["settings"].get("coldArchiveAfterDays").is_some());
    }

    #[test]
    fn exit_code_reflects_errors() {
        let report = GcReport {
            state_dir: String::new(),
            lock_path: String::new(),
            apply: false,
            settings: applied_settings(&args()),
            stale: None,
            blobs: Some(BlobResult {
                errors: vec!["boom".into()],
                ..Default::default()
            }),
            archive: None,
            wal: None,
        };
        assert_eq!(report.errors(), vec!["boom"]);
    }
}
