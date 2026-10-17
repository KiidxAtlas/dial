//! `dial images` (alias `img`): inspect and maintain the image reference store
//! (`storage::image_refs`), the content-addressed images that sessions and imports attached.
//!
//! `status` and `purge --apply` first refresh references from session transcripts, so images that
//! a live transcript still holds keep their serving window. A dry-run purge only reads the store.

use crate::storage::{
    image_refs::{self, IMAGE_TTL_HOURS},
    paths::Layout,
};
use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ImagesAction {
    Status,
    Doctor,
    Probe,
    Purge,
}

/// Arguments for `dial images [action]`.
#[derive(Debug, Clone, clap::Args)]
pub struct ImagesArgs {
    /// status (default), doctor, probe, or purge
    pub action: Option<ImagesAction>,
    /// Emit one JSON document
    #[arg(long)]
    pub json: bool,
    /// Apply purge deletions (default is dry-run)
    #[arg(long)]
    pub apply: bool,
    /// Purge all entries instead of expired entries only
    #[arg(long)]
    pub all: bool,
    /// Project directory (default: current directory)
    #[arg(long)]
    pub dir: Option<String>,
    /// Probe timeout in seconds (positive integer; default 10)
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: Option<u64>,
}

/// One action's outcome: exit code, JSON document, and human text.
struct Output {
    code: i32,
    json: Value,
    text: String,
}

/// Runs an `images` action against `layout`, or against the project at `--dir` when given.
/// Returns the process exit code: 0 on success, 1 when doctor finds problems, a probe fails, or a
/// purge hits deletion errors.
pub fn run(layout: &Layout, args: ImagesArgs) -> Result<i32> {
    let owned;
    let layout = match args.dir.as_deref() {
        Some(dir) => {
            owned = Layout::discover(Path::new(dir))?;
            &owned
        }
        None => layout,
    };
    let out = match args.action.unwrap_or(ImagesAction::Status) {
        ImagesAction::Status => status(layout)?,
        ImagesAction::Doctor => doctor(layout)?,
        ImagesAction::Probe => probe(layout, args.timeout)?,
        ImagesAction::Purge => purge(layout, args.all, args.apply)?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&out.json)?);
    } else {
        print!("{}", out.text);
    }
    Ok(out.code)
}

fn status(layout: &Layout) -> Result<Output> {
    let user_dir = &layout.user_dir;
    let found = image_refs::scan_all_sessions(user_dir)?;
    let refs = image_refs::list(user_dir)?;
    let now = now_secs();
    let bytes: u64 = refs.iter().map(|r| r.bytes).sum();
    let sessions: BTreeSet<&str> = refs
        .iter()
        .flat_map(|r| r.sessions.iter().map(String::as_str))
        .collect();
    let expired = refs.iter().filter(|r| r.expired_at(now)).count();
    let store = image_refs::store_dir(user_dir);
    let json = json!({
        "action": "status",
        "store": store.display().to_string(),
        "ttlHours": IMAGE_TTL_HOURS,
        "count": refs.len(),
        "bytes": bytes,
        "sessions": sessions.len(),
        "expired": expired,
        "transcriptImages": found,
    });
    let text = lines(&[
        format!("Image store: {}", store.display()),
        format!(
            "References: {} ({}), {expired} expired (window {IMAGE_TTL_HOURS}h)",
            refs.len(),
            format_bytes(bytes)
        ),
        format!("Sessions covered: {}", sessions.len()),
        format!("Images found in transcripts this run: {found}"),
    ]);
    Ok(Output {
        code: 0,
        json,
        text,
    })
}

fn doctor(layout: &Layout) -> Result<Output> {
    let report = image_refs::verify(&layout.user_dir)?;
    let healthy = report.problems.is_empty();
    let store = image_refs::store_dir(&layout.user_dir);
    let mut out = vec![
        format!("Image store: {}", store.display()),
        format!("Checked {} reference(s)", report.entries),
    ];
    out.extend(report.problems.iter().map(|p| format!("error: {p}")));
    if !report.orphans.is_empty() {
        out.push(format!(
            "warn: {} unreferenced blob(s): {}",
            report.orphans.len(),
            report.orphans.join(", ")
        ));
    }
    out.push(if healthy {
        "healthy".to_owned()
    } else {
        format!("unhealthy: {} problem(s)", report.problems.len())
    });
    let json = json!({
        "action": "doctor",
        "healthy": healthy,
        "entries": report.entries,
        "problems": report.problems,
        "orphans": report.orphans,
    });
    Ok(Output {
        code: if healthy { 0 } else { 1 },
        json,
        text: lines(&out),
    })
}

