//! `dial models status` and the Get models group of `/models`: one read-only snapshot per downloaded
//! local model (MLX or GGUF) with its format, decode speed from recorded requests, memory-fit
//! estimate, learning-store adapter and last gate, and what improvement is waiting on.
//!
//! Opening it never loads a model, starts Python, or touches the network. Sources that are missing
//! or unreadable are reported as unknown (`null` in JSON, "unknown" in text), never as errors and
//! never estimated. Learning stores are opened read-only with SQL, so a store that does not exist
//! is not created.

use crate::config::types::Config;
use crate::llm::catalog;
use crate::llm::local_fit::{self, Architecture, Estimate, Fit};
use crate::llm::local_format::{self, LocalFormat};
use crate::stats::{self, DecodeSpeed};
use crate::storage::paths::Layout;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const GIB: f64 = (1u64 << 30) as f64;
/// Context used for the fit estimate when the configuration does not request one.
pub const DEFAULT_CONTEXT: u64 = 32768;

/// Improvement and context settings the dashboard compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub context_tokens: u64,
    pub lora_min_examples: u64,
    pub min_new_turns: u64,
    pub cycle_interval_secs: u64,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            context_tokens: config
                .requested_context_window()
                .map_or(DEFAULT_CONTEXT, u64::from),
            lora_min_examples: u64::from(config.improve.lora_min_examples),
            min_new_turns: u64::from(config.improve.min_new_turns),
            cycle_interval_secs: config.improve.interval_secs,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub generated_ms: i64,
    pub memory_bytes: Option<u64>,
    /// `detected` or `unknown`.
    pub memory_source: &'static str,
    /// `available`, `missing` (no stats database yet) or `unreadable`.
    pub stats: &'static str,
    pub models: Vec<ModelRow>,
}

