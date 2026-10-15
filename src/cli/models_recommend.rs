//! `dial models recommend`: RAM-aware fit estimates for downloaded local MLX checkpoints.
//!
//! Reads weights, `config.json` and recorded `dial reliability check` reports from disk. It
//! loads no model and makes no network calls. Estimates come from `llm::local_fit`.

use crate::config::types::Config;
use crate::llm::catalog;
use crate::llm::local_fit::{self, Architecture, Estimate, Fit};
use crate::storage::paths::Layout;
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GIB: u64 = 1 << 30;
const DEFAULT_CONTEXT: u64 = 32768;
const RELIABILITY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_REPORT_BYTES: u64 = 64 * 1024;
const KV_BITS: [u8; 5] = [2, 3, 4, 6, 8];

/// Repositories suggested for download when they fit the machine. Not measured by Dial.
#[derive(Debug, Serialize)]
pub struct Suggestion {
    pub model: &'static str,
    pub min_memory_gib: u64,
    pub note: &'static str,
}

const SUGGESTIONS: &[Suggestion] = &[
    Suggestion {
        model: "mlx-community/Qwen2.5-0.5B-Instruct-4bit",
        min_memory_gib: 4,
        note: "small model for smoke tests",
    },
    Suggestion {
        model: "mlx-community/Qwen2.5-3B-Instruct-4bit",
        min_memory_gib: 8,
        note: "compact model used for project summaries",
    },
    Suggestion {
        model: "mlx-community/Qwen2.5-7B-Instruct-4bit",
        min_memory_gib: 16,
        note: "general coding model in the README examples",
    },
    Suggestion {
        model: "mlx-community/Qwen3-30B-A3B-4bit",
        min_memory_gib: 32,
        note: "mixture of experts with about 3B active parameters",
    },
    Suggestion {
        model: "mlx-community/Qwen3-32B-4bit",
        min_memory_gib: 48,
        note: "dense 32B model",
    },
];

/// Parsed `dial models recommend` flags.
#[derive(Debug, Clone, Default)]
pub struct RecommendOptions {
    pub context: Option<u64>,
    pub kv_bits: Option<u8>,
    pub memory_gb: Option<f64>,
    pub json: bool,
}

/// Recorded result of `dial reliability check` for one model.
#[derive(Debug, Clone, Serialize)]
pub struct Recorded {
    pub measured_at: u64,
    pub age_days: u64,
    pub stale: bool,
    pub passed: u64,
    pub total: u64,
}

#[derive(Debug)]
struct Candidate {
    label: String,
    source: String,
    path: PathBuf,
    favorite: bool,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub label: String,
    pub source: String,
    pub path: String,
    pub favorite: bool,
    pub quantization_bits: Option<u64>,
    pub weights_bytes: Option<u64>,
    pub estimate: Option<Estimate>,
    pub max_context_tokens: u64,
    pub fit: Fit,
    pub note: Option<String>,
    pub recorded: Option<Recorded>,
}

#[derive(Debug, Serialize)]
struct Report {
    memory_bytes: Option<u64>,
    memory_source: &'static str,
    context_tokens: u64,
    kv_bits: Option<u8>,
    comfortable_fraction: f64,
    limit_fraction: f64,
    models: Vec<Row>,
    suggestions: Vec<&'static Suggestion>,
}

/// Runs `dial models recommend`. `overlay` is the CLI config overlay (e.g. from `--model`).
pub fn run(cwd: &Path, options: &RecommendOptions, overlay: Option<toml::Table>) -> Result<i32> {
    validate(options)?;
    let layout = Layout::discover(cwd)?;
    let config = Config::load(&layout, overlay)?;
    let requested = options.context.unwrap_or_else(|| {
        config
            .requested_context_window()
            .map_or(DEFAULT_CONTEXT, u64::from)
    });
    let (memory_bytes, memory_source) = match options.memory_gb {
        Some(gb) => (Some((gb * GIB as f64) as u64), "override"),
        None => match local_fit::physical_memory_bytes() {
            Some(bytes) => (Some(bytes), "detected"),
            None => (None, "unknown"),
        },
    };
    let reports = recorded_reports(&layout.state_dir);
    let mut models: Vec<Row> = candidates(&layout, &config)
        .into_iter()
        .map(|candidate| {
            row(
                candidate,
                requested,
                options.kv_bits,
                memory_bytes,
                &reports,
            )
        })
        .collect();
    models.sort_by(|a, b| {
        a.fit
            .cmp(&b.fit)
            .then_with(|| pass_ratio(b).total_cmp(&pass_ratio(a)))
            .then_with(|| b.weights_bytes.cmp(&a.weights_bytes))
            .then_with(|| a.label.cmp(&b.label))
    });
    let suggestions = SUGGESTIONS
        .iter()
        .filter(|suggestion| !models.iter().any(|row| row.label == suggestion.model))
        .filter(|suggestion| {
            memory_bytes.is_none_or(|bytes| suggestion.min_memory_gib * GIB <= bytes)
        })
        .collect();
    let report = Report {
        memory_bytes,
        memory_source,
        context_tokens: requested,
        kv_bits: options.kv_bits,
        comfortable_fraction: local_fit::COMFORTABLE_FRACTION,
        limit_fraction: local_fit::LIMIT_FRACTION,
        models,
        suggestions,
    };
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", format_report(&report));
    }
    Ok(0)
}

