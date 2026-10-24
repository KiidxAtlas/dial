//! Recurring and file-triggered prompt runs: `dial schedule`.
//!
//! Schedules are per project and live in private state outside the repository, so the agent cannot
//! create or edit them. `dial schedule run` is a foreground scheduler: it launches each due entry
//! as an ordinary `dial -p` child process (one at a time, because local compute is serialised),
//! logging its output. Entries are `readonly` unless they were added with `--yes` or an explicit
//! `--allow` list, since nobody is there to answer approval prompts. Cancellation first asks the
//! child controller to stop, then kills its attributed descendants and process group. Output is
//! drained without blocking and logs retain bounded prefixes, tails and omitted-part diagnostics.
//! Watch coverage is limited to non-ignored regular files; an unreadable or over-cap traversal is
//! an error for that entry only (reported, retried), never an unchanged fingerprint. Filesystem
//! events (`notify`) select which watch entries to re-fingerprint; without them the scheduler
//! polls. The schedules file is re-parsed when it changes; invalid entries are skipped.
use crate::{
    config::paths::Layout,
    tools::{code::diagnostics::Collector, exec::shell::ProcessGroup},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
type ProcessId = libc::pid_t;
#[cfg(windows)]
type ProcessId = u32;
const MIN_INTERVAL_SECS: u64 = 30;
const MAX_ENTRIES: usize = 64;
const MAX_PROMPT: usize = 16 * 1024;
const MAX_WATCHED_FILES: usize = 20_000;
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const DEBOUNCE_SECS: u64 = 3;
const CAPTURE_HEAD: usize = 128 * 1024;
const CAPTURE_TAIL: usize = 128 * 1024;
const CAPTURE_DIAGNOSTICS: usize = 64 * 1024;
const POLL: Duration = Duration::from_secs(2);
const IO_POLL: Duration = Duration::from_millis(10);
const TREE_POLL: Duration = Duration::from_millis(200);
const GRACEFUL_STOP: Duration = Duration::from_millis(300);
const CLEANUP_GRACE: Duration = Duration::from_secs(1);
const TREE_SCAN_GRACE: Duration = Duration::from_millis(200);
#[cfg(unix)]
const MAX_PROCESSES: usize = 65_536;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub prompt: String,
    /// Run this often.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    /// Run when files matching these globs (project-relative; no `/` matches at any depth) change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch: Vec<String>,
    /// Tools pre-approved for the run (`--allow`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// `readonly` or `auto` (everything approved).
    pub approval: String,
    pub created: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    entries: Vec<Entry>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `30s`, `5m`, `2h`, `1d`; a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let n: u64 = number
        .parse()
        .with_context(|| format!("invalid duration {text:?}"))?;
    let unit = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("invalid duration unit in {text:?}; use s, m, h or d"),
    };
    n.checked_mul(unit).context("duration too large")
}

impl Entry {
    /// Build a schedule identically for CLI and interactive callers. Store::add assigns a
    /// missing id and validates triggers before publishing it.
    pub fn new(
        id: Option<String>,
        prompt: String,
        every: Option<&str>,
        watch: Vec<String>,
        allow: Vec<String>,
        approve_all: bool,
    ) -> Result<Self> {
        let mut tools = Vec::new();
        for value in allow {
            for tool in value.split(',').map(str::trim) {
                ensure!(
                    !tool.is_empty() && tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "invalid --allow tool name"
                );
                if !tools.iter().any(|existing| existing == tool) {
                    tools.push(tool.to_owned());
                }
            }
        }
        Ok(Self {
            id: id.unwrap_or_default(),
            prompt,
            every_secs: every.map(parse_duration).transpose()?,
            watch,
            allow: tools,
            approval: if approve_all { "auto" } else { "readonly" }.into(),
            created: now(),
        })
    }

    pub fn access_description(&self) -> String {
        if self.approval == "auto" {
            "all tools approved".into()
        } else if self.allow.is_empty() {
            "read-only".into()
        } else {
            format!("read-only + {}", self.allow.join(","))
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 32
                && self
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid schedule id"
        );
        ensure!(
            !self.prompt.trim().is_empty() && self.prompt.len() <= MAX_PROMPT,
            "prompt must be 1..{MAX_PROMPT} bytes"
        );
        ensure!(
            self.every_secs.is_some() || !self.watch.is_empty(),
            "a schedule needs --every or --watch"
        );
        if let Some(every) = self.every_secs {
            ensure!(
                every >= MIN_INTERVAL_SECS,
                "interval must be at least {MIN_INTERVAL_SECS} seconds"
            );
        }
        ensure!(
            self.watch.len() <= 16
                && self
                    .watch
                    .iter()
                    .all(|p| !p.trim().is_empty() && p.len() <= 256),
            "at most 16 watch patterns of 1..256 bytes"
        );
        ensure!(
            matches!(self.approval.as_str(), "readonly" | "auto"),
            "approval must be readonly or auto"
        );
        ensure!(
            self.allow
                .iter()
                .all(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
            "invalid --allow tool name"
        );
        watch_set(&self.watch)?;
        Ok(())
    }
}

fn watch_set(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let anchored = pattern.trim_start_matches("./");
        let pattern = if anchored.contains('/') {
            anchored.to_owned()
        } else {
            format!("**/{anchored}")
        };
        builder.add(
            globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid watch pattern {pattern:?}"))?,
        );
    }
    Ok(builder.build()?)
}

/// A digest of path, size and modification time of every watched, non-ignored regular file.
/// Refuse incomplete coverage, including traversal/metadata errors and more than 20,000 matches.
pub fn fingerprint(root: &Path, patterns: &[String]) -> Result<[u8; 32]> {
    let set = watch_set(patterns)?;
    fingerprints(root, &[&set]).pop().expect("one watch set")
}

/// Share directory traversal and metadata reads, but retain coverage limits and failures per set.
fn fingerprints(root: &Path, sets: &[&globset::GlobSet]) -> Vec<Result<[u8; 32]>> {
    fingerprints_with_walk(root, sets, MAX_WATCHED_FILES, || {
        ignore::WalkBuilder::new(root).follow_links(false).build()
    })
}

