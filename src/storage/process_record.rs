//! Private process metadata shared by model serving and background learning.
//!
//! A record also carries an optional [`SpawnSpec`] (how the process was started, so `ps restart`
//! can start it again) and the path of its captured output log (`ps logs`).
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::platform::fs as platform_fs;
use crate::storage::settings_store;
use crate::tools::background::procinfo::ProcId;

/// Largest tail returned by [`read_tail`].
pub const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// How a managed process was started. Restart re-runs `program` with `args` in `cwd`, with the
/// environment cleared, then copies `env_allow` names from Dial's current environment and sets `env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env_allow: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

pub struct Record {
    path: PathBuf,
    value: Value,
    log: Option<PathBuf>,
    /// Detached records outlive this handle (their process was started by `ps restart`).
    persist: bool,
}

impl Record {
    pub fn new(
        user: &Path,
        pid: u32,
        kind: &str,
        model: &str,
        project: &Path,
        state: &Path,
        phase: &str,
    ) -> Result<Self> {
        Self::create(
            user,
            pid,
            std::process::id(),
            kind,
            model,
            project,
            state,
            phase,
            false,
        )
    }

    /// Record for a process whose parent exits right after starting it (`ps restart`). Its parent
    /// becomes init, so the record is written with parent 1 and is kept after this handle drops.
    pub fn detached(
        user: &Path,
        pid: u32,
        kind: &str,
        model: &str,
        project: &Path,
        state: &Path,
        phase: &str,
    ) -> Result<Self> {
        Self::create(user, pid, 1, kind, model, project, state, phase, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        user: &Path,
        pid: u32,
        parent: u32,
        kind: &str,
        model: &str,
        project: &Path,
        state: &Path,
        phase: &str,
        persist: bool,
    ) -> Result<Self> {
        let path = record_path(user, pid);
        settings_store::private_dir(path.parent().context("process record directory")?)?;
        let value = json!({"pid":pid,"parent_pid":parent,"proc":ProcId::of(pid as i32),
            "kind":kind,"model":model,"project":project,"state":state,"phase":phase});
        let record = Self {
            path,
            value,
            log: None,
            persist,
        };
        record.save()?;
        Ok(record)
    }

    fn save(&self) -> Result<()> {
        settings_store::write_private(&self.path, self.value.to_string().as_bytes())
    }

    pub fn phase(&mut self, phase: &str) -> Result<()> {
        self.value["phase"] = json!(phase);
        self.save()
    }

    /// Records how the process was started and where its captured output goes.
    pub fn set_spawn(&mut self, spec: &SpawnSpec, log: &Log) -> Result<()> {
        self.value["spawn"] = serde_json::to_value(spec)?;
        self.value["log"] = json!(log.path());
        self.log = Some(log.path().to_path_buf());
        self.save()
    }

    /// Records how the process was started when its output is not captured to a log file.
    pub fn set_spawn_spec(&mut self, spec: &SpawnSpec) -> Result<()> {
        self.value["spawn"] = serde_json::to_value(spec)?;
        self.save()
    }

    /// Keeps the record file after this handle is dropped.
    pub fn detach(mut self) {
        self.persist = true;
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        if self.persist {
            return;
        }
        let _ = fs::remove_file(&self.path);
        if let Some(log) = &self.log {
            let _ = fs::remove_file(log);
        }
    }
}

fn record_path(user: &Path, pid: u32) -> PathBuf {
    user.join("compute-processes").join(format!("{pid}.json"))
}

/// Loads the private record for `pid`, if one exists.
pub fn load(user: &Path, pid: u32) -> Result<Option<Value>> {
    settings_store::read_private(&record_path(user, pid))?
        .map(|bytes| serde_json::from_slice(&bytes).context("reading process record"))
        .transpose()
}

/// The kernel identity recorded when the process was created. Missing or unverifiable identities
/// (including records written before identities were recorded) read as `None`, so the record can
/// never be attached to a recycled PID.
pub(crate) fn proc_id(record: &Value) -> Option<ProcId> {
    serde_json::from_value::<ProcId>(record["proc"].clone())
        .ok()
        .filter(|id| id.start != 0)
}

/// The spawn spec stored in a loaded record, if the process was started with one.
pub fn spawn_spec(record: &Value) -> Result<Option<SpawnSpec>> {
    if record["spawn"].is_null() {
        return Ok(None);
    }
    serde_json::from_value(record["spawn"].clone())
        .map(Some)
        .context("reading recorded spawn spec")
}

/// Captured output file for one process. Created before the child starts so its stdio can point at it.
pub struct Log {
    path: PathBuf,
}

static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl Log {
    pub fn create(user: &Path) -> Result<Arc<Self>> {
        let dir = user.join("compute-processes").join("logs");
        settings_store::private_dir(&dir)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let sequence = LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{sequence}-{nanos}.log", std::process::id()));
        platform_fs::open_owner_only(
            &OpenOptions::new().read(true).write(true).create_new(true),
            &path,
        )
        .with_context(|| format!("creating process log {}", path.display()))?;
        Ok(Arc::new(Self { path }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stdio handles that write straight into the log. Used for detached children, whose pipes
    /// would close when the launching command exits.
    pub fn stdio(&self) -> Result<(Stdio, Stdio)> {
        let open = || -> io::Result<File> { OpenOptions::new().append(true).open(&self.path) };
        Ok((Stdio::from(open()?), Stdio::from(open()?)))
    }
}

/// The last `max` bytes of a captured log, starting at a line boundary when the file was cut.
pub fn read_tail(path: &Path, max: u64) -> Result<String> {
    let mut file = platform_fs::open_nofollow_nonblocking(&OpenOptions::new().read(true), path)
        .with_context(|| format!("opening process log {}", path.display()))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "process log is not a regular file");
    let start = meta.len().saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file).take(max).read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> SpawnSpec {
        SpawnSpec {
            program: PathBuf::from("/opt/server/bin/python"),
            args: vec!["-m".into(), "server".into(), "--port=8001".into()],
            cwd: PathBuf::from("/work/project"),
            env_allow: vec!["PATH".into(), "HOME".into()],
            env: vec![("DIAL_MODEL_ENGINE".into(), "mlx-lm".into())],
        }
    }

    #[test]
    fn spawn_spec_round_trips_through_the_record_file() {
        let home = tempfile::tempdir().unwrap();
        let log = Log::create(home.path()).unwrap();
        let mut record = Record::new(
            home.path(),
            4242,
            "models",
            "test-model",
            Path::new("project"),
            Path::new("state"),
            "loading",
        )
        .unwrap();
        record.set_spawn(&sample_spec(), &log).unwrap();
        let loaded = load(home.path(), 4242).unwrap().unwrap();
        assert_eq!(spawn_spec(&loaded).unwrap(), Some(sample_spec()));
        assert_eq!(loaded["log"], json!(log.path()));
        let log_path = log.path().to_path_buf();
        drop(record);
        assert!(!record_path(home.path(), 4242).exists());
        assert!(!log_path.exists());
    }

    #[test]
    fn records_without_spawn_spec_report_none_and_missing_records_load_as_none() {
        let home = tempfile::tempdir().unwrap();
        let _record = Record::new(
            home.path(),
            4243,
            "tools",
            "",
            Path::new("project"),
            Path::new("state"),
            "running",
        )
        .unwrap();
        let loaded = load(home.path(), 4243).unwrap().unwrap();
        assert_eq!(spawn_spec(&loaded).unwrap(), None);
        assert!(load(home.path(), 99_999).unwrap().is_none());
    }

    #[test]
    fn detached_records_survive_their_handle_and_point_at_init() {
        let home = tempfile::tempdir().unwrap();
        let record = Record::detached(
            home.path(),
            4244,
            "models",
            "m",
            Path::new("project"),
            Path::new("state"),
            "loading",
        )
        .unwrap();
        record.detach();
        let loaded = load(home.path(), 4244).unwrap().unwrap();
        assert_eq!(loaded["parent_pid"], 1);
        fs::remove_file(record_path(home.path(), 4244)).unwrap();
    }

    #[test]
    fn read_tail_returns_only_the_bounded_tail_at_a_line_boundary() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("out.log");
        let mut text = String::new();
        for line in 0..2000 {
            text.push_str(&format!("line {line:04}\n"));
        }
        fs::write(&path, &text).unwrap();
        let tail = read_tail(&path, 100).unwrap();
        assert!(tail.len() <= 100);
        assert!(tail.ends_with("line 1999\n"));
        assert!(tail.starts_with("line "));
        assert_eq!(read_tail(&path, 1 << 20).unwrap(), text);
    }
}
