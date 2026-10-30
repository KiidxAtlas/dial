use crate::{
    config::types::{Admission, Event, Harness, Outcome, Version},
    platform::fs as platform_fs,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Duration;
mod session_training;
pub use session_training::SessionTrainingExample;

/// The immutable dataset manifest and incremental cursor associated with an adapter version.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingState {
    pub train_path: String,
    pub watermark: u64,
    pub replay: Vec<String>,
    #[serde(default)]
    pub prefix_digest: Option<String>,
    #[serde(default)]
    pub validation_path: Option<String>,
    #[serde(default)]
    pub train_digest: Option<String>,
    #[serde(default)]
    pub validation_digest: Option<String>,
}

/// A verified trajectory: (id, version, domain, score, source, payload).
pub type VerifiedSuccess = (i64, i64, String, f64, String, Value);

/// What [`Store::approve_session`] did with each completed turn of a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionApproval {
    /// Newly approved for training.
    pub approved: usize,
    /// Already verified (a passing check, commit or earlier approval).
    pub already: usize,
    /// Kept as is: a person or verifier scored the turn below 100.
    pub judged: usize,
    /// Failed, or no saved request to learn from.
    pub unusable: usize,
}

pub struct Store {
    conn: Connection,
}

const VERSION_COLUMNS: &str = "id,parent,kind,harness,adapter,status";

fn version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let harness: String = row.get(3)?;
    let harness = serde_json::from_str(&harness).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Version {
        id: row.get(0)?,
        parent: row.get(1)?,
        kind: row.get(2)?,
        harness,
        adapter: row.get::<_, Option<String>>(4)?.map(Into::into),
        status: row.get(5)?,
    })
}

/// Stored harness JSON keyed by version id, with the parent id.
type RawHarnesses = HashMap<i64, (Option<i64>, Value)>;

/// Tool names in a harness JSON value.
fn harness_tools(value: &Value) -> Option<Vec<String>> {
    value.get("tools")?.as_array().map(|tools| {
        tools
            .iter()
            .filter_map(|tool| tool.as_str().map(str::to_owned))
            .collect()
    })
}

/// Harness values earlier releases shipped as `Harness::default()`. Bootstrap rows created by those
/// releases froze them; resolution maps them back to the current defaults.
fn legacy_harness_defaults() -> [(&'static str, Value); 3] {
    [
        (
            "system_prompt",
            json!(
                "Work in small verified steps. Read before you edit, run the project's checks after changing code, and say plainly when you are unsure."
            ),
        ),
        ("max_steps", json!(40)),
        ("max_tokens", json!(8192)),
    ]
}

/// Resolves each version's effective harness from its parent chain. A root keeps its stored fields and
/// gains the current default tools it lacks, so a frozen legacy tool list picks up new tools. A child
/// applies only the fields that differ from its parent's stored JSON, and its tools as a set delta over
/// the resolved parent tools.
fn resolve_harnesses(conn: &Connection, versions: &mut [Version]) -> Result<()> {
    let mut statement = conn.prepare("SELECT id,parent,harness FROM versions")?;
    let mut raw: RawHarnesses = HashMap::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (id, parent, harness) = row?;
        raw.insert(id, (parent, serde_json::from_str(&harness)?));
    }
    let defaults = serde_json::to_value(Harness::default())?;
    let mut memo = HashMap::new();
    for version in versions.iter_mut() {
        let value = resolve_harness_value(version.id, &raw, &defaults, &mut memo, raw.len())?;
        version.harness = serde_json::from_value(value)?;
    }
    Ok(())
}

fn resolve_harness_value(
    id: i64,
    raw: &RawHarnesses,
    defaults: &Value,
    memo: &mut HashMap<i64, Value>,
    depth: usize,
) -> Result<Value> {
    if let Some(value) = memo.get(&id) {
        return Ok(value.clone());
    }
    let (parent, own) = raw
        .get(&id)
        .with_context(|| format!("version {id} is missing"))?;
    let value = match parent {
        None => {
            let mut root = own.clone();
            if let Some(object) = root.as_object_mut() {
                let mut tools = harness_tools(own).unwrap_or_default();
                for tool in harness_tools(defaults).unwrap_or_default() {
                    if !tools.contains(&tool) {
                        tools.push(tool);
                    }
                }
                object.insert("tools".into(), json!(tools));
                // A value equal to a previously shipped default was never an operator choice, so it
                // follows the current default; any other stored value is kept.
                for (field, shipped) in legacy_harness_defaults() {
                    if object.get(field) == Some(&shipped) {
                        object.insert(field.into(), defaults[field].clone());
                    }
                }
            }
            root
        }
        Some(parent) => {
            ensure!(depth > 0, "version {id} has a cyclic parent chain");
            let base = resolve_harness_value(*parent, raw, defaults, memo, depth - 1)?;
            let (_, parent_own) = raw
                .get(parent)
                .with_context(|| format!("version {parent} is missing"))?;
            apply_harness_delta(base, parent_own, own)
        }
    };
    memo.insert(id, value.clone());
    Ok(value)
}