fn fingerprints_with_walk(
    root: &Path,
    sets: &[&globset::GlobSet],
    limit: usize,
    walk: impl FnOnce() -> ignore::Walk,
) -> Vec<Result<[u8; 32]>> {
    let mut files: Vec<Result<Vec<_>>> = sets.iter().map(|_| Ok(Vec::new())).collect();
    if sets.is_empty() {
        return Vec::new();
    }
    let coverage = (|| -> Result<()> {
        for entry in walk() {
            let entry = entry.context("watch coverage incomplete: traversing project files")?;
            if let Some(error) = entry.error() {
                bail!(
                    "watch coverage incomplete: ignore rules for {}: {error}",
                    entry.path().display()
                );
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .context("watch coverage incomplete: path outside project")?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let matching: Vec<_> = sets
                .iter()
                .enumerate()
                .filter_map(|(index, set)| {
                    (files[index].is_ok() && set.is_match(relative)).then_some(index)
                })
                .collect();
            if matching.is_empty() {
                continue;
            }
            let metadata = (|| -> Result<_> {
                let meta = entry.metadata().with_context(|| {
                    format!(
                        "watch coverage incomplete: reading {}",
                        entry.path().display()
                    )
                })?;
                let modified = meta
                    .modified()
                    .with_context(|| {
                        format!(
                            "watch coverage incomplete: modification time of {}",
                            entry.path().display()
                        )
                    })?
                    .duration_since(UNIX_EPOCH)
                    .context("watch coverage incomplete: modification time predates Unix epoch")?
                    .as_nanos();
                Ok((relative.to_path_buf(), meta.len(), modified))
            })();
            for index in matching {
                let matched = files[index].as_mut().expect("active watch set");
                if matched.len() >= limit {
                    files[index] = Err(anyhow::anyhow!(
                        "watch coverage incomplete: more than {limit} matching files; narrow --watch"
                    ));
                } else {
                    match &metadata {
                        Ok(file) => matched.push(file.clone()),
                        Err(error) => files[index] = Err(anyhow::anyhow!("{error:#}")),
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = coverage {
        for result in &mut files {
            if result.is_ok() {
                *result = Err(anyhow::anyhow!("{error:#}"));
            }
        }
    }
    files
        .into_iter()
        .map(|files| files.map(hash_files))
        .collect()
}

fn hash_files(mut files: Vec<(PathBuf, u64, u128)>) -> [u8; 32] {
    files.sort();
    let mut hash = Sha256::new();
    for (path, len, modified) in files {
        let path = path.to_string_lossy();
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(len.to_le_bytes());
        hash.update(modified.to_le_bytes());
    }
    hash.finalize().into()
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(layout: &Layout) -> Result<Self> {
        layout.ensure_project_outside_storage()?;
        Ok(Self {
            dir: layout.state_dir.clone(),
        })
    }
    fn path(&self) -> PathBuf {
        self.dir.join("schedules.toml")
    }
    fn state_path(&self) -> PathBuf {
        self.dir.join("schedule-state.toml")
    }
    fn baseline_path(&self) -> PathBuf {
        self.dir.join("schedule-watch.toml")
    }
    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join("schedule-logs").join(format!("{id}.log"))
    }

    pub fn load(&self) -> Result<Vec<Entry>> {
        match fs::read_to_string(self.path()) {
            Ok(text) => {
                let entries = toml::from_str::<File>(&text)
                    .context("parsing schedules")?
                    .entries;
                ensure!(
                    entries.len() <= MAX_ENTRIES,
                    "at most {MAX_ENTRIES} schedules"
                );
                let mut ids = HashSet::new();
                for entry in &entries {
                    entry
                        .validate()
                        .with_context(|| format!("invalid schedule {}", entry.id))?;
                    ensure!(ids.insert(&entry.id), "duplicate schedule {}", entry.id);
                }
                Ok(entries)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).context("reading schedules"),
        }
    }

    /// The scheduler's view: one invalid or duplicate entry is reported and skipped instead of
    /// stopping every other schedule. `add`/`remove` still use the strict `load`.
    fn load_lenient(&self) -> Result<(Vec<Entry>, Vec<String>)> {
        let text = match fs::read_to_string(self.path()) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(e) => return Err(e).context("reading schedules"),
        };
        let table = toml::from_str::<toml::Table>(&text).context("parsing schedules")?;
        let mut problems = Vec::new();
        for key in table.keys().filter(|key| key.as_str() != "entries") {
            problems.push(format!("unknown schedules.toml key {key:?} ignored"));
        }
        let raw = match table.get("entries") {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => items.clone(),
            Some(_) => bail!("schedules.toml `entries` must be an array of tables"),
        };
        let mut entries: Vec<Entry> = Vec::new();
        let mut ids = HashSet::new();
        for (index, value) in raw.into_iter().enumerate() {
            if entries.len() >= MAX_ENTRIES {
                problems.push(format!(
                    "schedules beyond the first {MAX_ENTRIES} are ignored"
                ));
                break;
            }
            let entry = match value.try_into::<Entry>() {
                Ok(entry) => entry,
                Err(error) => {
                    problems.push(format!("schedule #{} ignored: {error}", index + 1));
                    continue;
                }
            };
            if let Err(error) = entry.validate() {
                problems.push(format!("schedule {} ignored: {error:#}", entry.id));
                continue;
            }
            if !ids.insert(entry.id.clone()) {
                problems.push(format!("duplicate schedule {} ignored", entry.id));
                continue;
            }
            entries.push(entry);
        }
        Ok((entries, problems))
    }

    /// Cheap change detection for the schedules file: (length, modification time).
    fn stamp(&self) -> Option<(u64, SystemTime)> {
        let meta = fs::metadata(self.path()).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    }

    /// Read-modify-write under an exclusive lock so concurrent `add`/`remove` calls cannot lose updates.
    fn update<T>(&self, change: impl FnOnce(&mut Vec<Entry>) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("schedules.lock"))?;
        lock.lock_exclusive()?;
        let mut entries = self.load()?;
        let value = change(&mut entries)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(toml::to_string(&File { entries })?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.path()).map_err(|e| e.error)?;
        Ok(value)
    }

    pub fn add(&self, mut entry: Entry) -> Result<String> {
        self.update(|entries| {
            ensure!(
                entries.len() < MAX_ENTRIES,
                "at most {MAX_ENTRIES} schedules"
            );
            if entry.id.is_empty() {
                let mut n = 1;
                while entries.iter().any(|e| e.id == format!("s{n}")) {
                    n += 1;
                }
                entry.id = format!("s{n}");
            }
            ensure!(
                !entries.iter().any(|e| e.id == entry.id),
                "schedule {} already exists",
                entry.id
            );
            entry.validate()?;
            let id = entry.id.clone();
            entries.push(entry);
            Ok(id)
        })
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.update(|entries| {
            let before = entries.len();
            entries.retain(|e| e.id != id);
            ensure!(entries.len() < before, "no schedule {id:?}");
            Ok(())
        })
    }

    pub fn last_runs(&self) -> HashMap<String, u64> {
        self.load_state()
    }
    fn load_state(&self) -> HashMap<String, u64> {
        fs::read_to_string(self.state_path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn load_baselines(&self) -> HashMap<String, SavedBaseline> {
        fs::read_to_string(self.baseline_path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn load_scheduler(&self) -> Scheduler {
        let mut scheduler = Scheduler {
            last_run: self.load_state(),
            ..Default::default()
        };
        for (id, saved) in self.load_baselines() {
            let fingerprint = match saved {
                SavedBaseline::Legacy(fp) => fp,
                SavedBaseline::Bound {
                    fingerprint,
                    definition,
                } => {
                    scheduler.definitions.insert(id.clone(), definition);
                    fingerprint
                }
            };
            scheduler.watches.insert(
                id,
                Watch {
                    baseline: Some(fingerprint),
                    pending: None,
                },
            );
        }
        scheduler
    }
    fn write_atomic(&self, path: PathBuf, text: &str) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
    fn save_state(&self, scheduler: &Scheduler) {
        let baselines: HashMap<String, SavedBaseline> = scheduler
            .watches
            .iter()
            .filter_map(|(id, watch)| {
                Some((
                    id.clone(),
                    SavedBaseline::Bound {
                        fingerprint: watch.baseline?,
                        definition: scheduler.definitions.get(id)?.clone(),
                    },
                ))
            })
            .collect();
        let write = || -> Result<()> {
            // A torn state file would make every interval schedule look overdue, so writes are atomic.
            self.write_atomic(self.state_path(), &toml::to_string(&scheduler.last_run)?)?;
            self.write_atomic(self.baseline_path(), &toml::to_string(&baselines)?)?;
            Ok(())
        };
        if let Err(error) = write() {
            eprintln!("dial schedule: could not save run state: {error:#}");
        }
    }
}

pub fn describe(entry: &Entry, last_run: Option<u64>) -> String {
    let mut triggers = Vec::new();
    if let Some(every) = entry.every_secs {
        triggers.push(format!("every {every}s"));
    }
    if !entry.watch.is_empty() {
        triggers.push(format!("on change of {}", entry.watch.join(", ")));
    }
    let access = entry.access_description();
    let last = last_run.map_or("never run".to_owned(), |t| {
        format!("last run {}s ago", now().saturating_sub(t))
    });
    format!(
        "{}  {}  [{access}; {last}]\n    {}",
        entry.id,
        triggers.join(" and "),
        entry.prompt.lines().next().unwrap_or("")
    )
}

/// Why an entry fired.
#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    Interval,
    Changed,
}

// Old fingerprint-only files remain readable; their definitions are bound on first load.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SavedBaseline {
    Legacy([u8; 32]),
    Bound {
        fingerprint: [u8; 32],
        definition: Entry,
    },
}

#[derive(Default)]
struct Watch {
    baseline: Option<[u8; 32]>,
    pending: Option<([u8; 32], u64)>,
}

/// Decides what is due. Time and file fingerprints are inputs so the policy is testable.
#[derive(Default)]
pub struct Scheduler {
    pub last_run: HashMap<String, u64>,
    watches: HashMap<String, Watch>,
    definitions: HashMap<String, Entry>,
}

impl Scheduler {
    fn reconcile(&mut self, fresh: &[Entry]) {
        self.watches.retain(|id, _| {
            fresh.iter().any(|entry| {
                entry.id == *id && self.definitions.get(id).is_none_or(|old| old == entry)
            })
        });
        self.last_run
            .retain(|id, _| fresh.iter().any(|entry| entry.id == *id));
        self.definitions = fresh
            .iter()
            .map(|entry| (entry.id.clone(), entry.clone()))
            .collect();
    }

    pub fn due(
        &mut self,
        entry: &Entry,
        now: u64,
        fingerprint: impl FnOnce() -> Option<[u8; 32]>,
    ) -> Option<Reason> {
        if let Some(every) = entry.every_secs {
            let since = self
                .last_run
                .get(&entry.id)
                .copied()
                .unwrap_or(entry.created)
                .max(entry.created);
            if now >= since.saturating_add(every) {
                return Some(Reason::Interval);
            }
        }
        if entry.watch.is_empty() {
            return None;
        }
        let fp = fingerprint()?;
        let watch = self.watches.entry(entry.id.clone()).or_default();
        let Some(baseline) = watch.baseline else {
            watch.baseline = Some(fp);
            return None;
        };
        if fp == baseline {
            watch.pending = None;
            return None;
        }
        match watch.pending {
            // Changes must settle so a save-all or a build is one run, not many.
            Some((seen, since)) if seen == fp && now >= since + DEBOUNCE_SECS => {
                Some(Reason::Changed)
            }
            Some((seen, _)) if seen == fp => None,
            _ => {
                watch.pending = Some((fp, now));
                None
            }
        }
    }

    /// Record a finished run. The new fingerprint becomes the baseline, so the run's own edits do not retrigger it.
    pub fn ran(&mut self, entry: &Entry, now: u64, fingerprint: Option<[u8; 32]>) {
        self.last_run.insert(entry.id.clone(), now);
        if !entry.watch.is_empty() {
            self.watches.insert(
                entry.id.clone(),
                Watch {
                    baseline: fingerprint,
                    pending: None,
                },
            );
        }
    }
}

fn append_log(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rotate = match fs::metadata(path) {
        Ok(meta) => meta.len().saturating_add(text.len() as u64) > MAX_LOG_BYTES,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspecting schedule log"),
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!rotate)
        .truncate(rotate)
        .open(path)
        .with_context(|| format!("opening schedule log {}", path.display()))?;
    file.write_all(text.as_bytes())
        .context("writing schedule log")
}

struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
    diagnostics: Collector,
}
impl Capture {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
            diagnostics: Collector::new(CAPTURE_DIAGNOSTICS),
        }
    }
    fn push(&mut self, mut bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head = bytes
            .len()
            .min(CAPTURE_HEAD.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..head]);
        bytes = &bytes[head..];
        self.tail.extend(bytes);
        let excess = self.tail.len().saturating_sub(CAPTURE_TAIL);
        if excess > 0 {
            let (front, back) = self.tail.as_slices();
            let first = excess.min(front.len());
            self.diagnostics.feed(&front[..first]);
            self.diagnostics.feed(&back[..excess - first]);
            self.tail.drain(..excess);
        }
    }
    fn finish(mut self) -> String {
        self.diagnostics.finish();
        let dropped = self.total.saturating_sub(self.head.len() + self.tail.len());
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if dropped > 0 {
            let (diagnostics, lines, overflow) = self.diagnostics.kept();
            text.push_str(&format!("\n[... {dropped} bytes omitted; {lines} diagnostic line(s) retained, {overflow} additional diagnostic line(s) omitted ...]\n"));
            text.push_str(diagnostics);
        }
        text.push_str(&String::from_utf8_lossy(self.tail.make_contiguous()));
        text
    }
}

fn nonblocking(pipe: &impl crate::platform::io::Pipe) -> io::Result<()> {
    crate::platform::io::set_nonblocking(pipe)
}

fn drain(
    stream: &mut (impl Read + crate::platform::io::Pipe),
    capture: &mut Capture,
) -> io::Result<bool> {
    let mut bytes = [0u8; 8192];
    for _ in 0..16 {
        match crate::platform::io::read_nonblocking(stream, &mut bytes) {
            Ok(0) => return Ok(true),
            Ok(count) => capture.push(&bytes[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct ProcessInfo {
    pid: ProcessId,
    parent: ProcessId,
    started: (u64, u64),
}

#[cfg(target_os = "macos")]
fn process_info(pid: libc::pid_t) -> Result<Option<ProcessInfo>> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info);
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as i32,
        )
    };
    if read == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("reading process {pid} identity"));
    }
    ensure!(
        read as usize == size,
        "incomplete identity for process {pid}"
    );
    Ok(Some(ProcessInfo {
        pid,
        parent: info.pbi_ppid as libc::pid_t,
        started: (info.pbi_start_tvsec, info.pbi_start_tvusec),
    }))
}

#[cfg(target_os = "linux")]
fn process_info(pid: libc::pid_t) -> Result<Option<ProcessInfo>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
            return Ok(None);
        }
        Err(error) => return Err(error).with_context(|| format!("reading process {pid} identity")),
    };
    // The comm field can contain whitespace and parentheses. Fields after its final ')' start at 3.
    let fields = stat
        .rsplit_once(") ")
        .context("malformed /proc process identity")?
        .1;
    let mut fields = fields.split_whitespace();
    let parent = fields.nth(1).context("missing process parent")?.parse()?;
    let started = fields
        .nth(17)
        .context("missing process start time")?
        .parse()?;
    Ok(Some(ProcessInfo {
        pid,
        parent,
        started: (started, 0),
    }))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_info(_: libc::pid_t) -> Result<Option<ProcessInfo>> {
    bail!("scheduler owned-process cancellation requires macOS or Linux")
}

