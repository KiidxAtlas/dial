//! Per-project security store. State lives under
//! `<user_dir>/security/<projectKey>/`, never inside the scanned tree. Writes are atomic
//! (temp file then rename), files are 0600 and directories 0700, and a per-project in-process
//! lock plus an advisory file lock serialize index updates.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::contracts::{self, Plan, Scan};

const INDEX_FILE: &str = "index.json";
const SCAN_FILE: &str = "scan.json";
const OWNER_FILE: &str = "owner.lock";
const CANCEL_FILE: &str = "cancel-requested";

/// Exclusive ownership of one scan, held by the process executing it.
pub struct ScanClaim(#[allow(dead_code)] fs::File);

/// On-disk `index.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub schema_version: u32,
    pub project_key: String,
    pub repository_root: String,
    pub scan_ids: Vec<String>,
    pub plan_ids: Vec<String>,
    pub updated_at: String,
}

/// Handle to one project's store.
pub struct Store {
    dir: PathBuf,
    project_key: String,
    repository_root: String,
}

type ProcessLocks = Vec<(PathBuf, Arc<Mutex<()>>)>;
static PROCESS_LOCKS: LazyLock<Mutex<ProcessLocks>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn process_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut locks = PROCESS_LOCKS.lock();
    if let Some((_, lock)) = locks.iter().find(|(path, _)| path == dir) {
        return Arc::clone(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.push((dir.to_path_buf(), Arc::clone(&lock)));
    lock
}

/// Creates a directory and every missing parent with mode 0700.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Writes `bytes` to `path` atomically with mode 0600: temp file in the same directory, sync,
/// then rename over the target.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("security store path has no parent directory")?;
    ensure_private_dir(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        contracts::sha256_hex(&rand::random::<[u8; 16]>())[..16].to_owned()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn is_valid_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

impl Store {
    /// Opens (creating directories as needed) the store for `repository_root` under `user_dir`.
    pub fn open(user_dir: &Path, repository_root: &str) -> Result<Self> {
        let project_key = contracts::project_key(repository_root);
        let dir = user_dir.join("security").join(&project_key);
        ensure_private_dir(&dir)?;
        Ok(Self {
            dir,
            project_key,
            repository_root: repository_root.to_owned(),
        })
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn repository_root(&self) -> &str {
        &self.repository_root
    }

    fn with_index_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = process_lock(&self.dir);
        let _guard = lock.lock();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".lock"))
            .context("opening security store lock")?;
        fs2::FileExt::lock_exclusive(&file).context("locking security store")?;
        let result = f();
        let _ = fs2::FileExt::unlock(&file);
        result
    }

    /// Reads `index.json`, or an empty index if none exists yet.
    pub fn index(&self) -> Result<Index> {
        let path = self.dir.join(INDEX_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Index {
                    schema_version: 1,
                    project_key: self.project_key.clone(),
                    repository_root: self.repository_root.clone(),
                    ..Index::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        let index: Index = serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("Unsupported security store index at {}", path.display())
        })?;
        ensure!(
            index.schema_version == 1,
            "Unsupported security store index at {}",
            path.display()
        );
        ensure!(
            index.project_key == self.project_key,
            "Security scan project key {} does not match {}",
            index.project_key,
            self.project_key
        );
        Ok(index)
    }

    fn save_index(&self, mut index: Index) -> Result<()> {
        index.schema_version = 1;
        index.project_key = self.project_key.clone();
        index.repository_root = self.repository_root.clone();
        index.updated_at = now_iso();
        let bytes = serde_json::to_vec_pretty(&index)?;
        write_private_atomic(&self.dir.join(INDEX_FILE), &bytes)
    }

    /// Persists a plan under `plans/<id>.json` and records it in the index.
    pub fn save_plan(&self, plan: &Plan) -> Result<()> {
        ensure!(
            plan.repository_root == self.repository_root,
            "Security plan repository {} does not match {}",
            plan.repository_root,
            self.repository_root
        );
        self.with_index_lock(|| {
            let plans = self.dir.join("plans");
            ensure_private_dir(&plans)?;
            write_private_atomic(
                &plans.join(format!("{}.json", plan.id)),
                &serde_json::to_vec_pretty(plan)?,
            )?;
            let mut index = self.index()?;
            if !index.plan_ids.contains(&plan.id) {
                index.plan_ids.push(plan.id.clone());
            }
            self.save_index(index)
        })
    }

    pub fn load_plan(&self, id: &str) -> Result<Plan> {
        ensure!(
            is_valid_id(id, "secplan_"),
            "Invalid security plan id: {id}"
        );
        let path = self.dir.join("plans").join(format!("{id}.json"));
        let bytes = fs::read(&path).map_err(|_| anyhow::anyhow!("Unknown security plan: {id}"))?;
        let plan: Plan = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid security plan document: {id}"))?;
        ensure!(
            plan.repository_root == self.repository_root,
            "Security plan repository {} does not match {}",
            plan.repository_root,
            self.repository_root
        );
        Ok(plan)
    }

    /// Writes a scan. `scan.json` is written last: a scan directory without it is incomplete.
    pub fn save_scan(&self, scan: &Scan) -> Result<()> {
        ensure!(
            scan.project_key == self.project_key,
            "Security scan project key {} does not match {}",
            scan.project_key,
            self.project_key
        );
        self.with_index_lock(|| {
            let dir = self.scan_dir(&scan.id)?;
            ensure_private_dir(&dir)?;
            write_private_atomic(&dir.join(SCAN_FILE), &serde_json::to_vec_pretty(scan)?)?;
            let mut index = self.index()?;
            if !index.scan_ids.contains(&scan.id) {
                index.scan_ids.push(scan.id.clone());
            }
            self.save_index(index)
        })
    }

    fn scan_dir(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            is_valid_id(id, "secscan_"),
            "Invalid security scan id: {id}"
        );
        Ok(self.dir.join("scans").join(id))
    }

    /// Claims a scan for this process. The advisory lock on `scans/<id>/owner.lock` is held for
    /// the guard's lifetime and released by the OS if the process dies, so other processes can
    /// tell a live scan from one interrupted by a crash.
    pub fn claim_scan(&self, id: &str) -> Result<ScanClaim> {
        let dir = self.scan_dir(id)?;
        ensure_private_dir(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(OWNER_FILE))
            .context("opening security scan owner lock")?;
        fs2::FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("Security scan {id} is already running"))?;
        Ok(ScanClaim(file))
    }

    /// Whether another live handle holds the scan's owner lock. Legacy scans without a lock
    /// file are not running.
    pub fn scan_claimed(&self, id: &str) -> bool {
        let Ok(dir) = self.scan_dir(id) else {
            return false;
        };
        let Ok(file) = OpenOptions::new().read(true).open(dir.join(OWNER_FILE)) else {
            return false;
        };
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                let _ = fs2::FileExt::unlock(&file);
                false
            }
            Err(_) => true,
        }
    }

    /// Asks the process that owns `id` to stop after its current batch.
    pub fn request_cancel_marker(&self, id: &str) -> Result<()> {
        let dir = self.scan_dir(id)?;
        write_private_atomic(&dir.join(CANCEL_FILE), b"cancel")
    }

    /// Whether a cross-process cancellation was requested for `id`.
    pub fn cancel_requested(&self, id: &str) -> bool {
        self.scan_dir(id)
            .is_ok_and(|dir| dir.join(CANCEL_FILE).is_file())
    }

    pub fn load_scan(&self, id: &str) -> Result<Scan> {
        let dir = self.scan_dir(id)?;
        let bytes = fs::read(dir.join(SCAN_FILE))
            .map_err(|_| anyhow::anyhow!("Unknown security scan: {id}"))?;
        let scan: Scan = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid security scan document: {id}"))?;
        ensure!(
            scan.id == id,
            "Security scan document id {} does not match {id}",
            scan.id
        );
        Ok(scan)
    }

    /// Scans in index order (oldest first). Incomplete scan directories are skipped.
    pub fn scans(&self) -> Result<Vec<Scan>> {
        let mut out = Vec::new();
        for id in self.index()?.scan_ids {
            if let Ok(scan) = self.load_scan(&id) {
                out.push(scan);
            }
        }
        Ok(out)
    }

    /// Loads a stored scan, applies `change`, and rewrites it, all under the index lock.
    /// Use this for read-modify-write updates: a load outside the lock can be overwritten by a
    /// concurrent writer that lands between the read and the save.
    pub fn update_scan_with<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Scan) -> Result<T>,
    ) -> Result<T> {
        let dir = self.scan_dir(id)?;
        self.with_index_lock(|| {
            let mut scan = self.load_scan(id)?;
            let value = change(&mut scan)?;
            write_private_atomic(&dir.join(SCAN_FILE), &serde_json::to_vec_pretty(&scan)?)?;
            Ok(value)
        })
    }

    /// Writes an artifact file inside a scan directory (e.g. `results.sarif`).
    pub fn write_scan_artifact(&self, scan_id: &str, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        ensure!(
            !name.contains('/') && !name.contains('\\') && !name.starts_with('.'),
            "invalid artifact name: {name}"
        );
        let dir = self.scan_dir(scan_id)?;
        ensure_private_dir(&dir)?;
        let path = dir.join(name);
        write_private_atomic(&path, bytes)?;
        Ok(path)
    }
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_utc(secs)
}

/// RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for a Unix time in seconds.
pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests;