/// Applies the fields of `own` that differ from `parent_own` onto `base`; tools change as a set delta.
fn apply_harness_delta(base: Value, parent_own: &Value, own: &Value) -> Value {
    let tools_of = harness_tools;
    let mut value = base;
    let Some(own_object) = own.as_object() else {
        return value;
    };
    if let Some(object) = value.as_object_mut() {
        for (key, field) in own_object {
            if key != "tools" && parent_own.get(key) != Some(field) {
                object.insert(key.clone(), field.clone());
            }
        }
    }
    let parent_tools = tools_of(parent_own).unwrap_or_default();
    if let Some(own_tools) = tools_of(own)
        && own_tools != parent_tools
    {
        let mut tools = tools_of(&value).unwrap_or_default();
        tools.retain(|tool| own_tools.contains(tool) || !parent_tools.contains(tool));
        for tool in &own_tools {
            if !parent_tools.contains(tool) && !tools.contains(tool) {
                tools.push(tool.clone());
            }
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("tools".into(), json!(tools));
        }
    }
    value
}

fn validate_score(score: f64) -> Result<()> {
    ensure!(
        score.is_finite() && (0.0..=100.0).contains(&score),
        "score must be finite and between 0 and 100"
    );
    Ok(())
}

fn valid_report(report: &Admission) -> bool {
    let mut names = HashSet::new();
    report.scores.len() >= 2
        && report.scores.iter().filter(|score| score.primary).count() == 1
        && report.scores.iter().all(|score| {
            !score.name.trim().is_empty()
                && names.insert(score.name.trim())
                && score.cases > 0
                && validate_score(score.baseline).is_ok()
                && validate_score(score.candidate).is_ok()
                && score.candidate >= score.baseline - 1.0
                && (!score.primary || score.candidate > score.baseline)
        })
}