#[cfg(unix)]
fn scan_deadline(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "owned-process coverage incomplete: process scan deadline exceeded"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn process_snapshot(
    deadline: Instant,
    roots: impl Iterator<Item = libc::pid_t>,
) -> Result<Vec<ProcessInfo>> {
    // libproc can enumerate immediate children; do not scan unrelated protected system processes.
    let mut pids = vec![0; MAX_PROCESSES];
    let mut pending: VecDeque<_> = roots.collect();
    let mut visited = HashSet::new();
    let mut processes = Vec::new();
    while let Some(pid) = pending.pop_front() {
        scan_deadline(deadline)?;
        if !visited.insert(pid) {
            continue;
        }
        let Some(info) = process_info(pid)? else {
            continue;
        };
        ensure!(
            processes.len() < MAX_PROCESSES,
            "owned-process coverage incomplete: process tree exceeds {MAX_PROCESSES}"
        );
        processes.push(info);
        let count = unsafe {
            libc::proc_listchildpids(
                pid,
                pids.as_mut_ptr().cast(),
                (pids.len() * std::mem::size_of::<libc::pid_t>()) as i32,
            )
        };
        ensure!(
            count >= 0,
            "enumerating children of {pid}: {}",
            io::Error::last_os_error()
        );
        ensure!(
            (count as usize) < pids.len(),
            "owned-process coverage incomplete: child list exceeds {MAX_PROCESSES}"
        );
        pending.extend(
            pids[..count as usize]
                .iter()
                .copied()
                .filter(|pid| *pid > 0),
        );
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn process_snapshot(
    deadline: Instant,
    _roots: impl Iterator<Item = libc::pid_t>,
) -> Result<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").context("enumerating owned processes")? {
        scan_deadline(deadline)?;
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        ensure!(
            processes.len() < MAX_PROCESSES,
            "owned-process coverage incomplete: process list exceeds {MAX_PROCESSES}"
        );
        if let Some(info) = process_info(pid)? {
            processes.push(info);
        }
    }
    Ok(processes)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_snapshot(_: Instant, _: impl Iterator<Item = libc::pid_t>) -> Result<Vec<ProcessInfo>> {
    bail!("scheduler owned-process cancellation requires macOS or Linux")
}

#[cfg(unix)]
struct OwnedProcess {
    info: ProcessInfo,
    #[cfg(target_os = "linux")]
    handle: OwnedFd,
}
#[cfg(unix)]
impl OwnedProcess {
    fn claim(info: ProcessInfo) -> Result<Option<Self>> {
        #[cfg(target_os = "linux")]
        let handle = {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, info.pid, 0) };
            if fd == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(None);
                }
                return Err(error).context("scheduler requires pidfd ownership (Linux 5.3+)");
            }
            unsafe { OwnedFd::from_raw_fd(fd as RawFd) }
        };
        if process_info(info.pid)?.is_none_or(|current| current.started != info.started) {
            return Ok(None);
        }
        Ok(Some(Self {
            info,
            #[cfg(target_os = "linux")]
            handle,
        }))
    }
    fn signal(&self, signal: i32) -> Result<()> {
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.handle.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            ) as i32
        };
        #[cfg(not(target_os = "linux"))]
        let result = {
            // macOS has no pidfd. Revalidate the birth time before every signal; never use names.
            if process_info(self.info.pid)?
                .is_none_or(|current| current.started != self.info.started)
            {
                return Ok(());
            }
            unsafe { libc::kill(self.info.pid, signal) }
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error)
                    .with_context(|| format!("signalling owned process {}", self.info.pid));
            }
        }
        Ok(())
    }
}

