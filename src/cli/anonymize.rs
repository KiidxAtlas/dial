//! `dial anonymize`: writes an anonymized copy of a session to a directory.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::storage::{
    anonymize::{Anonymizer, Identity, REVIEW_NOTE, safe_session_name},
    dump::{is_within, resolve_allowing_missing},
    paths::Layout,
    session::Session,
};

#[derive(clap::Args, Debug, Clone)]
pub struct AnonymizeArgs {
    /// Session file path, or a session id (prefix). Defaults to the most recent session.
    pub session: Option<String>,
    /// Output directory (must be new or empty). Defaults to `<cwd>/<session-stem>.anon`.
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
}

/// Runs the anonymize command. Returns the process exit code: 0 on success,
/// 2 for user errors (bad session reference, unsafe output directory).
pub fn run(layout: &Layout, args: AnonymizeArgs) -> Result<i32> {
    let session_path = match resolve_session(layout, args.session.as_deref())? {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let out_dir = match args.out {
        Some(path) => path,
        None => {
            let stem = session_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session");
            std::env::current_dir()?.join(format!("{stem}.anon"))
        }
    };

    if out_dir.is_file() {
        eprintln!("output path is a file: {}", out_dir.display());
        return Ok(2);
    }
    let session_dir = session_path.parent().unwrap_or_else(|| Path::new("."));
    if is_within(&out_dir, session_dir)? || is_within(&out_dir, &layout.sessions_dir())? {
        eprintln!(
            "refusing to write anonymized output inside the session directory: {}",
            out_dir.display()
        );
        return Ok(2);
    }
    if is_within(&session_path, &out_dir)? {
        eprintln!(
            "refusing to write output containing the source session: {}",
            out_dir.display()
        );
        return Ok(2);
    }
    if out_dir.exists() && !is_empty_dir(&out_dir)? {
        eprintln!("output directory is not empty: {}", out_dir.display());
        return Ok(2);
    }

    let raw = fs::read(&session_path)
        .with_context(|| format!("failed to read {}", session_path.display()))?;
    let mut anonymizer = Anonymizer::new(Identity::detect());
    let anon = anonymizer
        .session_jsonl(&raw)
        .with_context(|| format!("{} is not a valid session file", session_path.display()))?;
    let session = Session::read(&session_path)?;
    let request = session.context_request();

    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let session_out = out_dir.join(safe_session_name(&session_path));
    fs::write(&session_out, &anon.bytes)
        .with_context(|| format!("failed to write {}", session_out.display()))?;
    if let Some(value) = request {
        let anon_request = anonymizer.value(&value);
        let request_out = out_dir.join("llm-request.json");
        fs::write(&request_out, serde_json::to_vec_pretty(&anon_request)?)
            .with_context(|| format!("failed to write {}", request_out.display()))?;
    }

    let records = anon.bytes.iter().filter(|b| **b == b'\n').count();
    println!("Anonymized {records} record(s) → {}", out_dir.display());
    if anon.malformed > 0 {
        println!(
            "Skipped {} malformed record(s) in {}",
            anon.malformed,
            session_path.display()
        );
    }
    if anon.redacted > 0 {
        println!("Redacted {} secret(s)", anon.redacted);
    }
    println!("{REVIEW_NOTE}");
    Ok(0)
}

/// Resolves the session argument. Outer `Result` is I/O failure; inner `Err`
/// is a user-facing message (exit code 2).
fn resolve_session(layout: &Layout, arg: Option<&str>) -> Result<Result<PathBuf, String>> {
    let Some(arg) = arg else {
        let latest = Session::list_newest(layout, 1)?;
        return Ok(match latest.into_iter().next() {
            Some(info) => Ok(info.path),
            None => Err("no sessions found".to_string()),
        });
    };
    let as_path = PathBuf::from(arg);
    if as_path.is_file() {
        return Ok(Ok(resolve_allowing_missing(&as_path)?));
    }
    let matches: Vec<_> = Session::list(layout, usize::MAX)?
        .into_iter()
        .filter(|info| info.id.starts_with(arg))
        .collect();
    Ok(match matches.len() {
        0 => Err(format!("no session matching `{arg}`")),
        1 => Ok(matches[0].path.clone()),
        n => Err(format!("`{arg}` matches {n} sessions; use a longer prefix")),
    })
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    Ok(fs::read_dir(path)?.next().is_none())
}

#[cfg(test)]
mod tests;
