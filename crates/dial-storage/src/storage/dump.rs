//! Session dump archives: a zip containing the session JSONL, the last LLM request when
//! present, and every subagent transcript the session spawned under `subagents/`.
//! `All` is verbatim; `Anon` is anonymized (worker transcripts through the same anonymizer, so
//! identities stay consistent across the parent and its workers). The archive layout is
//! Dial-native.

use std::{
    fmt,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::config::paths::Layout;
use crate::storage::anonymize::{Anonymizer, Identity, REVIEW_NOTE};
use crate::storage::session::Session;
use crate::storage::subagent;

/// Whether the archive holds raw session data or an anonymized copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpKind {
    All,
    Anon,
}

impl DumpKind {
    pub fn label(self) -> &'static str {
        match self {
            DumpKind::All => "all",
            DumpKind::Anon => "anon",
        }
    }
}

/// Summary of a written archive.
#[derive(Debug, Clone)]
pub struct DumpReport {
    pub archive: PathBuf,
    pub kind: DumpKind,
    pub entries: Vec<String>,
    /// Subagent transcripts included under `subagents/`.
    pub subagents: usize,
    /// Malformed lines dropped during anonymization.
    pub malformed: usize,
    /// Credential matches replaced during anonymization.
    pub redacted: usize,
}

impl fmt::Display for DumpReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Session dump archive: {}", self.archive.display())?;
        writeln!(
            f,
            "Contains {} file(s) ({} subagent transcript(s)):",
            self.entries.len(),
            self.subagents
        )?;
        for entry in &self.entries {
            writeln!(f, "  {entry}")?;
        }
        if self.malformed > 0 {
            writeln!(f, "Skipped {} malformed record(s).", self.malformed)?;
        }
        if self.redacted > 0 {
            writeln!(f, "Redacted {} secret(s).", self.redacted)?;
        }
        match self.kind {
            DumpKind::Anon => write!(f, "{REVIEW_NOTE}"),
            DumpKind::All => write!(
                f,
                "This archive persists on disk and may contain raw context/secrets — treat accordingly."
            ),
        }
    }
}

/// Resolves `path` to an absolute path, canonicalizing the longest existing
/// ancestor so that not-yet-created components still compare correctly.
pub fn resolve_allowing_missing(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.clone();
    let mut tail = Vec::new();
    loop {
        if let Ok(canon) = existing.canonicalize() {
            let mut out = canon;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent().map(Path::to_path_buf),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return Ok(absolute),
        }
    }
}

/// True when `child` is `parent` or lies beneath it.
pub fn is_within(child: &Path, parent: &Path) -> Result<bool> {
    Ok(resolve_allowing_missing(child)?.starts_with(resolve_allowing_missing(parent)?))
}

/// Archive member name for one subagent transcript file.
fn worker_member(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("subagent transcript has no UTF-8 name: {}", path.display()))?;
    Ok(format!("subagents/{name}"))
}

/// Writes a dump archive of `session_path` into `out_dir`.
///
/// Refuses an `out_dir` inside the session's own directory or the sessions
/// directory, and never overwrites an existing archive.
pub fn dump_session(
    layout: &Layout,
    session_path: &Path,
    kind: DumpKind,
    out_dir: &Path,
) -> Result<DumpReport> {
    if !session_path.is_file() {
        bail!("session file not found: {}", session_path.display());
    }
    let session_dir = session_path.parent().unwrap_or_else(|| Path::new("."));
    if is_within(out_dir, session_dir)? || is_within(out_dir, &layout.sessions_dir())? {
        bail!(
            "refusing to write dump inside the session directory: {}",
            out_dir.display()
        );
    }

    let session = Session::read(session_path)
        .with_context(|| format!("failed to read session {}", session_path.display()))?;
    let session_id = session.id().to_string();
    let request = session.context_request();
    let transcripts = subagent::transcript_files(layout, &session_id)?;

    let (session_bytes, request_bytes, mut workers, malformed, redacted) = match kind {
        DumpKind::All => {
            let raw = fs::read(session_path)
                .with_context(|| format!("failed to read {}", session_path.display()))?;
            let request_bytes = match &request {
                Some(value) => Some(serde_json::to_vec_pretty(value)?),
                None => None,
            };
            let mut workers = Vec::with_capacity(transcripts.len());
            for path in &transcripts {
                let bytes =
                    fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
                workers.push((worker_member(path)?, bytes));
            }
            (raw, request_bytes, workers, 0, 0)
        }
        DumpKind::Anon => {
            let raw = fs::read(session_path)
                .with_context(|| format!("failed to read {}", session_path.display()))?;
            let mut anonymizer = Anonymizer::new(Identity::detect());
            let anon = anonymizer.session_jsonl(&raw)?;
            let request_bytes = match &request {
                Some(value) => Some(serde_json::to_vec_pretty(&anonymizer.value(value))?),
                None => None,
            };
            let (mut malformed, mut redacted) = (anon.malformed, anon.redacted);
            let mut workers = Vec::with_capacity(transcripts.len());
            for path in &transcripts {
                let raw =
                    fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
                let worker = anonymizer.transcript_jsonl(&raw)?;
                malformed += worker.malformed;
                redacted += worker.redacted;
                workers.push((worker_member(path)?, worker.bytes));
            }
            (anon.bytes, request_bytes, workers, malformed, redacted)
        }
    };

    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let archive = out_dir.join(format!(
        "dial-dump-{session_id}-{}-{millis}.zip",
        kind.label()
    ));

    let mut members: Vec<(String, Vec<u8>)> = vec![("session.jsonl".to_string(), session_bytes)];
    if let Some(bytes) = request_bytes {
        members.push(("llm-request.json".to_string(), bytes));
    }
    let subagents = workers.len();
    members.append(&mut workers);

    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&archive)
        .with_context(|| format!("failed to create {}", archive.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut entries = Vec::with_capacity(members.len());
    for (name, bytes) in &members {
        zip.start_file(name.as_str(), zip::write::SimpleFileOptions::default())?;
        zip.write_all(bytes)?;
        entries.push(name.clone());
    }
    zip.finish()?;

    Ok(DumpReport {
        archive,
        kind,
        entries,
        subagents,
        malformed,
        redacted,
    })
}

#[cfg(test)]
mod tests;