/// Checks that every referenced blob can be read, within the timeout. Read-only.
fn probe(layout: &Layout, timeout_secs: Option<u64>) -> Result<Output> {
    let limit = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_PROBE_TIMEOUT_SECS));
    let shas: Vec<String> = image_refs::list(&layout.user_dir)?
        .into_iter()
        .map(|r| r.sha256)
        .collect();
    let user_dir = layout.user_dir.clone();
    let started = Instant::now();
    let outcome = bounded(limit, move || read_blobs(&user_dir, &shas));
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (ok, blobs, detail) = match outcome {
        Ok(Ok((count, bytes))) => (
            true,
            count,
            format!("read {count} blob(s), {}", format_bytes(bytes)),
        ),
        Ok(Err(e)) => (false, 0, format!("{e:#}")),
        Err(e) => (false, 0, e),
    };
    let json = json!({
        "action": "probe",
        "ok": ok,
        "blobs": blobs,
        "durationMs": elapsed_ms,
        "timeoutMs": limit.as_millis() as u64,
        "detail": detail,
    });
    Ok(Output {
        code: if ok { 0 } else { 1 },
        json,
        text: format!(
            "probe {}: {detail} in {elapsed_ms}ms\n",
            if ok { "ok" } else { "failed" }
        ),
    })
}

fn purge(layout: &Layout, all: bool, apply: bool) -> Result<Output> {
    let user_dir = &layout.user_dir;
    let found = if apply {
        image_refs::scan_all_sessions(user_dir)?
    } else {
        0
    };
    let report = image_refs::purge(user_dir, all, apply)?;
    let scope = if all {
        "reference(s)"
    } else {
        "expired reference(s)"
    };
    let bytes = report.bytes();
    let mut out = vec![if apply {
        format!(
            "Purged {} of {} {scope} ({})",
            report.deleted,
            report.selected.len(),
            format_bytes(bytes)
        )
    } else {
        format!(
            "Dry run: {} {scope} would be purged ({}); re-run with --apply",
            report.selected.len(),
            format_bytes(bytes)
        )
    }];
    out.extend(report.errors.iter().map(|e| format!("error: {e}")));
    let json = json!({
        "action": "purge",
        "applied": apply,
        "all": all,
        "selected": report.selected.len(),
        "bytes": bytes,
        "deleted": report.deleted,
        "transcriptImages": found,
        "errors": report.errors,
    });
    Ok(Output {
        code: if report.errors.is_empty() { 0 } else { 1 },
        json,
        text: lines(&out),
    })
}

/// Reads every referenced blob, returning `(count, total bytes)`.
fn read_blobs(user_dir: &Path, shas: &[String]) -> Result<(usize, u64)> {
    let mut bytes = 0u64;
    for sha in shas {
        let path = image_refs::resolve(user_dir, sha)?;
        bytes += fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .len() as u64;
    }
    Ok((shas.len(), bytes))
}

/// Runs `work` on a worker thread and waits at most `limit` for its result. A worker that
/// overruns is left running; the process exits without waiting for it.
fn bounded<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> std::result::Result<T, String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(limit).map_err(|e| match e {
        mpsc::RecvTimeoutError::Timeout => format!("timed out after {}ms", limit.as_millis()),
        mpsc::RecvTimeoutError::Disconnected => "probe worker stopped unexpectedly".to_owned(),
    })
}

fn lines(items: &[String]) -> String {
    items.iter().map(|line| format!("{line}\n")).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let value = n as f64;
    if value < KIB {
        format!("{n} B")
    } else if value < MIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{:.1} MiB", value / MIB)
    }
}

#[cfg(test)]
mod tests;