/// Track descendants while the root PID is reserved by ProcessGroup::exited (WNOWAIT).
/// A known child remains owned after reparenting, but a reused PID never acquires that ownership.
#[cfg(unix)]
struct OwnedTree {
    root: ProcessInfo,
    processes: HashMap<libc::pid_t, OwnedProcess>,
}
#[cfg(unix)]
impl OwnedTree {
    fn new(root: libc::pid_t) -> Result<Self> {
        let root =
            process_info(root)?.context("scheduled child disappeared before claiming ownership")?;
        Ok(Self {
            root,
            processes: HashMap::new(),
        })
    }
    fn refresh(&mut self, deadline: Instant) -> Result<bool> {
        let snapshot = process_snapshot(
            deadline,
            std::iter::once(self.root.pid).chain(self.processes.keys().copied()),
        )?;
        let mut children: HashMap<libc::pid_t, Vec<ProcessInfo>> = HashMap::new();
        let mut parents = VecDeque::from([self.root.pid]);
        let mut live_owned = HashSet::new();
        for info in snapshot {
            scan_deadline(deadline)?;
            if self
                .processes
                .get(&info.pid)
                .is_some_and(|owned| owned.info.started == info.started)
            {
                live_owned.insert(info.pid);
                parents.push_back(info.pid);
            }
            children.entry(info.parent).or_default().push(info);
        }
        // Release dead pidfds rather than accumulating identities during unlimited healthy runs.
        self.processes.retain(|pid, _| live_owned.contains(pid));
        let mut visited = HashSet::new();
        let mut added = false;
        while let Some(parent) = parents.pop_front() {
            scan_deadline(deadline)?;
            if !visited.insert(parent) {
                continue;
            }
            let expected = if parent == self.root.pid {
                self.root
            } else {
                let Some(owned) = self.processes.get(&parent) else {
                    continue;
                };
                owned.info
            };
            // Revalidate each attributed parent before claiming its snapshot's children.
            if process_info(parent)?.is_none_or(|current| current.started != expected.started) {
                continue;
            }
            for info in children.remove(&parent).unwrap_or_default() {
                if info.pid == self.root.pid {
                    continue;
                }
                let known = self
                    .processes
                    .get(&info.pid)
                    .is_some_and(|owned| owned.info.started == info.started);
                if !known && let Some(owned) = OwnedProcess::claim(info)? {
                    self.processes.insert(info.pid, owned);
                    added = true;
                }
                if self
                    .processes
                    .get(&info.pid)
                    .is_some_and(|owned| owned.info.started == info.started)
                {
                    parents.push_back(info.pid);
                }
            }
        }
        Ok(added)
    }
    fn signal_all(&self, signal: i32, failures: &mut Vec<String>) {
        let mut omitted = 0;
        for owned in self.processes.values() {
            if let Err(error) = owned.signal(signal) {
                if failures.len() < 32 {
                    failures.push(format!("{error:#}"));
                } else {
                    omitted += 1;
                }
            }
        }
        if omitted > 0 && failures.len() == 32 {
            failures.push(format!(
                "{omitted} additional owned-process signal failures omitted"
            ));
        }
    }
    fn terminate(&mut self, deadline: Instant, failures: &mut Vec<String>) {
        // Freeze attributed processes before the last descendant walk, closing ordinary fork races.
        // Kill only those identities, not their unrelated group peers.
        self.signal_all(libc::SIGSTOP, failures);
        loop {
            match self.refresh(deadline) {
                Ok(added) => {
                    self.signal_all(libc::SIGSTOP, failures);
                    if !added {
                        break;
                    }
                }
                Err(error) => {
                    failures.push(format!(
                        "owned-process cleanup coverage incomplete: {error:#}"
                    ));
                    break;
                }
            }
        }
        self.signal_all(libc::SIGKILL, failures);
    }
}