fn validate(options: &RecommendOptions) -> Result<()> {
    if let Some(context) = options.context {
        ensure!(context > 0, "--context must be a positive token count");
    }
    if let Some(bits) = options.kv_bits {
        ensure!(
            KV_BITS.contains(&bits),
            "--kv-bits must be one of 2, 3, 4, 6, 8"
        );
    }
    if let Some(gb) = options.memory_gb {
        ensure!(
            gb.is_finite() && gb > 0.0,
            "--memory-gb must be a positive number"
        );
    }
    Ok(())
}

/// Downloaded MLX checkpoints plus favorite MLX paths that resolve to local snapshots.
fn candidates(layout: &Layout, config: &Config) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = catalog::downloaded_mlx_models(&layout.user_dir)
        .into_iter()
        .map(|entry| Candidate {
            label: entry.label,
            source: entry.source,
            path: PathBuf::from(entry.selection.model),
            favorite: false,
        })
        .collect();
    for favorite in config
        .model_favorites
        .iter()
        .filter(|favorite| favorite.provider == "mlx" && favorite.base_url.is_none())
    {
        let Ok(resolved) = catalog::resolve_local_model(&favorite.model) else {
            continue;
        };
        let path = PathBuf::from(resolved);
        match out.iter_mut().find(|known| same_path(&known.path, &path)) {
            Some(known) => known.favorite = true,
            None => out.push(Candidate {
                label: favorite.model.clone(),
                source: "favorite".into(),
                path,
                favorite: true,
            }),
        }
    }
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf())
        == fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf())
}

fn row(
    candidate: Candidate,
    requested: u64,
    kv_bits: Option<u8>,
    memory_bytes: Option<u64>,
    reports: &[(String, Recorded)],
) -> Row {
    let config = fs::read_to_string(candidate.path.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let architecture = config.as_ref().and_then(Architecture::from_config);
    let weights_bytes = local_fit::safetensors_bytes(&candidate.path);
    let (estimate, max_context_tokens, note) = match (architecture, weights_bytes) {
        (Some(arch), Some(weights)) => (
            Some(local_fit::estimate(
                weights,
                &arch,
                requested,
                kv_bits,
                memory_bytes,
            )),
            local_fit::max_context(weights, &arch, kv_bits, memory_bytes),
            None,
        ),
        (None, _) => (
            None,
            0,
            Some("config.json has no transformer layout".to_owned()),
        ),
        (Some(_), None) => (None, 0, Some("no safetensors weights found".to_owned())),
    };
    let path = candidate.path.to_string_lossy().into_owned();
    let recorded = reports
        .iter()
        .filter(|(model, _)| *model == candidate.label || *model == path)
        .map(|(_, report)| report.clone())
        .max_by_key(|report| report.measured_at);
    Row {
        label: candidate.label,
        source: candidate.source,
        path,
        favorite: candidate.favorite,
        quantization_bits: config.as_ref().and_then(local_fit::quantization_bits),
        weights_bytes,
        fit: estimate.map_or(Fit::Unknown, |estimate| estimate.fit),
        estimate,
        max_context_tokens,
        note,
        recorded,
    }
}

/// Newest recorded report per `model` value found under `<state_dir>/models/*/reliability.json`.
fn recorded_reports(state_dir: &Path) -> Vec<(String, Recorded)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let Ok(dirs) = fs::read_dir(state_dir.join("models")) else {
        return Vec::new();
    };
    dirs.filter_map(Result::ok)
        .filter_map(|dir| {
            let path = dir.path().join("reliability.json");
            let metadata = fs::metadata(&path).ok()?;
            if metadata.len() > MAX_REPORT_BYTES {
                return None;
            }
            let value: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
            let model = value.get("model")?.as_str()?.to_owned();
            let measured_at = value.get("measured_at")?.as_u64()?;
            let passed = value.get("passed")?.as_u64()?;
            let total = value.get("total")?.as_u64()?;
            let age = Duration::from_secs(now.saturating_sub(measured_at));
            Some((
                model,
                Recorded {
                    measured_at,
                    age_days: age.as_secs() / 86_400,
                    stale: age > RELIABILITY_MAX_AGE,
                    passed,
                    total,
                },
            ))
        })
        .collect()
}