impl Store {
    /// A separate observational connection for background UI status; never runs migrations or writes.
    pub fn open_read_only(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("state.sqlite3");
        let file =
            platform_fs::open_nofollow_nonblocking(fs::OpenOptions::new().read(true), &path)?;
        ensure!(
            file.metadata()?.is_file(),
            "state database must be a regular file"
        );
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(100))?;
        Ok(Self { conn })
    }

    pub fn open(state_dir: &Path, model: &str, use_case: &str, initial: &Harness) -> Result<Self> {
        ensure!(
            !model.trim().is_empty() && !use_case.trim().is_empty(),
            "model and use_case must be nonempty"
        );
        match platform_fs::create_dir_owner_only(fs::DirBuilder::new().recursive(true), state_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create state directory"),
        }
        let state_meta = fs::symlink_metadata(state_dir)?;
        ensure!(
            state_meta.is_dir() && !state_meta.file_type().is_symlink(),
            "state directory must be a regular directory"
        );
        platform_fs::set_owner_only(state_dir)?;
        let path = state_dir.join("state.sqlite3");
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "state database must be a regular file"
            );
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        let _file =
            platform_fs::open_owner_only(&options, &path).context("open state database file")?;
        platform_fs::set_owner_only(&path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS versions(
                id INTEGER PRIMARY KEY, parent INTEGER REFERENCES versions(id), kind TEXT NOT NULL,
                harness TEXT NOT NULL, adapter TEXT, status TEXT NOT NULL CHECK(status IN ('active','known_good','candidate','rejected')),
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE UNIQUE INDEX IF NOT EXISTS one_active ON versions(status) WHERE status='active';
            CREATE TRIGGER IF NOT EXISTS immutable_version BEFORE UPDATE OF id,parent,kind,harness,adapter,created_at ON versions
                BEGIN SELECT RAISE(ABORT,'version content is immutable'); END;
            CREATE TRIGGER IF NOT EXISTS retain_version BEFORE DELETE ON versions
                BEGIN SELECT RAISE(ABORT,'version history is immutable'); END;
            CREATE TABLE IF NOT EXISTS version_training(
                version INTEGER PRIMARY KEY REFERENCES versions(id),payload TEXT NOT NULL);
            CREATE TRIGGER IF NOT EXISTS immutable_training BEFORE UPDATE ON version_training
                BEGIN SELECT RAISE(ABORT,'version training state is immutable'); END;
            CREATE TRIGGER IF NOT EXISTS retain_training BEFORE DELETE ON version_training
                BEGIN SELECT RAISE(ABORT,'version training history is immutable'); END;
            CREATE TABLE IF NOT EXISTS admissions(
                id INTEGER PRIMARY KEY,candidate INTEGER NOT NULL,baseline INTEGER NOT NULL,report TEXT NOT NULL,
                activated INTEGER NOT NULL,decision TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE IF NOT EXISTS transitions(
                id INTEGER PRIMARY KEY,previous INTEGER REFERENCES versions(id),current INTEGER NOT NULL REFERENCES versions(id),
                reason TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE IF NOT EXISTS trajectories(
                id INTEGER PRIMARY KEY,version INTEGER NOT NULL REFERENCES versions(id),domain TEXT NOT NULL,task TEXT NOT NULL,prompt TEXT NOT NULL,
                output TEXT,error TEXT,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),finished_at TEXT);
            CREATE TABLE IF NOT EXISTS events(
                id INTEGER PRIMARY KEY,trajectory INTEGER NOT NULL REFERENCES trajectories(id),version INTEGER NOT NULL REFERENCES versions(id),
                domain TEXT NOT NULL,task TEXT NOT NULL,step INTEGER NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL,score REAL CHECK(score BETWEEN 0 AND 100),
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE INDEX IF NOT EXISTS trajectory_events ON events(trajectory,id);
            CREATE TABLE IF NOT EXISTS outcomes(
                trajectory INTEGER PRIMARY KEY REFERENCES trajectories(id),score REAL NOT NULL CHECK(score BETWEEN 0 AND 100),source TEXT NOT NULL,reference TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE IF NOT EXISTS outcome_audit(
                id INTEGER PRIMARY KEY,trajectory INTEGER NOT NULL REFERENCES trajectories(id),score REAL NOT NULL,source TEXT NOT NULL,reference TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE IF NOT EXISTS tool_recoveries(
                id INTEGER PRIMARY KEY,trajectory INTEGER NOT NULL REFERENCES trajectories(id),version INTEGER NOT NULL REFERENCES versions(id),
                digest TEXT NOT NULL UNIQUE,payload TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE TABLE IF NOT EXISTS executable_repairs(
                id INTEGER PRIMARY KEY,trajectory INTEGER NOT NULL REFERENCES trajectories(id),version INTEGER NOT NULL REFERENCES versions(id),
                project_root TEXT NOT NULL,digest TEXT NOT NULL UNIQUE,payload TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
            CREATE INDEX IF NOT EXISTS executable_repairs_project ON executable_repairs(project_root,id);
            CREATE TABLE IF NOT EXISTS trajectory_messages(
                trajectory INTEGER PRIMARY KEY REFERENCES trajectories(id),payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS session_examples(
                id INTEGER PRIMARY KEY,source_session TEXT NOT NULL,source_turn TEXT NOT NULL,
                source_model TEXT NOT NULL,payload TEXT NOT NULL,
                UNIQUE(source_session,source_turn));")?;
        let store = Self { conn };
        let tx = store.transaction()?;
        let binding: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='model'", [], |r| r.get(0))
            .optional()?;
        if let Some(binding) = binding {
            let stored_case: String =
                tx.query_row("SELECT value FROM meta WHERE key='use_case'", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                binding == model && stored_case == use_case,
                "state is already bound to a different model or use_case"
            );
        } else {
            let count: i64 = tx.query_row("SELECT count(*) FROM versions", [], |r| r.get(0))?;
            ensure!(
                count == 0,
                "state binding is missing from an existing version history"
            );
            tx.execute("INSERT INTO versions(parent,kind,harness,adapter,status) VALUES(NULL,'bootstrap',?1,NULL,'active')", [serde_json::to_string(initial)?])?;
            let id = tx.last_insert_rowid();
            for (key, value) in [
                ("model", model.to_owned()),
                ("use_case", use_case.to_owned()),
                ("active", id.to_string()),
            ] {
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES(?1,?2)",
                    params![key, value],
                )?;
            }
            tx.execute(
                "INSERT INTO transitions(previous,current,reason) VALUES(NULL,?1,'bootstrap')",
                [id],
            )?;
        }
        // Legacy global state can only be attributed to the currently active adapter.
        // Never invent cursors for ancestors; rolling back to one safely rereads the dataset.
        let legacy: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='lora_commit'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(legacy) = legacy {
            let state: TrainingState =
                serde_json::from_str(&legacy).context("corrupt legacy training state")?;
            tx.execute(
                "INSERT OR IGNORE INTO version_training(version,payload)
                SELECT id,?1 FROM versions WHERE status='active' AND adapter IS NOT NULL",
                [serde_json::to_string(&state)?],
            )?;
            tx.execute("DELETE FROM meta WHERE key='lora_commit'", [])?;
        }
        tx.commit()?;
        store.active()?;
        Ok(store)
    }

    fn transaction(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    pub fn active(&self) -> Result<Version> {
        let id: String =
            self.conn
                .query_row("SELECT value FROM meta WHERE key='active'", [], |row| {
                    row.get(0)
                })?;
        let version = self.version(id.parse().context("invalid active version metadata")?)?;
        ensure!(
            version.status == "active",
            "active version status is inconsistent"
        );
        Ok(version)
    }

    pub fn version(&self, id: i64) -> Result<Version> {
        let mut versions = vec![self.conn.query_row(
            &format!("SELECT {VERSION_COLUMNS} FROM versions WHERE id=?1"),
            [id],
            version_row,
        )?];
        resolve_harnesses(&self.conn, &mut versions)?;
        Ok(versions.remove(0))
    }

    pub fn versions(&self) -> Result<Vec<Version>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {VERSION_COLUMNS} FROM versions ORDER BY id"
        ))?;
        let mut versions: Vec<Version> = statement
            .query_map([], version_row)?
            .collect::<rusqlite::Result<_>>()?;
        resolve_harnesses(&self.conn, &mut versions)?;
        Ok(versions)
    }

    /// Latest LoRA gate decision, including rejected/stale candidates; never infer activation from a score.
    pub fn latest_lora_admission(&self) -> Result<Option<(Admission, bool, String)>> {
        let row = self.conn.query_row("SELECT a.report,a.activated,a.decision FROM admissions a JOIN versions v ON v.id=a.candidate WHERE v.kind='lora' ORDER BY a.id DESC LIMIT 1", [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?, row.get::<_, String>(2)?))).optional()?;
        row.map(|(report, activated, decision)| {
            Ok((serde_json::from_str(&report)?, activated, decision))
        })
        .transpose()
    }

    pub fn candidate(
        &self,
        parent: i64,
        kind: &str,
        harness: &Harness,
        adapter: Option<&Path>,
    ) -> Result<Version> {
        ensure!(!kind.trim().is_empty(), "candidate kind must be nonempty");
        let adapter = adapter
            .map(|path| path.to_str().context("adapter path must be UTF-8"))
            .transpose()?;
        let tx = self.transaction()?;
        let status: String =
            tx.query_row("SELECT status FROM versions WHERE id=?1", [parent], |row| {
                row.get(0)
            })?;
        ensure!(
            status == "active" || status == "known_good",
            "candidate parent must be known-good"
        );
        tx.execute("INSERT INTO versions(parent,kind,harness,adapter,status) VALUES(?1,?2,?3,?4,'candidate')",
            params![parent, kind, serde_json::to_string(harness)?, adapter])?;
        let id = tx.last_insert_rowid();
        // Harness-only versions preserve the adapter's data lineage.
        tx.execute(
            "INSERT INTO version_training(version,payload)
            SELECT ?1,t.payload FROM version_training t JOIN versions v ON v.id=t.version
            WHERE t.version=?2 AND v.adapter IS ?3",
            params![id, parent, adapter],
        )?;
        tx.commit()?;
        self.version(id)
    }

    /// The gate is trusted, but lineage, completeness and active-baseline CAS are rechecked here.
    pub fn admit(&self, report: &Admission) -> Result<()> {
        self.admit_with_training(report, None)
    }

    pub fn training_state(&self, version: i64) -> Result<Option<TrainingState>> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM version_training WHERE version=?1",
                [version],
                |r| r.get(0),
            )
            .optional()?;
        payload
            .map(|text| serde_json::from_str(&text).context("corrupt version training state"))
            .transpose()
    }

    /// Promotion and the consumed dataset window commit together, or neither does.
    pub fn admit_with_training(
        &self,
        report: &Admission,
        training: Option<&TrainingState>,
    ) -> Result<()> {
        let tx = self.transaction()?;
        let candidate = tx
            .query_row(
                &format!("SELECT {VERSION_COLUMNS} FROM versions WHERE id=?1"),
                [report.candidate],
                version_row,
            )
            .optional()?
            .map(|mut version| -> Result<Version> {
                resolve_harnesses(&tx, std::slice::from_mut(&mut version))?;
                Ok(version)
            })
            .transpose()?;
        let active: String =
            tx.query_row("SELECT value FROM meta WHERE key='active'", [], |row| {
                row.get(0)
            })?;
        let invalid = if candidate.is_none() {
            Some("candidate does not exist")
        } else if candidate.as_ref().is_some_and(|v| v.status != "candidate") {
            Some("version is not a pending candidate")
        } else if candidate
            .as_ref()
            .is_some_and(|v| v.parent != Some(report.baseline))
        {
            Some("candidate parent does not match the evaluated baseline")
        } else if active != report.baseline.to_string() {
            Some("admission is stale: active baseline changed")
        } else if training.is_some()
            && candidate
                .as_ref()
                .is_some_and(|v| v.kind != "lora" || v.adapter.is_none())
        {
            Some("training state requires a LoRA adapter candidate")
        } else if report.passed && !valid_report(report) {
            Some("passing admission is incomplete or internally invalid")
        } else {
            None
        };
        let activated = report.passed && invalid.is_none();
        if activated {
            if let Some(training) = training {
                ensure!(
                    !training.train_path.trim().is_empty(),
                    "training dataset path must be nonempty"
                );
                tx.execute(
                    "INSERT INTO version_training(version,payload) VALUES(?1,?2)",
                    params![report.candidate, serde_json::to_string(training)?],
                )?;
            }
            let changed = tx.execute(
                "UPDATE versions SET status='known_good' WHERE id=?1 AND status='active'",
                [report.baseline],
            )?;
            ensure!(changed == 1, "active baseline status is inconsistent");
            tx.execute(
                "UPDATE versions SET status='active' WHERE id=?1 AND status='candidate'",
                [report.candidate],
            )?;
            let changed = tx.execute(
                "UPDATE meta SET value=?1 WHERE key='active' AND value=?2",
                params![report.candidate.to_string(), report.baseline.to_string()],
            )?;
            ensure!(changed == 1, "active baseline changed during admission");
            tx.execute(
                "INSERT INTO transitions(previous,current,reason) VALUES(?1,?2,'admission')",
                params![report.baseline, report.candidate],
            )?;
        } else {
            tx.execute(
                "UPDATE versions SET status='rejected' WHERE id=?1 AND status='candidate'",
                [report.candidate],
            )?;
        }
        tx.execute("INSERT INTO admissions(candidate,baseline,report,activated,decision) VALUES(?1,?2,?3,?4,?5)",
            params![report.candidate, report.baseline, serde_json::to_string(report)?, activated, invalid.unwrap_or(if activated { "activated" } else { "gate rejected" })])?;
        tx.commit()?;
        if let Some(reason) = invalid {
            bail!("{reason}");
        }
        Ok(())
    }

    pub fn rollback(&self) -> Result<Version> {
        let tx = self.transaction()?;
        let active: String =
            tx.query_row("SELECT value FROM meta WHERE key='active'", [], |row| {
                row.get(0)
            })?;
        let active_id: i64 = active.parse()?;
        let parent: Option<i64> = tx.query_row(
            "SELECT parent FROM versions WHERE id=?1 AND status='active'",
            [active_id],
            |row| row.get(0),
        )?;
        let parent = parent.context("active version has no previous known-good version")?;
        let status: String =
            tx.query_row("SELECT status FROM versions WHERE id=?1", [parent], |row| {
                row.get(0)
            })?;
        ensure!(
            status == "known_good",
            "rollback target is not a previously admitted known-good version"
        );
        tx.execute(
            "UPDATE versions SET status='known_good' WHERE id=?1",
            [active_id],
        )?;
        tx.execute("UPDATE versions SET status='active' WHERE id=?1", [parent])?;
        let changed = tx.execute(
            "UPDATE meta SET value=?1 WHERE key='active' AND value=?2",
            params![parent.to_string(), active],
        )?;
        ensure!(changed == 1, "active version changed during rollback");
        tx.execute(
            "INSERT INTO transitions(previous,current,reason) VALUES(?1,?2,'rollback')",
            params![active_id, parent],
        )?;
        tx.commit()?;
        self.version(parent)
    }

    pub fn begin_trajectory(
        &self,
        version: i64,
        domain: &str,
        task: &str,
        prompt: &str,
    ) -> Result<i64> {
        ensure!(
            !domain.trim().is_empty() && !task.trim().is_empty(),
            "trajectory domain and task must be nonempty"
        );
        self.conn.execute(
            "INSERT INTO trajectories(version,domain,task,prompt) VALUES(?1,?2,?3,?4)",
            params![version, domain, task, prompt],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn event(&self, event: &Event) -> Result<()> {
        if let Some(score) = event.score {
            validate_score(score)?;
        }
        ensure!(!event.kind.trim().is_empty(), "event kind must be nonempty");
        let changed = self.conn.execute(
            "INSERT INTO events(trajectory,version,domain,task,step,kind,data,score)
            SELECT id,version,domain,task,?5,?6,?7,?8 FROM trajectories
            WHERE id=?1 AND version=?2 AND domain=?3 AND task=?4 AND finished_at IS NULL",
            params![
                event.trajectory,
                event.version,
                event.domain,
                event.task,
                event.step,
                event.kind,
                serde_json::to_string(&event.data)?,
                event.score
            ],
        )?;
        ensure!(
            changed == 1,
            "event must match an existing unfinished trajectory"
        );
        Ok(())
    }

    pub fn finish_trajectory(
        &self,
        id: i64,
        output: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        ensure!(
            output.is_some() || error.is_some(),
            "completed trajectory requires output or error"
        );
        let changed = self.conn.execute("UPDATE trajectories SET output=?2,error=?3,finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND finished_at IS NULL", params![id, output, error])?;
        ensure!(
            changed == 1,
            "trajectory does not exist or is already completed"
        );
        Ok(())
    }

    /// Record an implicit signal (a commit) unless a person already judged this trajectory with
    /// `/outcome`, a rating key or `/undo`. Returns whether it was recorded.
    pub fn implicit_outcome(&self, outcome: &Outcome) -> Result<bool> {
        let judged: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM outcomes WHERE trajectory=?1 AND source LIKE 'user%')",
            [outcome.trajectory],
            |row| row.get(0),
        )?;
        if judged {
            return Ok(false);
        }
        self.outcome(outcome)?;
        Ok(true)
    }

    pub fn outcome(&self, outcome: &Outcome) -> Result<()> {
        validate_score(outcome.score)?;
        ensure!(
            !outcome.source.trim().is_empty(),
            "outcome requires a nonempty external source"
        );
        let tx = self.transaction()?;
        let completed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM trajectories WHERE id=?1 AND finished_at IS NOT NULL)",
            [outcome.trajectory],
            |row| row.get(0),
        )?;
        ensure!(
            completed,
            "outcome requires an existing completed trajectory"
        );
        tx.execute("INSERT INTO outcomes(trajectory,score,source,reference) VALUES(?1,?2,?3,?4)
            ON CONFLICT(trajectory) DO UPDATE SET score=excluded.score,source=excluded.source,reference=excluded.reference,created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            params![outcome.trajectory, outcome.score, outcome.source, outcome.reference])?;
        tx.execute(
            "INSERT INTO outcome_audit(trajectory,score,source,reference) VALUES(?1,?2,?3,?4)",
            params![
                outcome.trajectory,
                outcome.score,
                outcome.source,
                outcome.reference
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Result<Value> {
        let mut statement = self.conn.prepare("SELECT t.id,t.version,t.domain,t.task,t.prompt,t.output,t.error,t.created_at,t.finished_at,o.score,o.source,o.reference
            FROM trajectories t LEFT JOIN outcomes o ON o.trajectory=t.id WHERE t.finished_at IS NOT NULL ORDER BY t.id DESC LIMIT ?1")?;
        let mut events = self.conn.prepare("SELECT step,kind,data,score,created_at FROM
            (SELECT id,step,kind,data,score,created_at FROM events WHERE trajectory=?1 ORDER BY id DESC LIMIT 128) ORDER BY id")?;
        let rows = statement.query_map([limit.min(200) as i64], |row| {
            let score: Option<f64> = row.get(9)?;
            let source: Option<String> = row.get(10)?;
            let reference: Option<String> = row.get(11)?;
            Ok(json!({"id": row.get::<_,i64>(0)?, "version":row.get::<_,i64>(1)?, "domain":row.get::<_,String>(2)?,
                "task":row.get::<_,String>(3)?, "prompt":row.get::<_,String>(4)?, "output":row.get::<_,Option<String>>(5)?,
                "error":row.get::<_,Option<String>>(6)?, "created_at":row.get::<_,String>(7)?, "finished_at":row.get::<_,String>(8)?,
                "outcome":score.map(|score| json!({"score":score,"source":source,"reference":reference}))}))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let mut row = row?;
            let trajectory = row["id"]
                .as_i64()
                .context("invalid trajectory identifier")?;
            let values = events.query_map([trajectory], |event| {
                let data: String = event.get(2)?;
                let data: Value = serde_json::from_str(&data).map_err(|error| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error)))?;
                Ok(json!({"step":event.get::<_,u32>(0)?,"kind":event.get::<_,String>(1)?,"data":data,"score":event.get::<_,Option<f64>>(3)?,"created_at":event.get::<_,String>(4)?}))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            row["events"] = json!(values);
            result.push(row);
        }
        Ok(json!(result))
    }

    pub fn metrics(&self) -> Result<Value> {
        let mut trajectories = self.conn.prepare("SELECT t.domain,t.task,t.version,count(*),count(o.score),avg(o.score)
            FROM trajectories t LEFT JOIN outcomes o ON o.trajectory=t.id WHERE t.finished_at IS NOT NULL GROUP BY t.domain,t.task,t.version ORDER BY t.domain,t.task,t.version")?;
        let outcomes = trajectories.query_map([], |row| Ok(json!({"domain":row.get::<_,String>(0)?,"task":row.get::<_,String>(1)?,"version":row.get::<_,i64>(2)?,
            "completed":row.get::<_,i64>(3)?,"scored":row.get::<_,i64>(4)?,"mean_score":row.get::<_,Option<f64>>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut events = self.conn.prepare("SELECT domain,task,version,step,kind,count(*),count(score),avg(score) FROM events GROUP BY domain,task,version,step,kind ORDER BY domain,task,version,step,kind")?;
        let events = events.query_map([], |row| Ok(json!({"domain":row.get::<_,String>(0)?,"task":row.get::<_,String>(1)?,"version":row.get::<_,i64>(2)?,
            "step":row.get::<_,u32>(3)?,"kind":row.get::<_,String>(4)?,"count":row.get::<_,i64>(5)?,"scored":row.get::<_,i64>(6)?,"mean_score":row.get::<_,Option<f64>>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"outcomes":outcomes,"events":events}))
    }

    /// Persist a tool-call recovery: the context before a malformed call and the later call that was
    /// schema-valid and executed successfully. Kept apart from `events` so the feedback model never
    /// sees raw training context. Returns false for duplicate or oversized (>256 KiB) records.
    pub fn tool_recovery(&self, trajectory: i64, version: i64, payload: &Value) -> Result<bool> {
        let text = serde_json::to_string(payload)?;
        if text.len() > 256 * 1024 {
            // Optional learning capture must not abort the coding turn. Never train on truncated context.
            return Ok(false);
        }
        let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
        let changed = self.conn.execute("INSERT OR IGNORE INTO tool_recoveries(trajectory,version,digest,payload) VALUES(?1,?2,?3,?4)",
            params![trajectory, version, digest, text])?;
        Ok(changed == 1)
    }

    pub fn tool_recoveries(&self) -> Result<Vec<(i64, Value)>> {
        let mut statement = self
            .conn
            .prepare("SELECT trajectory,payload FROM tool_recoveries ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (trajectory, payload) = row?;
            Ok((
                trajectory,
                serde_json::from_str(&payload).context("corrupt tool recovery payload")?,
            ))
        })
        .collect()
    }

    /// A bounded newest-first window for advisory repair retrieval, scoped before rows are loaded.
    pub fn recent_tool_recoveries(
        &self,
        project_root: &str,
        limit: usize,
    ) -> Result<Vec<(i64, Value)>> {
        ensure!(limit <= 1024, "repair lookup limit exceeds 1024");
        let mut statement = self.conn.prepare(
            "SELECT trajectory,payload FROM tool_recoveries \
             WHERE json_extract(payload,'$.project_root')=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![project_root, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (trajectory, payload) = row?;
            Ok((
                trajectory,
                serde_json::from_str(&payload).context("corrupt tool recovery payload")?,
            ))
        })
        .collect()
    }

    /// Persist scoped executable proof separately from call-form training targets and outcomes.
    /// A passing syntax check must never enter the externally verified-success harvest path.
    pub fn executable_repair(
        &self,
        trajectory: i64,
        version: i64,
        payload: &Value,
    ) -> Result<bool> {
        let record: crate::storage::repair_evidence::RepairRecord =
            serde_json::from_value(payload.clone()).context("invalid executable repair record")?;
        ensure!(
            record.valid(),
            "executable repair lacks failure/change/pass provenance"
        );
        let text = serde_json::to_string(&record)?;
        ensure!(
            text.len() <= 256 * 1024,
            "executable repair exceeds 256 KiB"
        );
        let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
        let trajectory_version: i64 = self.conn.query_row(
            "SELECT version FROM trajectories WHERE id=?1",
            [trajectory],
            |row| row.get(0),
        )?;
        ensure!(
            trajectory_version == version,
            "repair version does not match trajectory"
        );
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO executable_repairs(trajectory,version,project_root,digest,payload) VALUES(?1,?2,?3,?4,?5)",
            params![trajectory, version, record.project_root, digest, text])? == 1)
    }

    /// Bounded project-scoped retrieval for advisory reuse; does not create training labels.
    pub fn recent_executable_repairs(
        &self,
        project_root: &str,
        limit: usize,
    ) -> Result<Vec<(i64, Value)>> {
        ensure!(limit <= 1024, "repair lookup limit exceeds 1024");
        let mut statement = self.conn.prepare(
            "SELECT trajectory,payload FROM executable_repairs WHERE project_root=?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = statement.query_map(params![project_root, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (trajectory, payload) = row?;
            Ok((
                trajectory,
                serde_json::from_str(&payload).context("corrupt executable repair payload")?,
            ))
        })
        .collect()
    }

    pub fn trajectory_domain(&self, trajectory: i64) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT domain FROM trajectories WHERE id=?1",
            [trajectory],
            |row| row.get(0),
        )?)
    }

    /// Persist request/target snapshots ({"examples":[{"messages":[...],"tools":[...]}]})
    /// or a legacy conversation payload, for later externally verified harvesting.
    pub fn save_messages(&self, trajectory: i64, payload: &Value) -> Result<()> {
        let text = serde_json::to_string(payload)?;
        ensure!(
            text.len() <= 2 * 1024 * 1024,
            "trajectory messages exceed 2 MiB"
        );
        self.conn.execute(
            "INSERT INTO trajectory_messages(trajectory,payload) VALUES(?1,?2)
            ON CONFLICT(trajectory) DO UPDATE SET payload=excluded.payload",
            params![trajectory, text],
        )?;
        Ok(())
    }

    pub fn trajectory_messages(&self, trajectory: i64) -> Result<Option<Value>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT payload FROM trajectory_messages WHERE trajectory=?1",
                [trajectory],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|t| serde_json::from_str(&t).context("corrupt trajectory messages"))
            .transpose()
    }

    /// Trajectories with an externally assigned score >= `min_score` and saved messages, oldest first:
    /// (trajectory, version, domain, score, source, payload).
    pub fn verified_successes(&self, min_score: f64, limit: usize) -> Result<Vec<VerifiedSuccess>> {
        let mut statement = self.conn.prepare("SELECT t.id,t.version,t.domain,o.score,o.source,m.payload
            FROM trajectories t JOIN outcomes o ON o.trajectory=t.id JOIN trajectory_messages m ON m.trajectory=t.id
            WHERE t.finished_at IS NOT NULL AND o.score>=?1 AND o.source NOT LIKE 'check:%' ORDER BY t.id LIMIT ?2")?;
        let rows = statement.query_map(params![min_score, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, version, domain, score, source, payload) = row?;
            Ok((
                id,
                version,
                domain,
                score,
                source,
                serde_json::from_str(&payload).context("corrupt trajectory messages")?,
            ))
        })
        .collect()
    }

    /// Newest-first finished live-session trajectories with a perfect external score and saved
    /// messages: (trajectory, prompt, source). Payloads are loaded separately, only for matches.
    pub fn recent_verified_live(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        ensure!(limit <= 1024, "verified example lookup limit exceeds 1024");
        let mut statement = self.conn.prepare("SELECT t.id,t.prompt,o.source FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
            WHERE t.finished_at IS NOT NULL AND o.score>=100 AND o.source NOT LIKE 'check:%' AND t.domain NOT LIKE 'selfplay:%'
            AND t.task LIKE 'session:%' AND EXISTS(SELECT 1 FROM trajectory_messages m WHERE m.trajectory=t.id) ORDER BY t.id DESC LIMIT ?1")?;
        let rows =
            statement.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Highest finished trajectory id (0 when none).
    pub fn max_trajectory(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(id),0) FROM trajectories WHERE finished_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// Finished live (non self-play) trajectories after `id`.
    pub fn live_turns_after(&self, id: i64) -> Result<usize> {
        let n: i64 = self.conn.query_row("SELECT count(*) FROM trajectories WHERE id>?1 AND finished_at IS NOT NULL AND domain NOT LIKE 'selfplay:%'", [id], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Finished live trajectories with a perfect outcome from a source other than internal checks
    /// (`/outcome`, or passing behavioral checks). Session learning only uses these.
    pub fn verified_live_turns(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(DISTINCT t.id) FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
            WHERE t.finished_at IS NOT NULL AND t.domain NOT LIKE 'selfplay:%' AND o.score>=100 AND o.source NOT LIKE 'check:%'",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Record the user's approval (`user:session`, score 100) for every completed turn of one
    /// session, so `/improve session` can train on it. Turns that failed, were interrupted (no saved
    /// messages), or already carry a person's verdict or a lower external score are left unchanged.
    pub fn approve_session(&self, session_id: &str) -> Result<SessionApproval> {
        ensure!(!session_id.trim().is_empty(), "session id must be nonempty");
        let mut statement = self.conn.prepare(
            "SELECT t.id,t.error IS NOT NULL,
                EXISTS(SELECT 1 FROM trajectory_messages m WHERE m.trajectory=t.id),
                o.score,o.source
            FROM trajectories t LEFT JOIN outcomes o ON o.trajectory=t.id
            WHERE t.task=?1 AND t.finished_at IS NOT NULL AND t.domain NOT LIKE 'selfplay:%'
            ORDER BY t.id",
        )?;
        let turns = statement
            .query_map([format!("session:{session_id}")], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut approval = SessionApproval::default();
        for (trajectory, failed, saved, score, source) in turns {
            match (score, source) {
                _ if failed || !saved => approval.unusable += 1,
                (Some(score), _) if score >= 100.0 => approval.already += 1,
                (Some(_), Some(_)) => approval.judged += 1,
                _ => {
                    self.outcome(&Outcome {
                        trajectory,
                        score: 100.0,
                        source: "user:session".into(),
                        reference: Some("approved with /improve session".into()),
                    })?;
                    approval.approved += 1;
                }
            }
        }
        Ok(approval)
    }

    /// Scored live trajectories of one version: (count, mean score). Self-play is excluded so the
    /// number reflects real use.
    pub fn live_score(&self, version: i64) -> Result<(usize, Option<f64>)> {
        let (n, mean): (i64, Option<f64>) = self.conn.query_row("SELECT count(o.score),avg(o.score) FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
            WHERE t.version=?1 AND t.domain NOT LIKE 'selfplay:%' AND o.source NOT LIKE 'check:%'", [version], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok((n as usize, mean))
    }

    /// Task-matched live results for a candidate and its parent: one `(parent, candidate)` score pair per
    /// prompt that both versions answered with an explicit score from the same source class. Only
    /// explicit scores count (`user*` verdicts, passing `checks:behavioral`); commits and `check:*`
    /// verifier outcomes never do. Each side is averaged per prompt first. A prompt scored in both
    /// classes counts once, preferring the user verdict. Prompts never leave this query: callers
    /// receive only numbers.
    pub fn matched_live_scores(&self, candidate: i64, parent: i64) -> Result<Vec<(f64, f64)>> {
        let mut statement = self.conn.prepare(
            "WITH scored AS (
                SELECT t.version AS version,t.prompt AS prompt,
                    CASE WHEN o.source IN ('user','user:rating','user:undo','user:session') THEN 'user' ELSE 'checks' END AS class,
                    avg(o.score) AS score
                FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
                WHERE t.version IN (?1,?2) AND t.finished_at IS NOT NULL AND t.error IS NULL AND t.domain NOT LIKE 'selfplay:%'
                    AND (o.source IN ('user','user:rating','user:undo','user:session') OR o.source='checks:behavioral')
                GROUP BY t.version,t.prompt,class),
            paired AS (
                SELECT p.score AS parent,c.score AS candidate,c.prompt AS prompt,c.class AS class
                FROM scored c JOIN scored p ON p.prompt=c.prompt AND p.class=c.class
                WHERE c.version=?1 AND p.version=?2)
            SELECT parent,candidate FROM (
                SELECT parent,candidate,prompt,class,row_number() OVER (PARTITION BY prompt ORDER BY CASE class WHEN 'user' THEN 0 ELSE 1 END,class) AS rank
                FROM paired)
            WHERE rank=1 ORDER BY parent,candidate",
        )?;
        let rows = statement.query_map([candidate, parent], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Self-play pass rates by family for one version: (family, passed, total), from verifier outcomes.
    pub fn selfplay_rates(&self, version: i64) -> Result<Vec<(String, usize, usize)>> {
        let mut statement = self.conn.prepare("SELECT t.domain,sum(o.score>=100),count(*) FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
            WHERE t.version=?1 AND t.domain LIKE 'selfplay:%' GROUP BY t.domain ORDER BY t.domain")?;
        let rows = statement.query_map([version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as usize,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All events of one trajectory in order (kind, step, data), for step-level learning.
    pub fn trajectory_events(&self, trajectory: i64) -> Result<Vec<(String, u32, Value)>> {
        let mut statement = self
            .conn
            .prepare("SELECT kind,step,data FROM events WHERE trajectory=?1 ORDER BY id")?;
        let rows = statement.query_map([trajectory], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (k, s, d) = row?;
            Ok((k, s, serde_json::from_str(&d).unwrap_or(Value::Null)))
        })
        .collect()
    }

    /// Scored trajectories with their ids (for probe training): (trajectory, score).
    pub fn scored_trajectories(&self, limit: usize) -> Result<Vec<(i64, f64)>> {
        let mut statement = self.conn.prepare("SELECT t.id,o.score FROM trajectories t JOIN outcomes o ON o.trajectory=t.id WHERE o.source NOT LIKE 'check:%' ORDER BY t.id DESC LIMIT ?1")?;
        let rows = statement.query_map([limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        ensure!(
            !matches!(key, "active" | "model" | "use_case"),
            "reserved state metadata cannot be changed"
        );
        self.conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
