//! Recurring and file-triggered prompt runs: `dial schedule`.
//!
//! Schedules are per project and live in private state outside the repository, so the agent cannot
//! create or edit them. `dial schedule run` is a foreground scheduler: it launches each due entry
//! as an ordinary `dial -p` child process (one at a time, because local compute is serialised),
//! logging its output. Entries are `readonly` unless they were added with `--yes` or an explicit
//! `--allow` list, since nobody is there to answer approval prompts.
use crate::storage::paths::Layout;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MIN_INTERVAL_SECS: u64 = 30;
const MAX_ENTRIES: usize = 64;
const MAX_PROMPT: usize = 16 * 1024;
const MAX_WATCHED_FILES: usize = 20_000;
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const DEBOUNCE_SECS: u64 = 3;
const RUN_TIMEOUT_SECS: u64 = 3600;
const POLL: Duration = Duration::from_secs(2);

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

/// A digest of path, size and modification time of every watched, non-ignored file.
pub fn fingerprint(root: &Path, patterns: &[String]) -> Result<[u8; 32]> {
    let set = watch_set(patterns)?;
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .follow_links(false)
        .build()
        .flatten()
    {
        if files.len() >= MAX_WATCHED_FILES {
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) || !set.is_match(relative) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        files.push((relative.to_path_buf(), meta.len(), modified));
    }
    files.sort();
    let mut hash = Sha256::new();
    for (path, len, modified) in files {
        hash.update(format!("{}\0{len}\0{modified}\n", path.display()));
    }
    Ok(hash.finalize().into())
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(layout: &Layout) -> Result<Self> {
        layout.ensure_state_outside_project()?;
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
    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join("schedule-logs").join(format!("{id}.log"))
    }

    pub fn load(&self) -> Result<Vec<Entry>> {
        match fs::read_to_string(self.path()) {
            Ok(text) => Ok(toml::from_str::<File>(&text)
                .context("parsing schedules")?
                .entries),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).context("reading schedules"),
        }
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
    fn save_state(&self, state: &HashMap<String, u64>) {
        if let Ok(text) = toml::to_string(state) {
            let _ = fs::write(self.state_path(), text);
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
    let access = if entry.approval == "auto" {
        "all tools approved".to_owned()
    } else if entry.allow.is_empty() {
        "read-only".to_owned()
    } else {
        format!("read-only + {}", entry.allow.join(","))
    };
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
}

impl Scheduler {
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
            if now >= since + every {
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

fn append_log(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::remove_file(path);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Run one entry as a child `dial -p`; returns the exit code (None when killed or signalled).
pub fn run_entry(exe: &Path, root: &Path, entry: &Entry, log: &Path) -> Result<Option<i32>> {
    let mut command = Command::new(exe);
    command
        .arg("-C")
        .arg(root)
        .args(["-p", "--output-format", "json"]);
    if entry.approval == "auto" {
        command.arg("-y");
    }
    // Named tools need ask mode: `--allow` never overrides readonly, and with no terminal nothing else is approved.
    else if !entry.allow.is_empty() {
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
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let reader = |stream: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut s) = stream {
                let _ = std::io::Read::read_to_end(
                    &mut std::io::Read::take(&mut s, 4 * 1024 * 1024),
                    &mut bytes,
                );
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let (out, err) = (
        reader(out.map(|s| Box::new(s) as _)),
        reader(err.map(|s| Box::new(s) as _)),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(RUN_TIMEOUT_SECS) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let (stdout, stderr) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let code = status.and_then(|s| s.code());
    append_log(
        log,
        &format!(
            "--- {} exit {} after {}s\n{}{}\n",
            now(),
            code.map_or("none (timeout/signal)".into(), |c| c.to_string()),
            started.elapsed().as_secs(),
            stdout.trim_end(),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\n[stderr]\n{}", stderr.trim_end())
            }
        ),
    );
    Ok(code)
}

/// The scheduler loop. Only one per project runs: a second instance fails fast on the lock.
pub fn run(layout: &Layout, once: bool, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
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
    let mut scheduler = Scheduler {
        last_run: store.load_state(),
        ..Scheduler::default()
    };
    eprintln!(
        "dial schedule: watching {} schedule(s) for {}",
        store.load()?.len(),
        layout.project_root.display()
    );
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        // Re-read every tick so `add` and `remove` take effect without a restart.
        let entries = store.load()?;
        for entry in &entries {
            let at = now();
            let root = &layout.project_root;
            let Some(reason) = scheduler.due(entry, at, || fingerprint(root, &entry.watch).ok())
            else {
                continue;
            };
            eprintln!("dial schedule: running {} ({reason:?})", entry.id);
            match run_entry(&exe, root, entry, &store.log_path(&entry.id)) {
                Ok(code) => eprintln!(
                    "dial schedule: {} finished with {}",
                    entry.id,
                    code.map_or("no exit code".into(), |c| c.to_string())
                ),
                Err(error) => {
                    eprintln!("dial schedule: {} failed to start: {error:#}", entry.id);
                    append_log(
                        &store.log_path(&entry.id),
                        &format!("--- {} start failed: {error:#}\n", now()),
                    );
                }
            }
            scheduler.ran(entry, now(), fingerprint(root, &entry.watch).ok());
            store.save_state(&scheduler.last_run);
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        if once {
            break;
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(every: Option<u64>, watch: &[&str]) -> Entry {
        Entry {
            id: "s1".into(),
            prompt: "p".into(),
            every_secs: every,
            watch: watch.iter().map(|w| (*w).into()).collect(),
            allow: vec![],
            approval: "readonly".into(),
            created: 1000,
        }
    }

    #[test]
    fn durations_parse_and_reject_garbage() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        assert_eq!(parse_duration("45").unwrap(), 45);
        for bad in ["", "m", "5x", "-5m", "1.5h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entries_need_a_trigger_and_a_sane_interval() {
        assert!(entry(Some(60), &[]).validate().is_ok());
        assert!(entry(None, &["src/**/*.rs"]).validate().is_ok());
        assert!(entry(None, &[]).validate().is_err());
        assert!(entry(Some(5), &[]).validate().is_err());
        assert!(entry(None, &["["]).validate().is_err());
        assert!(
            Entry {
                approval: "ask".into(),
                ..entry(Some(60), &[])
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn intervals_count_from_creation_then_from_the_last_run() {
        let mut s = Scheduler::default();
        let e = entry(Some(100), &[]);
        assert_eq!(s.due(&e, 1099, || None), None);
        assert_eq!(s.due(&e, 1100, || None), Some(Reason::Interval));
        s.ran(&e, 1100, None);
        assert_eq!(s.due(&e, 1199, || None), None);
        assert_eq!(s.due(&e, 1200, || None), Some(Reason::Interval));
    }

    #[test]
    fn file_changes_must_settle_and_a_runs_own_edits_do_not_retrigger() {
        let mut s = Scheduler::default();
        let e = entry(None, &["*.rs"]);
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        assert_eq!(
            s.due(&e, 10, || Some(a)),
            None,
            "first sight is the baseline"
        );
        assert_eq!(s.due(&e, 11, || Some(a)), None);
        assert_eq!(s.due(&e, 12, || Some(b)), None, "change seen, debouncing");
        assert_eq!(
            s.due(&e, 13, || Some(c)),
            None,
            "still changing restarts the wait"
        );
        assert_eq!(s.due(&e, 15, || Some(c)), None);
        assert_eq!(s.due(&e, 16, || Some(c)), Some(Reason::Changed));
        s.ran(&e, 17, Some(c));
        assert_eq!(
            s.due(&e, 30, || Some(c)),
            None,
            "edits made by the run itself are the new baseline"
        );
    }

    #[test]
    fn fingerprints_follow_matching_files_only_and_respect_ignores() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "1").unwrap();
        fs::write(dir.path().join("notes.txt"), "1").unwrap();
        let pats = vec!["src/**/*.rs".to_owned()];
        let before = fingerprint(dir.path(), &pats).unwrap();
        fs::write(dir.path().join("notes.txt"), "22").unwrap();
        assert_eq!(
            fingerprint(dir.path(), &pats).unwrap(),
            before,
            "unmatched file"
        );
        fs::write(dir.path().join("src/a.rs"), "22").unwrap();
        assert_ne!(fingerprint(dir.path(), &pats).unwrap(), before);
    }

    #[test]
    fn store_adds_numbers_and_removes() {
        let layout_dir = tempfile::tempdir().unwrap();
        let store = Store {
            dir: layout_dir.path().to_path_buf(),
        };
        let mut e = entry(Some(60), &[]);
        e.id.clear();
        assert_eq!(store.add(e.clone()).unwrap(), "s1");
        assert_eq!(store.add(e.clone()).unwrap(), "s2");
        store.remove("s1").unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        assert!(store.remove("s1").is_err());
    }
}
