//! `dial ps`: list and control the compute processes Dial manages (model servers, services,
//! tool kernels). Backed by `learn::processes`. Every action re-reads the inventory and binds
//! to the selected process's identity before signalling or reading its output.

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde_json::Value;

use crate::learn::processes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PsAction {
    List,
    Info,
    Logs,
    Stop,
    Kill,
    Restart,
}

/// Arguments for `dial ps [action] [pid]`.
#[derive(Debug, Clone, clap::Args)]
pub struct PsArgs {
    /// list (default), info, stop, kill, restart, or logs
    pub action: Option<PsAction>,
    /// Process id (required for every action except list)
    pub target: Option<String>,
    /// Emit machine-readable JSON
    #[arg(short = 'j', long)]
    pub json: bool,
}

/// Runs a `ps` action against the compute-process inventory under `user_dir`. Returns the exit code.
pub fn run(user_dir: &Path, args: PsArgs) -> Result<i32> {
    match args.action.unwrap_or(PsAction::List) {
        PsAction::List => {
            if args.target.is_some() {
                bail!("ps list takes no process id");
            }
            let rows = processes::snapshot(user_dir)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("No compute processes.");
            } else {
                print!("{}", format_table(&rows));
            }
            Ok(0)
        }
        PsAction::Info => {
            let pid = parse_pid(args.target.as_deref(), "info")?;
            let rows = processes::snapshot(user_dir)?;
            let row = find_row(&rows, pid)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(row)?);
            } else {
                print!("{}", format_detail(row));
            }
            Ok(0)
        }
        PsAction::Stop => {
            let pid = parse_pid(args.target.as_deref(), "stop")?;
            // Identity is re-read from a fresh snapshot so `stop` only signals the process we just listed.
            let rows = processes::snapshot(user_dir)?;
            let row = find_row(&rows, pid)?;
            let identity = row["identity"]
                .as_str()
                .context("compute process row has no identity")?;
            println!("{}", processes::stop(user_dir, pid, identity)?);
            Ok(0)
        }
        PsAction::Kill => {
            let pid = parse_pid(args.target.as_deref(), "kill")?;
            let rows = processes::snapshot(user_dir)?;
            let row = find_row(&rows, pid)?;
            let identity = row["identity"]
                .as_str()
                .context("compute process row has no identity")?;
            println!("{}", processes::kill(user_dir, pid, identity)?);
            Ok(0)
        }
        PsAction::Restart => {
            let pid = parse_pid(args.target.as_deref(), "restart")?;
            let rows = processes::snapshot(user_dir)?;
            let row = find_row(&rows, pid)?;
            let identity = row["identity"]
                .as_str()
                .context("compute process row has no identity")?;
            println!("{}", processes::restart(user_dir, pid, identity)?);
            Ok(0)
        }
        PsAction::Logs => {
            let pid = parse_pid(args.target.as_deref(), "logs")?;
            let rows = processes::snapshot(user_dir)?;
            let row = find_row(&rows, pid)?;
            let identity = row["identity"]
                .as_str()
                .context("compute process row has no identity")?;
            print!("{}", processes::logs(user_dir, pid, identity)?);
            Ok(0)
        }
    }
}

/// Parses a pid argument. Pids 0 and 1 are never valid targets.
fn parse_pid(target: Option<&str>, action: &str) -> Result<i32> {
    let raw = target.with_context(|| format!("ps {action} requires a process id"))?;
    let pid: i32 = raw
        .parse()
        .with_context(|| format!("process id must be a number, got {raw:?}"))?;
    if pid <= 1 {
        bail!("process id must be greater than 1, got {pid}");
    }
    Ok(pid)
}