#[cfg(windows)]
struct OwnedTree(u32);
#[cfg(windows)]
impl OwnedTree {
    fn new(root: u32) -> Result<Self> {
        ensure!(
            crate::platform::process::is_group_leader(root),
            "scheduled child is not assigned to a Windows Job Object"
        );
        Ok(Self(root))
    }
    fn refresh(&mut self, _: Instant) -> Result<bool> {
        ensure!(
            crate::platform::process::is_group_leader(self.0),
            "scheduled process Job Object ownership was lost"
        );
        Ok(false)
    }
    fn terminate(&mut self, _: Instant, failures: &mut Vec<String>) {
        if let Err(error) = crate::platform::process::kill_group(self.0) {
            failures.push(format!("terminating scheduled Job Object: {error}"));
        }
    }
}

#[cfg(unix)]
fn poll_pipes(stdout: RawFd, stderr: RawFd) -> io::Result<()> {
    let mut pipes = [
        libc::pollfd {
            fd: stdout,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    if unsafe {
        libc::poll(
            pipes.as_mut_ptr(),
            pipes.len() as libc::nfds_t,
            IO_POLL.as_millis() as i32,
        )
    } == -1
    {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn poll_pipes<A: crate::platform::io::Pipe, B: crate::platform::io::Pipe>(
    stdout: &A,
    stderr: &B,
) -> io::Result<()> {
    crate::platform::io::wait_readable(stdout, IO_POLL)?;
    crate::platform::io::wait_readable(stderr, IO_POLL)?;
    Ok(())
}
fn request_schedule_cancel(pid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        crate::platform::process::terminate_group(pid)
    }
}

enum RunFailure {
    Start(anyhow::Error),
    Active(anyhow::Error),
}

/// Run one ordinary Dial child. A positive configured deadline covers its active lifetime;
/// zero remains unlimited. Stop is checked during output/wait, not only between schedules.
fn run_entry(
    exe: &Path,
    root: &Path,
    entry: &Entry,
    log: &Path,
    timeout_secs: u64,
    stop: &std::sync::atomic::AtomicBool,
) -> std::result::Result<Option<i32>, RunFailure> {
    use std::sync::atomic::Ordering;
    if stop.load(Ordering::Acquire) {
        return Ok(None);
    }
    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let mut command = Command::new(exe);
    command
        .arg("-C")
        .arg(root)
        .args(["-p", "--output-format", "json"]);
    if entry.approval == "auto" {
        command.arg("-y");
    } else if !entry.allow.is_empty() {
        // Named tools need ask mode: --allow never overrides readonly; no terminal approves others.
        command.args(["--approval", "ask", "--allow", &entry.allow.join(",")]);
    } else {
        command.args(["--approval", "readonly"]);
    }
    command
        .arg("--")
        .arg(&entry.prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = crate::platform::process::spawn_group(&mut command)
        .with_context(|| format!("starting {}", exe.display()))
        .map_err(RunFailure::Start)?;
    let mut group = ProcessGroup::new(child);
    let mut stdout = group.child.stdout.take().expect("piped schedule stdout");
    let mut stderr = group.child.stderr.take().expect("piped schedule stderr");
    let mut out = Capture::new();
    let mut err = Capture::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut failures = Vec::new();
    let mut tree = match OwnedTree::new(group.child.id() as ProcessId) {
        Ok(tree) => Some(tree),
        Err(error) => {
            failures.push(format!("claiming scheduled process: {error:#}"));
            None
        }
    };
    if let Err(error) = nonblocking(&stdout) {
        failures.push(format!(
            "configuring schedule stdout: {error}; output capture incomplete"
        ));
        out_done = true;
    }
    if let Err(error) = nonblocking(&stderr) {
        failures.push(format!(
            "configuring schedule stderr: {error}; output capture incomplete"
        ));
        err_done = true;
    }
    let mut next_tree_scan = started;
    let mut stopping = None;
    let mut stop_reason = None;
    while failures.is_empty() {
        let at = Instant::now();
        if stopping.is_none()
            && (stop.load(Ordering::Acquire)
                || (!timeout.is_zero() && started.elapsed() >= timeout))
        {
            stop_reason = Some(if stop.load(Ordering::Acquire) {
                "cancelled by the user".to_owned()
            } else {
                format!("timed out after {timeout_secs}s")
            });
            // Capture ownership before graceful cancellation can reparent the child's descendants.
            if let Some(tree) = tree.as_mut()
                && let Err(error) = tree.refresh(at + TREE_SCAN_GRACE)
            {
                failures.push(format!("owned-process coverage incomplete: {error:#}"));
                break;
            }
            // Ask Dial to cancel its own independent tool groups and write final diagnostics first.
            if let Err(error) = request_schedule_cancel(group.child.id()) {
                failures.push(format!("requesting schedule cancellation: {error}"));
                break;
            }
            stopping = Some(Instant::now());
        }
        if at >= next_tree_scan {
            if let Some(tree) = tree.as_mut() {
                let mut deadline = at + TREE_SCAN_GRACE;
                if timeout_secs > 0 && stopping.is_none() {
                    deadline = deadline.min(started + timeout);
                }
                if let Err(error) = tree.refresh(deadline) {
                    failures.push(format!("owned-process coverage incomplete: {error:#}"));
                    break;
                }
            }
            next_tree_scan = at + TREE_POLL;
        }
        if !out_done {
            match drain(&mut stdout, &mut out) {
                Ok(done) => out_done = done,
                Err(error) => {
                    failures.push(format!("reading schedule stdout: {error}"));
                    break;
                }
            }
        }
        if !err_done {
            match drain(&mut stderr, &mut err) {
                Ok(done) => err_done = done,
                Err(error) => {
                    failures.push(format!("reading schedule stderr: {error}"));
                    break;
                }
            }
        }
        match group.exited() {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                failures.push(format!("observing schedule exit: {error}"));
                break;
            }
        }
        if stopping.is_some_and(|since| since.elapsed() >= GRACEFUL_STOP) {
            break;
        }
        #[cfg(unix)]
        let waited = poll_pipes(
            if out_done { -1 } else { stdout.as_raw_fd() },
            if err_done { -1 } else { stderr.as_raw_fd() },
        );
        #[cfg(windows)]
        let waited = poll_pipes(&stdout, &stderr);
        if let Err(error) = waited {
            failures.push(format!("waiting for schedule output: {error}"));
        }
    }
    // Reserve the root PID until all attributed descendants have been stopped and killed.
    #[cfg(unix)]
    unsafe {
        libc::killpg(group.child.id() as libc::pid_t, libc::SIGSTOP)
    };
    if let Some(tree) = tree.as_mut() {
        tree.terminate(Instant::now() + TREE_SCAN_GRACE, &mut failures);
    }
    group.terminate();
    let cleanup = Instant::now();
    let mut status: Option<ExitStatus> = None;
    let mut reaped = false;
    while cleanup.elapsed() < CLEANUP_GRACE {
        if !out_done {
            match drain(&mut stdout, &mut out) {
                Ok(done) => out_done = done,
                Err(error) => {
                    failures.push(format!("reading final schedule stdout: {error}"));
                    out_done = true;
                }
            }
        }
        if !err_done {
            match drain(&mut stderr, &mut err) {
                Ok(done) => err_done = done,
                Err(error) => {
                    failures.push(format!("reading final schedule stderr: {error}"));
                    err_done = true;
                }
            }
        }
        if !reaped {
            match group.exited() {
                Ok(true) => {
                    match group.child.wait() {
                        Ok(exit) => status = Some(exit),
                        Err(error) => failures.push(format!("reaping schedule: {error}")),
                    }
                    reaped = true;
                }
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    failures.push("schedule status unavailable: child was already reaped".into());
                    reaped = true;
                }
                Err(error) => {
                    failures.push(format!("observing final schedule exit: {error}"));
                    break;
                }
                Ok(false) => {}
            }
        }
        if reaped && out_done && err_done {
            break;
        }
        #[cfg(unix)]
        let waited = poll_pipes(
            if out_done { -1 } else { stdout.as_raw_fd() },
            if err_done { -1 } else { stderr.as_raw_fd() },
        );
        #[cfg(windows)]
        let waited = poll_pipes(&stdout, &stderr);
        if let Err(error) = waited {
            failures.push(format!("waiting for final schedule output: {error}"));
            break;
        }
    }
    if !out_done || !err_done {
        failures.push(
            "output pipes did not close within the 1s cleanup grace; captured output is partial"
                .into(),
        );
    }
    // No pipe threads or joins. A kernel-stuck child cannot block the scheduler in wait()/Drop.
    if !reaped {
        failures.push(
            "child did not exit within the 1s cleanup grace; status unknown and reaping deferred"
                .into(),
        );
        std::thread::spawn(move || drop(group));
    } else {
        drop(group);
    }
    let code = status.and_then(|status| status.code());
    let actual_status = status.map_or("unknown".to_owned(), |status| {
        if let Some(signal) = crate::platform::process::exit_signal(&status) {
            format!("signal {signal}")
        } else {
            format!(
                "exit {}",
                status
                    .code()
                    .map_or("unknown".to_owned(), |code| code.to_string())
            )
        }
    });
    let mut text = format!(
        "--- {} {actual_status} after {:.3}s\n",
        now(),
        started.elapsed().as_secs_f64()
    );
    text.push_str(out.finish().trim_end());
    let stderr = err.finish();
    if !stderr.is_empty() {
        text.push_str(&format!("\n[stderr]\n{}", stderr.trim_end()));
    }
    if let Some(reason) = stop_reason {
        let cleanup = if failures.is_empty() {
            "attributed process tree terminated"
        } else {
            "cleanup/capture errors follow"
        };
        text.push_str(&format!("\n[{reason}; {cleanup}]"));
    }
    for failure in &failures {
        text.push_str(&format!("\n[{failure}]"));
    }
    text.push('\n');
    append_log(log, &text).map_err(RunFailure::Active)?;
    if !failures.is_empty() {
        return Err(RunFailure::Active(anyhow::anyhow!(
            "schedule runtime cleanup/capture failed: {}",
            failures.join("; ")
        )));
    }
    Ok(code)
}

/// Filesystem change notifications for watch entries. Events only say *which entries may have
/// changed*; the fingerprint stays the authority, so ignore rules, coverage caps, debounce and
/// "a run's own edits are the new baseline" are unchanged. Without a working watcher (or after
/// an overflow/rescan event) every watched entry is re-fingerprinted.
struct FsEvents {
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    /// Events received by `wait`, consumed by the next `drain`.
    early: std::cell::RefCell<Vec<notify::Result<notify::Event>>>,
    roots: Vec<PathBuf>,
    _watcher: notify::RecommendedWatcher,
}

enum Changes {
    Paths(Vec<PathBuf>),
    Everything,
}

impl FsEvents {
    fn start(root: &Path) -> Result<Self> {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .context("creating filesystem watcher")?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
        // Event paths may arrive canonicalized (macOS /var → /private/var) or as given.
        let mut roots = vec![root.to_path_buf()];
        if let Ok(canonical) = root.canonicalize()
            && canonical != root
        {
            roots.push(canonical);
        }
        Ok(Self {
            rx,
            early: Default::default(),
            roots,
            _watcher: watcher,
        })
    }

    /// Wait until the deadline or `stop`; an event shortens the wait to a 250 ms batch window so
    /// a burst (save-all, build output) wakes the scheduler once.
    fn wait(&self, mut deadline: Instant, stop: &std::sync::atomic::AtomicBool) {
        while Instant::now() < deadline && !stop.load(std::sync::atomic::Ordering::Acquire) {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            match self.rx.recv_timeout(slice) {
                Ok(event) => {
                    self.early.borrow_mut().push(event);
                    deadline = deadline.min(Instant::now() + Duration::from_millis(250));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Relative paths changed since the last drain, or `Everything` when the watcher lost track.
    fn drain(&self) -> Changes {
        let mut events = std::mem::take(&mut *self.early.borrow_mut());
        events.extend(self.rx.try_iter());
        let mut paths = Vec::new();
        for event in events {
            let Ok(event) = event else {
                return Changes::Everything;
            };
            if event.need_rescan() {
                return Changes::Everything;
            }
            if matches!(event.kind, notify::EventKind::Access(_)) {
                continue;
            }
            for path in event.paths {
                match self
                    .roots
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                {
                    Some(relative) => paths.push(relative.to_path_buf()),
                    // A path we cannot place (renamed root, foreign mount): be conservative.
                    None => return Changes::Everything,
                }
            }
        }
        Changes::Paths(paths)
    }
}

/// How long a failing watch entry waits before it is re-checked without a new event.
const ERROR_RETRY: Duration = Duration::from_secs(30);

/// Per-entry watch failures: reported once per distinct message, retried, never fatal to others.
#[derive(Default)]
struct EntryErrors {
    current: HashMap<String, (String, Instant)>,
}

impl EntryErrors {
    fn fail(&mut self, store: &Store, id: &str, what: &str, error: &anyhow::Error) {
        let message = format!("{what}: {error:#}");
        let repeated = self
            .current
            .get(id)
            .is_some_and(|(previous, _)| *previous == message);
        self.current
            .insert(id.to_owned(), (message.clone(), Instant::now()));
        if !repeated {
            eprintln!("dial schedule: {id} skipped; {message}");
            if let Err(log_error) =
                append_log(&store.log_path(id), &format!("--- {} {message}\n", now()))
            {
                eprintln!("dial schedule: {id} log unavailable: {log_error:#}");
            }
        }
    }
    fn recovered(&mut self, id: &str) {
        if self.current.remove(id).is_some() {
            eprintln!("dial schedule: {id} watch coverage restored");
        }
    }
    fn retry_due(&self, id: &str) -> bool {
        self.current
            .get(id)
            .is_some_and(|(_, at)| at.elapsed() >= ERROR_RETRY)
    }
}

fn needs_watch_check(
    entry: &Entry,
    scheduler: &Scheduler,
    errors: &EntryErrors,
    dirty: &HashSet<String>,
    everything_dirty: bool,
) -> bool {
    !entry.watch.is_empty()
        && (everything_dirty
            || dirty.contains(&entry.id)
            || scheduler
                .watches
                .get(&entry.id)
                .is_none_or(|watch| watch.baseline.is_none() || watch.pending.is_some())
            || errors.retry_due(&entry.id))
}

/// The scheduler loop. Only one per project runs: a second instance fails fast on the lock.
/// Watch/fingerprint failures and invalid entries are isolated to their entry. Only an
/// uncertain process-ownership outcome (`RunFailure::Active`) stops the scheduler, because
/// launching another run could then overlap an orphaned one.
pub fn run(
    layout: &Layout,
    once: bool,
    timeout_secs: u64,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let store = Store::new(layout)?;
    fs::create_dir_all(&store.dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.dir.join("schedule.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!("another `dial schedule run` is already running for this project")
    })?;
    let exe = std::env::current_exe()?;
    let root = &layout.project_root;
    let mut scheduler = store.load_scheduler();
    let events = if once {
        None
    } else {
        match FsEvents::start(root) {
            Ok(events) => Some(events),
            Err(error) => {
                eprintln!(
                    "dial schedule: filesystem events unavailable ({error:#}); polling watched files every {}s",
                    POLL.as_secs()
                );
                None
            }
        }
    };
    let mut globs: HashMap<String, globset::GlobSet> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut stamp = None;
    let mut loaded = false;
    let mut reported: HashSet<String> = HashSet::new();
    let mut errors = EntryErrors::default();
    let mut dirty: HashSet<String> = HashSet::new();
    let mut everything_dirty = true;
    let mut rescan_next = false;
    let mut first = true;
    while !stop.load(Ordering::Acquire) {
        // Re-parse only when the file changed; a broken replacement keeps the last good set.
        let current_stamp = store.stamp();
        if !loaded || current_stamp != stamp {
            match store.load_lenient() {
                Ok((fresh, problems)) => {
                    for problem in problems {
                        if reported.insert(problem.clone()) {
                            eprintln!("dial schedule: {problem}");
                        }
                    }
                    scheduler.reconcile(&fresh);
                    errors
                        .current
                        .retain(|id, _| fresh.iter().any(|entry| entry.id == *id));
                    // Entries were validated, so their watch sets compile.
                    globs = fresh
                        .iter()
                        .filter(|entry| !entry.watch.is_empty())
                        .filter_map(|entry| {
                            watch_set(&entry.watch)
                                .ok()
                                .map(|set| (entry.id.clone(), set))
                        })
                        .collect();
                    entries = fresh;
                    everything_dirty = true;
                }
                Err(error) => {
                    let message = format!(
                        "schedules.toml unreadable; keeping the previous schedules: {error:#}"
                    );
                    if reported.insert(message.clone()) {
                        eprintln!("dial schedule: {message}");
                    }
                }
            }
            stamp = current_stamp;
            loaded = true;
        }
        if first {
            eprintln!(
                "dial schedule: watching {} schedule(s) for {}",
                entries.len(),
                root.display()
            );
            first = false;
        }
        match events.as_ref().map(FsEvents::drain) {
            None | Some(Changes::Everything) => everything_dirty = true,
            Some(Changes::Paths(paths)) => {
                for (id, set) in &globs {
                    if paths.iter().any(|path| set.is_match(path)) {
                        dirty.insert(id.clone());
                    }
                }
            }
        }
        let mut checked: HashMap<String, Result<[u8; 32]>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if stop.load(Ordering::Acquire) {
                break;
            }
            // A preceding long run may have allowed a replacement/removal on disk.
            if store.stamp() != stamp
                && !store
                    .load_lenient()
                    .is_ok_and(|(fresh, _)| fresh.iter().any(|current| current == entry))
            {
                continue;
            }
            let at = now();
            // Even interval triggers cannot pretend that a configured watch has complete coverage.
            let before = if entry.watch.is_empty() {
                None
            } else {
                let needs_check =
                    needs_watch_check(entry, &scheduler, &errors, &dirty, everything_dirty);
                if needs_check {
                    if !checked.contains_key(&entry.id) {
                        let selected: Vec<_> = entries[index..]
                            .iter()
                            .filter(|entry| {
                                needs_watch_check(
                                    entry,
                                    &scheduler,
                                    &errors,
                                    &dirty,
                                    everything_dirty,
                                )
                            })
                            .collect();
                        let sets: Vec<_> = selected.iter().map(|entry| &globs[&entry.id]).collect();
                        checked = selected
                            .iter()
                            .map(|entry| entry.id.clone())
                            .zip(fingerprints(root, &sets))
                            .collect();
                    }
                    match checked.remove(&entry.id).expect("selected watch set") {
                        Ok(fp) => {
                            errors.recovered(&entry.id);
                            Some(fp)
                        }
                        Err(error) => {
                            errors.fail(&store, &entry.id, "watch rejected", &error);
                            continue;
                        }
                    }
                } else if errors.current.contains_key(&entry.id) {
                    continue;
                } else {
                    scheduler
                        .watches
                        .get(&entry.id)
                        .and_then(|watch| watch.baseline)
                }
            };
            dirty.remove(&entry.id);
            let Some(reason) = scheduler.due(entry, at, || before) else {
                continue;
            };
            eprintln!("dial schedule: running {} ({reason:?})", entry.id);
            // A child may edit any watched file. Later entries need a fresh shared snapshot,
            // not results computed before this run (including when no watcher is available).
            checked.clear();
            match run_entry(
                &exe,
                root,
                entry,
                &store.log_path(&entry.id),
                timeout_secs,
                stop,
            ) {
                Ok(code) => eprintln!(
                    "dial schedule: {} finished with {}",
                    entry.id,
                    code.map_or("no exit code".into(), |c| c.to_string())
                ),
                Err(RunFailure::Start(error)) => {
                    eprintln!("dial schedule: {} failed to start: {error:#}", entry.id);
                    if let Err(log_error) = append_log(
                        &store.log_path(&entry.id),
                        &format!("--- {} start failed: {error:#}\n", now()),
                    ) {
                        eprintln!("dial schedule: {} log unavailable: {log_error:#}", entry.id);
                    }
                }
                Err(RunFailure::Active(error)) => {
                    // Do not launch another entry when process ownership or completion is uncertain.
                    scheduler.ran(entry, now(), None);
                    store.save_state(&scheduler);
                    return Err(error)
                        .with_context(|| format!("schedule {} stopped; see its log", entry.id));
                }
            }
            if stop.load(Ordering::Acquire) {
                scheduler.ran(entry, now(), None);
                store.save_state(&scheduler);
                break;
            }
            let after = if entry.watch.is_empty() {
                Ok(None)
            } else {
                fingerprint(root, &entry.watch).map(Some)
            };
            let finished = now();
            match after {
                Ok(after) => scheduler.ran(entry, finished, after),
                Err(error) => {
                    // No baseline: the next check re-baselines instead of re-running on stale state.
                    scheduler.ran(entry, finished, None);
                    errors.fail(
                        &store,
                        &entry.id,
                        "watch coverage rejected after run",
                        &error,
                    );
                }
            }
            // The run's own edits were folded into the new baseline; drop their queued events.
            match events.as_ref().map(FsEvents::drain) {
                Some(Changes::Paths(paths)) => {
                    for (id, set) in &globs {
                        if id != &entry.id && paths.iter().any(|path| set.is_match(path)) {
                            dirty.insert(id.clone());
                        }
                    }
                }
                Some(Changes::Everything) => rescan_next = true,
                None => {}
            }
            store.save_state(&scheduler);
        }
        everything_dirty = events.is_none() || std::mem::take(&mut rescan_next);
        if once {
            break;
        }
        // Wake for the next interval/debounce check, a filesystem event, or a schedules edit.
        let deadline = Instant::now() + POLL;
        match &events {
            Some(events) => {
                events.wait(deadline, stop);
            }
            None => {
                while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
                    std::thread::sleep(IO_POLL);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fingerprint_tests;