#[derive(Debug, Serialize)]
pub struct ModelRow {
    pub label: String,
    pub path: String,
    pub source: String,
    /// `MLX` or `GGUF`; `null` when the path is no longer a recognised model.
    pub format: Option<&'static str>,
    pub speed: DecodeSpeed,
    pub memory: Memory,
    pub improvement: Improvement,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Memory {
    pub weights_bytes: Option<u64>,
    pub resident_bytes: Option<u64>,
    pub context_tokens: Option<u64>,
    /// `unknown` for GGUF (no architecture is read from the file) and when memory is unknown.
    pub fit: Fit,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Improvement {
    /// `found`, `no_store` (nothing recorded for this model) or `unreadable`.
    pub store: &'static str,
    pub store_dir: Option<String>,
    pub version: Option<i64>,
    pub kind: Option<String>,
    pub adapter: Option<String>,
    pub last_gate: Option<Gate>,
    /// Finished, verified live turns since the last cycle watermark: the examples LoRA draws on.
    pub verified_turns: Option<u64>,
    pub lora_min_examples: u64,
    /// Finished live turns since the last cycle watermark.
    pub new_turns: Option<u64>,
    pub min_new_turns: u64,
    /// Seconds until the next cycle may start; `0` when none is recorded.
    pub cooldown_secs: Option<u64>,
    /// Recorded blockers. `None` when unknown; empty when none is recorded.
    pub waiting: Option<Vec<String>>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Gate {
    pub candidate: i64,
    pub baseline: i64,
    pub passed: bool,
    pub activated: bool,
    pub decision: String,
    /// Primary suite scores (percent) for the baseline and the candidate.
    pub primary_baseline: Option<f64>,
    pub primary_candidate: Option<f64>,
}

/// Reads the dashboard snapshot. Never fails: unavailable sources become unknown fields.
pub fn collect(layout: &Layout, settings: &Settings, generated_ms: i64) -> Dashboard {
    let user_dir = &layout.user_dir;
    let memory_bytes = local_fit::physical_memory_bytes();
    let (db, stats_state) = match stats_db(user_dir) {
        Ok(Some(db)) => (Some(db), "available"),
        Ok(None) => (None, "missing"),
        Err(_) => (None, "unreadable"),
    };
    let mut models: Vec<(String, String, String, PathBuf, LocalFormat)> = Vec::new();
    for entry in catalog::downloaded_local_models(user_dir) {
        let path = PathBuf::from(&entry.selection.model);
        if let Some(format @ (LocalFormat::Mlx | LocalFormat::Gguf)) = local_format::detect(&path)
            && !models.iter().any(|known| known.3 == path)
        {
            models.push((
                entry.label,
                entry.source,
                path.to_string_lossy().into_owned(),
                path,
                format,
            ));
        }
    }
    if let Some(db) = &db {
        for name in db.model_names().unwrap_or_default() {
            let path = PathBuf::from(&name);
            if models.iter().any(|known| known.3 == path) {
                continue;
            }
            if let Some(format @ (LocalFormat::Mlx | LocalFormat::Gguf)) =
                local_format::detect(&path)
            {
                let label = path
                    .file_name()
                    .map_or_else(|| name.clone(), |n| n.to_string_lossy().into_owned());
                models.push((
                    label,
                    "recorded requests".into(),
                    name.clone(),
                    path,
                    format,
                ));
            }
        }
    }
    let stores = learning_stores(&layout.state_dir);
    let now_secs = u64::try_from(generated_ms / 1000).unwrap_or(0);
    let rows = models
        .into_iter()
        .map(|(label, source, path_text, path, format)| {
            let speed = match &db {
                Some(db) => db
                    .decode_speed(&[path_text.as_str(), label.as_str()])
                    .unwrap_or_default(),
                None => DecodeSpeed::default(),
            };
            ModelRow {
                memory: memory(&path, format, settings, memory_bytes),
                improvement: improvement(&stores, &[&path_text, &label], settings, now_secs),
                format: Some(format.label()),
                label,
                source,
                path: path_text,
                speed,
            }
        })
        .collect();
    Dashboard {
        generated_ms,
        memory_bytes,
        memory_source: if memory_bytes.is_some() {
            "detected"
        } else {
            "unknown"
        },
        stats: stats_state,
        models: rows,
    }
}

/// `Ok(None)` when no stats database exists yet; it is never created by reading.
fn stats_db(user_dir: &Path) -> Result<Option<stats::Db>> {
    let path = stats::db_path(user_dir);
    if !path.is_file() {
        return Ok(None);
    }
    stats::Db::open_read_only(&path).map(Some)
}

fn memory(
    path: &Path,
    format: LocalFormat,
    settings: &Settings,
    memory_bytes: Option<u64>,
) -> Memory {
    let unknown = |weights_bytes| Memory {
        weights_bytes,
        resident_bytes: None,
        context_tokens: None,
        fit: Fit::Unknown,
    };
    // The same layout sources as `dial models recommend`: config.json for MLX, the header for GGUF.
    let (architecture, weights) = match format {
        LocalFormat::Gguf => (
            local_format::gguf_info(path).and_then(|info| Architecture::from_gguf(&info)),
            local_format::gguf_weight_bytes(path),
        ),
        _ => (
            fs::read_to_string(path.join("config.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|config| Architecture::from_config(&config)),
            local_fit::safetensors_bytes(path),
        ),
    };
    match (architecture, weights) {
        (Some(arch), Some(weights)) => {
            let Estimate {
                context_tokens,
                resident_bytes,
                fit,
                ..
            } = local_fit::estimate(weights, &arch, settings.context_tokens, None, memory_bytes);
            Memory {
                weights_bytes: Some(weights),
                resident_bytes: Some(resident_bytes),
                context_tokens: Some(context_tokens),
                fit,
            }
        }
        _ => unknown(weights),
    }
}

/// A learning store found under `<state_dir>/models/*/state.sqlite3`, opened read-only.
struct LearningStore {
    dir: PathBuf,
    model: String,
    conn: Connection,
}

/// Opens every existing learning store read-only. Returns the stores and how many were unreadable.
fn learning_stores(state_dir: &Path) -> (Vec<LearningStore>, usize) {
    let mut stores = Vec::new();
    let mut unreadable = 0;
    let Ok(dirs) = fs::read_dir(state_dir.join("models")) else {
        return (stores, unreadable);
    };
    for dir in dirs.filter_map(Result::ok).map(|entry| entry.path()) {
        let file = dir.join("state.sqlite3");
        if !file.is_file() {
            continue;
        }
        let opened = Connection::open_with_flags(
            &file,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|conn| {
            conn.busy_timeout(std::time::Duration::from_secs(1))?;
            let model: Option<String> = conn
                .query_row("SELECT value FROM meta WHERE key='model'", [], |row| {
                    row.get(0)
                })
                .optional()?;
            Ok((conn, model))
        });
        match opened {
            Ok((conn, Some(model))) => stores.push(LearningStore { dir, model, conn }),
            _ => unreadable += 1,
        }
    }
    (stores, unreadable)
}

fn improvement(
    (stores, unreadable): &(Vec<LearningStore>, usize),
    names: &[&str],
    settings: &Settings,
    now_secs: u64,
) -> Improvement {
    let mut base = Improvement {
        store: "no_store",
        store_dir: None,
        version: None,
        kind: None,
        adapter: None,
        last_gate: None,
        verified_turns: None,
        lora_min_examples: settings.lora_min_examples,
        new_turns: None,
        min_new_turns: settings.min_new_turns,
        cooldown_secs: None,
        waiting: Some(vec!["no recorded turns for this model yet".into()]),
    };
    // Several use cases can share one model: the store changed most recently is the one shown.
    let matching = stores
        .iter()
        .filter(|store| names.contains(&store.model.as_str()))
        .filter_map(|store| {
            let newest: i64 = store
                .conn
                .query_row("SELECT COALESCE(MAX(id),0) FROM versions", [], |row| {
                    row.get(0)
                })
                .ok()?;
            Some((newest, store))
        })
        .max_by_key(|(newest, _)| *newest);
    let Some((_, store)) = matching else {
        // An unreadable store might belong to this model, so it is unknown rather than absent.
        if *unreadable > 0 {
            base.store = "unreadable";
            base.waiting = None;
        }
        return base;
    };
    base.store_dir = Some(store.dir.to_string_lossy().into_owned());
    match read_store(&store.conn, settings, now_secs) {
        Ok(read) => {
            base.store = "found";
            base.waiting = Some(read.waiting());
            base.version = read.version;
            base.kind = read.kind;
            base.adapter = read.adapter;
            base.last_gate = read.gate;
            base.verified_turns = Some(read.verified_turns);
            base.new_turns = Some(read.new_turns);
            base.cooldown_secs = Some(read.cooldown_secs);
            base
        }
        Err(_) => {
            base.store = "unreadable";
            base.store_dir = None;
            base.waiting = None;
            base
        }
    }
}

struct StoreRead {
    version: Option<i64>,
    kind: Option<String>,
    adapter: Option<String>,
    gate: Option<Gate>,
    verified_turns: u64,
    new_turns: u64,
    cooldown_secs: u64,
    settings: Settings,
}

impl StoreRead {
    fn waiting(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.verified_turns < self.settings.lora_min_examples {
            out.push(format!(
                "{}/{} verified turns for LoRA",
                self.verified_turns, self.settings.lora_min_examples
            ));
        }
        if self.new_turns < self.settings.min_new_turns {
            out.push(format!(
                "{}/{} new turns for a cycle",
                self.new_turns, self.settings.min_new_turns
            ));
        }
        if self.cooldown_secs > 0 {
            out.push(format!(
                "cooldown {} remaining",
                duration(self.cooldown_secs)
            ));
        }
        out
    }
}

/// Reads one learning store with SELECTs only; the schema is `storage::store`'s.
fn read_store(
    conn: &Connection,
    settings: &Settings,
    now_secs: u64,
) -> rusqlite::Result<StoreRead> {
    let meta = |key: &str| -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
    };
    let active: Option<(i64, String, Option<String>)> = conn
        .query_row(
            "SELECT id, kind, adapter FROM versions WHERE status='active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let gate = conn
        .query_row(
            "SELECT a.report, a.activated, a.decision FROM admissions a
             JOIN versions v ON v.id=a.candidate WHERE v.kind='lora' ORDER BY a.id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .and_then(|(report, activated, decision)| {
            let admission: crate::config::types::Admission = serde_json::from_str(&report).ok()?;
            let primary = admission.scores.iter().find(|score| score.primary);
            Some(Gate {
                candidate: admission.candidate,
                baseline: admission.baseline,
                passed: admission.passed,
                activated,
                decision,
                primary_baseline: primary.map(|score| score.baseline),
                primary_candidate: primary.map(|score| score.candidate),
            })
        });
    let watermark = meta("last_cycle_trajectory")?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let verified_turns: i64 = conn.query_row(
        "SELECT count(DISTINCT t.id) FROM trajectories t JOIN outcomes o ON o.trajectory=t.id
         WHERE t.id>?1 AND t.finished_at IS NOT NULL AND t.domain NOT LIKE 'selfplay:%'
           AND o.score>=100 AND o.source NOT LIKE 'check:%'",
        [watermark],
        |row| row.get(0),
    )?;
    let new_turns: i64 = conn.query_row(
        "SELECT count(*) FROM trajectories WHERE id>?1 AND finished_at IS NOT NULL
           AND domain NOT LIKE 'selfplay:%'",
        [watermark],
        |row| row.get(0),
    )?;
    let last_attempt = meta("last_cycle_attempt")?.and_then(|value| value.parse::<u64>().ok());
    let cooldown_secs = last_attempt.map_or(0, |last| {
        (last + settings.cycle_interval_secs).saturating_sub(now_secs)
    });
    Ok(StoreRead {
        version: active.as_ref().map(|(id, _, _)| *id),
        kind: active.as_ref().map(|(_, kind, _)| kind.clone()),
        adapter: active.and_then(|(_, _, adapter)| adapter),
        gate,
        verified_turns: u64::try_from(verified_turns).unwrap_or(0),
        new_turns: u64::try_from(new_turns).unwrap_or(0),
        cooldown_secs,
        settings: *settings,
    })
}

/// `1h 02m`, `4m 10s`, or `9s`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// Speed line for humans: `34.1 tok/s median · recent 36.0 (up) · 12 samples`.
pub fn speed_text(speed: &DecodeSpeed) -> String {
    let Some(samples) = speed.samples else {
        return "unknown (stats unavailable)".into();
    };
    let Some(median) = speed.median_tok_per_s else {
        return format!("unknown ({samples} recorded requests with output and timing)");
    };
    let mut out = format!("{median:.1} tok/s median");
    if let Some(recent) = speed.recent_median_tok_per_s {
        out.push_str(&format!(" · recent {recent:.1}"));
        if let Some(trend) = speed.trend {
            out.push_str(&format!(" ({trend})"));
        }
    }
    out.push_str(&format!(" · {samples} samples"));
    out
}

/// Memory line for humans: `fits · 5.2 GiB resident at 32768 tokens`.
pub fn memory_text(memory: &Memory) -> String {
    let fit = match memory.fit {
        Fit::Fits => "fits",
        Fit::Tight => "tight",
        Fit::WontFit => "will not fit",
        Fit::Unknown => "unknown",
    };
    match (memory.resident_bytes, memory.context_tokens) {
        (Some(resident), Some(context)) => {
            format!(
                "{fit} · {:.1} GiB resident at {context} tokens",
                resident as f64 / GIB
            )
        }
        _ => match memory.weights_bytes {
            Some(weights) => format!("{fit} · weights {:.1} GiB", weights as f64 / GIB),
            None => fit.to_string(),
        },
    }
}

/// Adapter/version line for humans, or the reason it is unknown.
pub fn version_text(improvement: &Improvement) -> String {
    match improvement.store {
        "no_store" => "no learning store for this model".into(),
        "unreadable" => "unknown (learning store unreadable)".into(),
        _ => match (improvement.version, &improvement.kind) {
            (Some(version), Some(kind)) => match &improvement.adapter {
                Some(adapter) => format!("version {version} ({kind}) · adapter {adapter}"),
                None => format!("version {version} ({kind}) · no adapter"),
            },
            _ => "unknown".into(),
        },
    }
}

/// Gate line for humans.
pub fn gate_text(improvement: &Improvement) -> String {
    match &improvement.last_gate {
        None if improvement.store == "found" => "no LoRA gate decision recorded".into(),
        None => "unknown".into(),
        Some(gate) => {
            let outcome = if gate.passed { "passed" } else { "rejected" };
            let score = match (gate.primary_baseline, gate.primary_candidate) {
                (Some(base), Some(candidate)) => format!(" · primary {base:.1} → {candidate:.1}"),
                _ => String::new(),
            };
            let activation = if gate.activated {
                "activated"
            } else {
                "not activated"
            };
            format!("{outcome}{score} · {activation} ({})", gate.decision)
        }
    }
}

/// Waiting line for humans.
pub fn waiting_text(improvement: &Improvement) -> String {
    match &improvement.waiting {
        None => "unknown".into(),
        Some(items) if items.is_empty() => "no recorded blocker".into(),
        Some(items) => items.join("; "),
    }
}

/// `dial models status [--json]`.
pub fn run_status(cwd: &Path, json: bool, overlay: Option<toml::Table>) -> Result<i32> {
    let layout = Layout::discover(cwd)?;
    let config = Config::load(&layout, overlay)?;
    let dashboard = collect(&layout, &Settings::from_config(&config), stats::now_ms());
    if json {
        println!("{}", serde_json::to_string_pretty(&dashboard)?);
    } else {
        print!("{}", render_text(&dashboard));
    }
    Ok(0)
}

pub fn render_text(dashboard: &Dashboard) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let memory = match dashboard.memory_bytes {
        Some(bytes) => format!("{:.1} GiB {}", bytes as f64 / GIB, dashboard.memory_source),
        None => "unknown".into(),
    };
    let _ = writeln!(
        out,
        "Local models: {} · memory {memory} · stats {}",
        dashboard.models.len(),
        dashboard.stats
    );
    if dashboard.models.is_empty() {
        out.push_str("no downloaded MLX or GGUF models found\n");
    }
    for row in &dashboard.models {
        let _ = writeln!(
            out,
            "\n{} [{}]",
            row.label,
            row.format.unwrap_or("unknown format")
        );
        let _ = writeln!(out, "  speed     {}", speed_text(&row.speed));
        let _ = writeln!(out, "  memory    {}", memory_text(&row.memory));
        let _ = writeln!(out, "  version   {}", version_text(&row.improvement));
        let _ = writeln!(out, "  gate      {}", gate_text(&row.improvement));
        let _ = writeln!(out, "  waiting   {}", waiting_text(&row.improvement));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Admission, SuiteScore};

    fn settings() -> Settings {
        Settings {
            context_tokens: 4096,
            lora_min_examples: 24,
            min_new_turns: 5,
            cycle_interval_secs: 3600,
        }
    }

    fn layout(root: &Path) -> Layout {
        Layout {
            project_root: root.join("project"),
            project_dir: root.join("project/.dial"),
            user_dir: root.join("user"),
            state_dir: root.join("state"),
        }
    }

    fn write_mlx(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("config.json"),
            r#"{"model_type":"llama","num_hidden_layers":2,"num_attention_heads":4,"num_key_value_heads":4,"hidden_size":64,"max_position_embeddings":8192}"#,
        )
        .unwrap();
        fs::write(dir.join("model.safetensors"), vec![0u8; 4096]).unwrap();
    }

    #[test]
    fn speed_summary_medians_and_trend_are_exact() {
        // Forty samples: the older twenty at 10 tok/s, the newest twenty at 20 tok/s.
        let rising: Vec<f64> = (0..40).map(|i| if i < 20 { 10.0 } else { 20.0 }).collect();
        let speed = stats::speed_summary(&rising);
        assert_eq!(speed.samples, Some(40));
        assert_eq!(speed.median_tok_per_s, Some(15.0));
        assert_eq!(speed.recent_median_tok_per_s, Some(20.0));
        assert_eq!(speed.previous_median_tok_per_s, Some(10.0));
        assert_eq!(speed.trend, Some("up"));
        assert_eq!(speed.history.len(), 40);

        let short = stats::speed_summary(&[5.0, 7.0]);
        assert_eq!(short.median_tok_per_s, Some(6.0));
        assert_eq!(short.previous_median_tok_per_s, None);
        assert_eq!(short.trend, None);
        assert_eq!(stats::speed_summary(&[]).samples, Some(0));
    }

    #[test]
    fn missing_stats_and_stores_are_unknown_not_errors() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let model = root.path().join("models/mlx-model");
        write_mlx(&model);
        let stores = learning_stores(&layout.state_dir);
        let memory = memory(&model, LocalFormat::Mlx, &settings(), Some(16 << 30));
        assert_eq!(memory.weights_bytes, Some(4096));
        assert!(memory.resident_bytes.is_some());
        assert_eq!(memory.context_tokens, Some(4096));
        let improvement = improvement(&stores, &["mlx-model"], &settings(), 1_000);
        assert_eq!(improvement.store, "no_store");
        assert_eq!(improvement.verified_turns, None);
        assert_eq!(improvement.version, None);
        assert!(improvement.store_dir.is_none());
        assert!(
            !layout.state_dir.join("models").exists(),
            "reading must not create stores"
        );
        assert!(!stats::db_path(&layout.user_dir).exists());
        assert!(stats_db(&layout.user_dir).unwrap().is_none());
        assert_eq!(DecodeSpeed::default().samples, None);
        assert_eq!(
            speed_text(&DecodeSpeed::default()),
            "unknown (stats unavailable)"
        );
    }

    #[test]
    fn unknown_memory_reports_unknown_fit_and_gguf_is_sized_from_its_header() {
        use crate::llm::local_format::gguf_metadata_tests::{header, string, uint32};
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("m");
        write_mlx(&model);
        let no_memory = memory(&model, LocalFormat::Mlx, &settings(), None);
        assert_eq!(no_memory.fit, Fit::Unknown);
        assert!(no_memory.resident_bytes.is_some());

        let sized = root.path().join("s");
        fs::create_dir(&sized).unwrap();
        let bytes = header(&[
            ("general.architecture", string("llama")),
            ("llama.block_count", uint32(2)),
            ("llama.attention.head_count", uint32(4)),
            ("llama.attention.head_count_kv", uint32(4)),
            ("llama.embedding_length", uint32(64)),
            ("llama.context_length", uint32(8192)),
        ]);
        fs::write(sized.join("model-Q8_0.gguf"), &bytes).unwrap();
        let row = memory(&sized, LocalFormat::Gguf, &settings(), Some(16 << 30));
        assert_eq!(row.weights_bytes, Some(bytes.len() as u64));
        assert!(
            row.resident_bytes
                .is_some_and(|resident| resident > bytes.len() as u64)
        );
        assert_eq!(row.fit, Fit::Fits);

        // A header without a transformer layout still reports the file size, but no estimate.
        let gguf = root.path().join("g");
        fs::create_dir(&gguf).unwrap();
        fs::write(gguf.join("model-Q4.gguf"), b"GGUF\x03\0\0\0rest").unwrap();
        let row = memory(&gguf, LocalFormat::Gguf, &settings(), Some(16 << 30));
        assert_eq!(row.fit, Fit::Unknown);
        assert_eq!(row.resident_bytes, None);
        assert_eq!(row.weights_bytes, Some(12));
    }

    #[test]
    fn store_reads_adapter_gate_and_what_is_waiting() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let dir = state.join("models/abc");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("state.sqlite3")).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE versions(id INTEGER PRIMARY KEY, parent INTEGER, kind TEXT NOT NULL,
               harness TEXT NOT NULL, adapter TEXT, status TEXT NOT NULL);
             CREATE TABLE admissions(id INTEGER PRIMARY KEY, candidate INTEGER NOT NULL,
               baseline INTEGER NOT NULL, report TEXT NOT NULL, activated INTEGER NOT NULL,
               decision TEXT NOT NULL);
             CREATE TABLE trajectories(id INTEGER PRIMARY KEY, version INTEGER NOT NULL,
               domain TEXT NOT NULL, finished_at TEXT);
             CREATE TABLE outcomes(trajectory INTEGER PRIMARY KEY, score REAL NOT NULL,
               source TEXT NOT NULL);
             INSERT INTO meta VALUES('model', 'mlx-model'), ('use_case', 'coding'),
               ('last_cycle_trajectory', '10'), ('last_cycle_attempt', '1000');
             INSERT INTO versions VALUES(1, NULL, 'harness', '{}', NULL, 'known_good'),
               (2, 1, 'lora', '{}', '/adapters/2', 'active');",
        )
        .unwrap();
        let report = Admission {
            candidate: 2,
            baseline: 1,
            passed: true,
            reason: "ok".into(),
            scores: vec![SuiteScore {
                name: "primary".into(),
                primary: true,
                baseline: 50.0,
                candidate: 62.5,
                cases: 8,
                candidate_only: 2,
                baseline_only: 0,
                tier: None,
            }],
        };
        conn.execute(
            "INSERT INTO admissions VALUES(1, 2, 1, ?1, 1, 'activated')",
            [serde_json::to_string(&report).unwrap()],
        )
        .unwrap();
        // Ten finished live turns after the watermark: three verified, the rest unverified.
        for id in 11..=20 {
            conn.execute(
                "INSERT INTO trajectories VALUES(?1, 2, 'live', '2026-01-01')",
                [id],
            )
            .unwrap();
        }
        for id in 11..=13 {
            conn.execute("INSERT INTO outcomes VALUES(?1, 100, 'user')", [id])
                .unwrap();
        }
        drop(conn);

        let stores = learning_stores(&state);
        // Cooldown: attempt at 1000 + 3600 interval, now 2000 → 2600 s left.
        let found = improvement(&stores, &["mlx-model"], &settings(), 2_000);
        assert_eq!(found.store, "found");
        assert_eq!(found.version, Some(2));
        assert_eq!(found.kind.as_deref(), Some("lora"));
        assert_eq!(found.adapter.as_deref(), Some("/adapters/2"));
        assert_eq!(found.verified_turns, Some(3));
        assert_eq!(found.new_turns, Some(10));
        assert_eq!(found.cooldown_secs, Some(2600));
        let gate = found.last_gate.as_ref().unwrap();
        assert_eq!(gate.primary_candidate, Some(62.5));
        assert!(gate.passed && gate.activated);
        let waiting = found.waiting.clone().unwrap();
        assert_eq!(
            waiting,
            vec![
                "3/24 verified turns for LoRA".to_owned(),
                "cooldown 43m 20s remaining".to_owned()
            ]
        );
        assert!(gate_text(&found).contains("primary 50.0 → 62.5"));
        assert!(version_text(&found).contains("adapter /adapters/2"));

        let other = improvement(&stores, &["unrelated"], &settings(), 2_000);
        assert_eq!(other.store, "no_store");
    }