fn find_row(rows: &[Value], pid: i32) -> Result<&Value> {
    rows.iter()
        .find(|row| row["pid"].as_i64() == Some(i64::from(pid)))
        .with_context(|| format!("no compute process with pid {pid}"))
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn or_dash(value: &str) -> String {
    if value.is_empty() {
        "-".to_owned()
    } else {
        value.to_owned()
    }
}

fn format_bytes(bytes: Option<u64>) -> String {
    const KIB: f64 = 1024.0;
    let Some(bytes) = bytes else {
        return "-".to_owned();
    };
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// The most descriptive name for a row: the model when known, otherwise the project.
fn label(row: &Value) -> String {
    let model = text(&row["model"]);
    if model.is_empty() {
        or_dash(text(&row["project"]))
    } else {
        model.to_owned()
    }
}

/// Aligned plain-text table with one row per process. The last column is left unpadded.
fn format_table(rows: &[Value]) -> String {
    let header: Vec<String> = ["PID", "KIND", "PHASE", "MEMORY", "CPU", "LABEL"]
        .map(str::to_owned)
        .into();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row["pid"]
                    .as_i64()
                    .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
                or_dash(text(&row["kind"])),
                or_dash(text(&row["phase"])),
                format_bytes(row["memory_bytes"].as_u64()),
                row["cpu_percent"]
                    .as_f64()
                    .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.1}")),
                label(row),
            ]
        })
        .collect();

    let mut widths: Vec<usize> = header.iter().map(String::len).collect();
    for cells in &body {
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for cells in std::iter::once(&header).chain(body.iter()) {
        push_line(&mut out, cells, &widths);
    }
    out
}

fn push_line(out: &mut String, cells: &[String], widths: &[usize]) {
    let last = cells.len() - 1;
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index == last {
            out.push_str(cell);
        } else {
            out.push_str(&format!("{cell:<width$}  ", width = width));
        }
    }
    out.push('\n');
}

/// Key/value detail block for `ps info`.
fn format_detail(row: &Value) -> String {
    let fields = [
        ("pid", row["pid"].to_string()),
        ("parent", row["parent_pid"].to_string()),
        ("kind", or_dash(text(&row["kind"]))),
        ("model", or_dash(text(&row["model"]))),
        ("project", or_dash(text(&row["project"]))),
        ("phase", or_dash(text(&row["phase"]))),
        ("state", or_dash(text(&row["state"]))),
        ("memory", format_bytes(row["memory_bytes"].as_u64())),
        (
            "cpu",
            row["cpu_percent"]
                .as_f64()
                .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.1}%")),
        ),
        ("orphaned", row["orphaned"].to_string()),
    ];
    let mut out = String::new();
    for (key, value) in fields {
        out.push_str(&format!("{key:<10} {value}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_valid_pid_and_rejects_others() {
        assert_eq!(parse_pid(Some("42"), "info").unwrap(), 42);
        assert!(parse_pid(Some("1"), "info").is_err());
        assert!(parse_pid(Some("0"), "info").is_err());
        assert!(parse_pid(Some("web"), "info").is_err());
        assert!(parse_pid(None, "stop").is_err());
    }

    #[test]
    fn finds_row_by_pid() {
        let rows = vec![json!({"pid": 7, "kind": "models"}), json!({"pid": 9})];
        assert_eq!(find_row(&rows, 9).unwrap()["pid"], 9);
        assert!(find_row(&rows, 3).is_err());
    }

    #[test]
    fn formats_bytes_with_units() {
        assert_eq!(format_bytes(None), "-");
        assert_eq!(format_bytes(Some(512)), "512 B");
        assert_eq!(format_bytes(Some(1536)), "1.5 KiB");
        assert_eq!(format_bytes(Some(3 * 1024 * 1024)), "3.0 MiB");
    }

    #[test]
    fn table_has_header_and_aligned_rows() {
        let rows = vec![json!({
            "pid": 42,
            "kind": "models",
            "phase": "running",
            "memory_bytes": 1048576,
            "cpu_percent": 1.5,
            "model": "mlx-model",
        })];
        let table = format_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("PID"));
        assert!(lines[1].contains("42"));
        assert!(lines[1].contains("1.0 MiB"));
        assert!(lines[1].ends_with("mlx-model"));
    }

    #[test]
    fn label_prefers_model_then_project() {
        assert_eq!(label(&json!({"model": "m", "project": "p"})), "m");
        assert_eq!(label(&json!({"model": "", "project": "p"})), "p");
        assert_eq!(label(&json!({})), "-");
    }
}
