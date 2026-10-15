//! Session dump archives: a zip containing the session JSONL and, when present,
//! the last LLM request. `All` is verbatim; `Anon` is anonymized. The archive layout is
//! Dial-native.
//!
//! Subagent transcripts are never persisted in Dial (child sessions live in a
//! dropped temp dir), so `subagents` is always 0.

use std::{
    fmt,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::storage::{
    anonymize::{Anonymizer, Identity, REVIEW_NOTE},
    paths::Layout,
    session::Session,
};

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
    /// Subagent transcript count. Always 0 in Dial.
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

    let (session_bytes, request_bytes, malformed, redacted) = match kind {
        DumpKind::All => {
            let raw = fs::read(session_path)
                .with_context(|| format!("failed to read {}", session_path.display()))?;
            let request_bytes = match &request {
                Some(value) => Some(serde_json::to_vec_pretty(value)?),
                None => None,
            };
            (raw, request_bytes, 0, 0)
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
            (anon.bytes, request_bytes, anon.malformed, anon.redacted)
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

    let mut members: Vec<(&str, Vec<u8>)> = vec![("session.jsonl", session_bytes)];
    if let Some(bytes) = request_bytes {
        members.push(("llm-request.json", bytes));
    }

    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&archive)
        .with_context(|| format!("failed to create {}", archive.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let mut entries = Vec::with_capacity(members.len());
    for (name, bytes) in &members {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())?;
        zip.write_all(bytes)?;
        entries.push((*name).to_string());
    }
    zip.finish()?;

    Ok(DumpReport {
        archive,
        kind,
        entries,
        subagents: 0,
        malformed,
        redacted,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn fixture(layout: &Layout) -> PathBuf {
        let dir = layout.sessions_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-01-01T00-00-00_sess-1.jsonl");
        let lines = [
            r#"{"type":"session","id":"sess-1","started":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work","model":"m1","version":1}"#,
            r#"{"type":"message","id":"m1","parent":null,"ts":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"RAWMARKER token ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#,
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    fn read_member(archive: &Path, name: &str) -> Option<String> {
        let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
        let mut entry = zip.by_name(name).ok()?;
        let mut out = String::new();
        entry.read_to_string(&mut out).unwrap();
        Some(out)
    }

    fn setup() -> (tempfile::TempDir, Layout, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("cwd");
        let home = root.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        let layout = Layout::with_user_dir(&cwd, home).unwrap();
        let session = fixture(&layout);
        (root, layout, session)
    }

    #[test]
    fn all_archive_contains_verbatim_session() {
        let (root, layout, session) = setup();
        let out = root.path().join("out");
        let report = dump_session(&layout, &session, DumpKind::All, &out).unwrap();
        assert_eq!(
            report.entries,
            vec!["session.jsonl".to_string(), "llm-request.json".to_string()]
        );
        assert_eq!(report.subagents, 0);
        let body = read_member(&report.archive, "session.jsonl").unwrap();
        assert!(body.contains("RAWMARKER"));
        assert!(report.to_string().contains("treat accordingly"));
    }

    #[test]
    fn anon_archive_has_no_raw_content() {
        let (root, layout, session) = setup();
        let out = root.path().join("out");
        let report = dump_session(&layout, &session, DumpKind::Anon, &out).unwrap();
        let body = read_member(&report.archive, "session.jsonl").unwrap();
        assert!(!body.contains("RAWMARKER"));
        assert!(!body.contains("ghp_aaaa"));
        assert!(body.contains("\"id\":\"sess-1\""));
        assert!(report.redacted >= 1);
        assert!(report.to_string().contains(REVIEW_NOTE));
    }

    #[test]
    fn refuses_out_dir_inside_sessions() {
        let (root, layout, session) = setup();
        let bad = layout.sessions_dir().join("nested");
        let err = dump_session(&layout, &session, DumpKind::All, &bad).unwrap_err();
        assert!(err.to_string().contains("refusing"));
        let _ = root;
    }
}