fn pass_ratio(row: &Row) -> f64 {
    row.recorded.as_ref().map_or(-1.0, |recorded| {
        recorded.passed as f64 / recorded.total.max(1) as f64
    })
}

fn fit_label(fit: Fit) -> &'static str {
    match fit {
        Fit::Fits => "fits",
        Fit::Tight => "tight",
        Fit::WontFit => "wont_fit",
        Fit::Unknown => "unknown",
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

fn reliability_cell(recorded: Option<&Recorded>) -> String {
    match recorded {
        None => "not measured".to_owned(),
        Some(recorded) => format!(
            "{}/{} passed, {} days old{}",
            recorded.passed,
            recorded.total,
            recorded.age_days,
            if recorded.stale { " (stale)" } else { "" }
        ),
    }
}

fn format_report(report: &Report) -> String {
    let mut out = String::new();
    let memory = match report.memory_bytes {
        Some(bytes) => format!("{} ({})", gib(bytes), report.memory_source),
        None => "unknown (pass --memory-gb)".to_owned(),
    };
    let kv = report
        .kv_bits
        .map_or("fp16".to_owned(), |bits| format!("{bits}-bit"));
    let _ = writeln!(out, "Memory: {memory}");
    let _ = writeln!(
        out,
        "Context: {} tokens requested; KV cache {kv}",
        report.context_tokens
    );
    let _ = writeln!(
        out,
        "Budget: fits up to {:.0}% of RAM, tight up to {:.0}%",
        report.comfortable_fraction * 100.0,
        report.limit_fraction * 100.0
    );
    out.push('\n');
    if report.models.is_empty() {
        out.push_str(
            "no downloaded local MLX models found; download one with `dial setup -m <repo>`\n",
        );
    } else {
        let header = [
            "FIT",
            "MODEL",
            "WEIGHTS",
            "KV",
            "RESIDENT",
            "FITS_CTX",
            "RELIABILITY",
        ];
        let cells: Vec<[String; 7]> = report
            .models
            .iter()
            .map(|row| {
                let estimate = row.estimate.as_ref();
                let max_ctx = match (report.memory_bytes, estimate) {
                    (Some(_), Some(_)) if row.max_context_tokens == 0 => "none".to_owned(),
                    (Some(_), Some(_)) => row.max_context_tokens.to_string(),
                    _ => "-".to_owned(),
                };
                [
                    fit_label(row.fit).to_owned(),
                    if row.favorite {
                        format!("{} [favorite]", row.label)
                    } else {
                        row.label.clone()
                    },
                    row.weights_bytes.map_or("-".to_owned(), gib),
                    estimate.map_or("-".to_owned(), |e| gib(e.kv_bytes)),
                    estimate.map_or("-".to_owned(), |e| gib(e.resident_bytes)),
                    max_ctx,
                    reliability_cell(row.recorded.as_ref()),
                ]
            })
            .collect();
        let mut widths = header.map(str::len);
        for cell_row in &cells {
            for (width, cell) in widths.iter_mut().zip(cell_row.iter()) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let line = |cells: &[String]| {
            let mut text = String::new();
            for (index, cell) in cells.iter().enumerate() {
                let _ = write!(text, "{cell:<width$}  ", width = widths[index]);
            }
            text.trim_end().to_owned() + "\n"
        };
        let header_cells: Vec<String> = header.iter().map(|cell| (*cell).to_owned()).collect();
        out.push_str(&line(&header_cells));
        for cell_row in &cells {
            out.push_str(&line(cell_row));
        }
        for row in &report.models {
            if let Some(note) = &row.note {
                let _ = writeln!(out, "  {}: {note}", row.label);
            }
        }
    }
    if !report.suggestions.is_empty() {
        out.push_str("\nSuggested downloads (not measured on this machine):\n");
        for suggestion in &report.suggestions {
            let _ = writeln!(
                out,
                "  {}  (from {} GiB RAM; {})",
                suggestion.model, suggestion.min_memory_gib, suggestion.note
            );
        }
    }
    out.push_str(
        "\nEstimates add a 1.5 GiB runtime allowance and 5% of weights to weights plus KV cache.\n\
         FITS_CTX is the longest context (capped by the model's limit) that stays within the fits budget.\n\
         RELIABILITY shows recorded `dial reliability check` results only, not coding accuracy.\n",
    );
    out
}