    #[test]
    fn unknown_fields_render_as_unknown_text() {
        let unknown = Improvement {
            store: "unreadable",
            store_dir: None,
            version: None,
            kind: None,
            adapter: None,
            last_gate: None,
            verified_turns: None,
            lora_min_examples: 24,
            new_turns: None,
            min_new_turns: 5,
            cooldown_secs: None,
            waiting: None,
        };
        assert_eq!(waiting_text(&unknown), "unknown");
        assert_eq!(
            version_text(&unknown),
            "unknown (learning store unreadable)"
        );
        assert_eq!(gate_text(&unknown), "unknown");
        let json = serde_json::to_value(&unknown).unwrap();
        assert!(json["waiting"].is_null());
        assert!(json["verified_turns"].is_null());
    }

    #[test]
    fn dashboard_json_has_null_speed_when_stats_are_missing() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let model = root.path().join("hub/mlx-model");
        write_mlx(&model);
        let stores = learning_stores(&layout.state_dir);
        let row = ModelRow {
            label: "mlx-model".into(),
            path: model.to_string_lossy().into_owned(),
            source: "test".into(),
            format: Some("MLX"),
            speed: DecodeSpeed::default(),
            memory: memory(&model, LocalFormat::Mlx, &settings(), None),
            improvement: improvement(&stores, &["mlx-model"], &settings(), 0),
        };
        let dashboard = Dashboard {
            generated_ms: 0,
            memory_bytes: None,
            memory_source: "unknown",
            stats: "missing",
            models: vec![row],
        };
        let json = serde_json::to_value(&dashboard).unwrap();
        assert_eq!(json["stats"], "missing");
        assert!(json["models"][0]["speed"]["samples"].is_null());
        assert!(json["models"][0]["speed"]["median_tok_per_s"].is_null());
        assert_eq!(json["models"][0]["memory"]["fit"], "unknown");
        assert_eq!(json["models"][0]["improvement"]["store"], "no_store");
        let text = render_text(&dashboard);
        assert!(text.contains("speed     unknown (stats unavailable)"));
        assert!(text.contains("[MLX]"));
    }

    #[test]
    fn speed_from_recorded_requests_uses_only_successful_timed_output() {
        let root = tempfile::tempdir().unwrap();
        let mut db = stats::Db::open(&root.path().join("stats.sqlite3")).unwrap();
        let record = |ts: i64, output: u64, duration_ms: u64, error: bool| stats::RequestRecord {
            ts_ms: ts,
            session: "s".into(),
            project: "p".into(),
            model: "/models/a".into(),
            output,
            duration_ms,
            error,
            reported: true,
            finish_reason: "stop".into(),
            ..Default::default()
        };
        db.record_request(&record(1, 100, 2000, false)).unwrap();
        db.record_request(&record(2, 300, 2000, false)).unwrap();
        db.record_request(&record(3, 999, 1000, true)).unwrap();
        db.record_request(&record(4, 0, 1000, false)).unwrap();
        drop(db);
        let read = stats::Db::open_read_only(&root.path().join("stats.sqlite3")).unwrap();
        let speed = read.decode_speed(&["/models/a"]).unwrap();
        assert_eq!(speed.samples, Some(2));
        assert_eq!(speed.history, vec![50.0, 150.0]);
        assert_eq!(speed.median_tok_per_s, Some(100.0));
        assert_eq!(read.decode_speed(&["/models/b"]).unwrap().samples, Some(0));
        assert_eq!(read.model_names().unwrap(), vec!["/models/a".to_owned()]);
    }
}
