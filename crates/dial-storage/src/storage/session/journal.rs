//! Journal line I/O: exclusive writer claims, bounded line reads, torn-tail repair, entry encoding and the load path.

use super::*;

/// How long a reopen waits for a writer lock that looks busy.
const WRITER_CLAIM_WAIT: Duration = Duration::from_secs(2);

/// Claims the exclusive writer lock. A child forked by any thread (tool processes start with
/// fork+exec) shares the descriptor, and so the `flock`, until it execs; a writer that has just
/// closed can therefore look busy for a moment. Retry briefly before treating it as a live writer.
pub(super) fn claim_writer(file: &File, path: &Path) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + WRITER_CLAIM_WAIT;
    loop {
        match platform_fs::try_lock_journal(file, true) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(()) => {
                return Ok(
                    platform_fs::identity(file)? == platform_fs::path_identity(path)?
                        && platform_fs::link_count(file)? == 1,
                );
            }
            Err(error) => return Err(error),
        }
    }
}

/// Writer-claim outcome. Contention is a value so `open_or_fork` can fall back to a fork; every
/// other claim failure stays an error.
pub(super) enum Claimed {
    Owned(Box<Session>),
    Contended(io::Error),
}

pub(super) fn busy_session_error(path: &Path, error: io::Error) -> anyhow::Error {
    anyhow::Error::new(error).context(format!(
        "session {} is already open for writing; close its owning Dial session before resuming or renaming it",
        path.display()
    ))
}

/// Longest line we write or read. Longer lines on disk are skipped; an oversized message append is
/// stored truncated with an artifact pointer to the full original (see `fit_oversized_message`).
pub(super) const MAX_LINE: usize = 8 * 1024 * 1024;
/// Record types this build understands. Other types are skipped silently for forward compatibility.
const KNOWN_ENTRY_TYPES: &[&str] = &[
    "runtime",
    "message",
    "compaction",
    "custom",
    "reset_boundary",
    "meta",
    "label",
];

pub(super) struct RawLine {
    pub(super) consumed: usize,
    pub(super) terminated: bool,
    pub(super) oversized: bool,
}

/// Cuts the file after its last newline, dropping an unterminated final line, and syncs the cut.
fn truncate_torn_tail(file: &File) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut reader = file;
    let mut end = file.metadata()?.len();
    let mut chunk = [0u8; 8192];
    let cut = loop {
        if end == 0 {
            break 0;
        }
        let n = end.min(chunk.len() as u64) as usize;
        end -= n as u64;
        reader.seek(SeekFrom::Start(end))?;
        reader.read_exact(&mut chunk[..n])?;
        if let Some(newline) = chunk[..n].iter().rposition(|&byte| byte == b'\n') {
            break end + newline as u64 + 1;
        }
    };
    file.set_len(cut)?;
    crate::platform::fs::sync_file_data(file)
}

/// Reads one line into `buf` (without the newline). Lines over `MAX_LINE` are consumed and flagged
/// rather than buffered. `None` at EOF.
pub(super) fn read_bounded_line<R: BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<RawLine>> {
    buf.clear();
    let (mut consumed, mut oversized, mut terminated) = (0usize, false, false);
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !oversized {
            let body = if found { take - 1 } else { take };
            if buf.len() + body > MAX_LINE {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..body]);
            }
        }
        r.consume(take);
        consumed += take;
        if found {
            terminated = true;
            break;
        }
    }
    Ok((consumed > 0).then_some(RawLine {
        consumed,
        terminated,
        oversized,
    }))
}

pub(super) fn entry_json(e: &Entry, copied: bool) -> Value {
    let mut v = match &e.kind {
        Kind::Runtime(node) => {
            json!({"type":"runtime","id":e.id,"parent":e.parent,"ts":e.ts,"node":node})
        }
        Kind::Message(m) => {
            json!({"type": "message", "id": e.id, "parent": e.parent, "ts": e.ts, "message": m})
        }
        Kind::Title(title) => {
            json!({"type": "meta", "id": e.id, "parent": e.parent, "ts": e.ts, "kind": "title", "title": title})
        }
        Kind::Label { target, label } => {
            json!({"type": "label", "id": e.id, "parent": e.parent, "ts": e.ts, "target": target, "label": label})
        }
        Kind::Compaction {
            summary,
            first_kept,
            prefix,
        } => {
            json!({"type": "compaction", "id": e.id, "parent": e.parent, "ts": e.ts, "summary": summary, "first_kept": first_kept, "prefix": prefix})
        }
        Kind::Custom { custom_type, data } => {
            json!({"type":"custom","id":e.id,"parent":e.parent,"ts":e.ts,"custom_type":custom_type,"data":data})
        }
        Kind::ContextReset {
            bootstrap,
            notes,
            digest,
        } => {
            json!({"type":"reset_boundary","id":e.id,"parent":e.parent,"ts":e.ts,
                "version":1,"bootstrap":bootstrap,"notes":notes,"digest":digest})
        }
        Kind::Meta { kind, removed } => {
            json!({"type": "meta", "id": e.id, "parent": e.parent, "ts": e.ts, "kind": kind, "removed": removed})
        }
    };
    if copied {
        v["copied"] = Value::Bool(true);
    }
    v
}

fn parse_entry(v: &Value) -> Option<Entry> {
    let id = v.get("id")?.as_str()?;
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    let parent = v.get("parent").and_then(Value::as_str).map(str::to_string);
    let ts = v
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = match v.get("type")?.as_str()? {
        "runtime" => Kind::Runtime(Box::new(
            serde_json::from_value(v.get("node")?.clone()).ok()?,
        )),
        "message" => {
            let m = v.get("message")?;
            m.is_object().then_some(())?;
            m.get("role")?.as_str()?;
            Kind::Message(m.clone())
        }
        "compaction" => Kind::Compaction {
            summary: v.get("summary")?.as_str()?.to_string(),
            first_kept: v
                .get("first_kept")
                .and_then(Value::as_str)
                .map(str::to_string),
            prefix: v.get("prefix").and_then(Value::as_array).cloned(),
        },
        "custom" => Kind::Custom {
            custom_type: v
                .get("custom_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            data: v.get("data").cloned().unwrap_or(Value::Null),
        },
        "reset_boundary" => {
            // Even an invalid reset remains a visibility boundary. Never recover notebook
            // text or provider replay from before a deliberately cleared context.
            let bootstrap = v
                .get("bootstrap")
                .and_then(Value::as_array)
                .filter(|messages| v["version"] == 1 && valid_context_bootstrap(messages))
                .cloned()
                .unwrap_or_default();
            Kind::ContextReset {
                bootstrap,
                notes: v
                    .get("notes")
                    .filter(|data| v["version"] == 1 && context_notes_text(data).is_some())
                    .cloned(),
                digest: v
                    .get("digest")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }
        }
        "label" => Kind::Label {
            target: v.get("target")?.as_str()?.to_owned(),
            label: v.get("label")?.as_str()?.to_owned(),
        },
        "meta" if v["kind"] == "title" => Kind::Title(v["title"].as_str().unwrap_or("").to_owned()),
        "meta" => Kind::Meta {
            kind: v
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            removed: v.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        _ => return None,
    };
    Some(Entry {
        id: id.to_string(),
        parent,
        ts,
        kind,
    })
}

// ---------------------------------------------------------------- Session

impl Session {
    /// Claims the writer for `path`. Only a busy lock is `Contended`; ownership, link, and load
    /// failures are errors, so `open_or_fork` never forks over them.
    pub(super) fn claim_open(path: &Path) -> Result<Claimed> {
        crate::config::settings_store::validate_private_path(
            path.parent().context("session path has no parent")?,
        )?;
        let deadline = Instant::now() + WRITER_CLAIM_WAIT;
        let file = loop {
            let file = platform_fs::open_nofollow_nonblocking(
                OpenOptions::new().read(true).append(true),
                path,
            )
            .with_context(|| format!("opening session {}", path.display()))?;
            let meta = file.metadata()?;
            ensure!(
                meta.is_file()
                    && platform_fs::link_count(&file)? == 1
                    && platform_fs::owned_by_current_user(&file)?,
                "session writer requires an owner-owned regular file without hard links"
            );
            match claim_writer(&file, path) {
                Ok(true) => break file,
                Ok(false) if Instant::now() < deadline => continue,
                Ok(false) => {
                    return Ok(Claimed::Contended(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "session path changed while claiming writer",
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(Claimed::Contended(error));
                }
                Err(error) => return Err(busy_session_error(path, error)),
            }
        };
        // Parse the claimed inode, not a second path lookup that could have been replaced.
        let mut s = Self::load_file(path, file.try_clone()?)?;
        // A torn final line that was not loaded is an append that never completed. Sealing it with
        // the next append's newline would turn it into a malformed interior line, reported as
        // corruption on every later load; the owner removes those bytes instead.
        if s.pending_newline
            && s.recovery
                .iter()
                .any(|issue| issue.kind == JournalIssueKind::TornTail)
        {
            truncate_torn_tail(&file)
                .with_context(|| format!("removing the torn tail of {}", path.display()))?;
            s.pending_newline = false;
        }
        s.file = Some(file);
        s.runtime
            .journal(crate::runtime::stream_journal::Journal::open(path)?);
        s.repair_incomplete_tools()?;
        Ok(Claimed::Owned(Box::new(s)))
    }

    pub(super) fn load(path: &Path) -> Result<Session> {
        crate::config::settings_store::validate_private_path(
            path.parent().context("session path has no parent")?,
        )?;
        let file = platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path)
            .with_context(|| format!("opening session {}", path.display()))?;
        Self::load_file(path, file)
    }

    fn load_file(path: &Path, file: File) -> Result<Session> {
        let meta = file
            .metadata()
            .with_context(|| format!("session {}", path.display()))?;
        ensure!(meta.is_file(), "{} is not a session file", path.display());
        let mut reader = BufReader::new(file);
        let name_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(file_id)
            .map(str::to_string);
        let mut s = Session {
            runtime: crate::runtime::Tree::default(),
            id: String::new(),
            model: String::new(),
            started: String::new(),
            parent_id: None,
            path: Some(path.to_path_buf()),
            file: None,
            pending_newline: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            active: ActiveView::default(),
            history_generation_floor: 0,
            additional_dirs: Vec::new(),
            recovery: Vec::new(),
            io: SessionIoMetrics::default(),
        };
        let mut header_id: Option<String> = None;
        // A journal without a header marker is legacy (0); its records upgrade in memory.
        let mut schema = 0u32;
        let mut saw_header = false;
        let mut buf = Vec::new();
        let mut ends_with_newline = true;
        // Physical line of each loaded entry, for lineage diagnostics.
        let mut entry_lines: Vec<usize> = Vec::new();
        let mut line_no = 0usize;
        while let Some(line) = read_bounded_line(&mut reader, &mut buf)? {
            line_no += 1;
            ends_with_newline = line.terminated;
            // Only the final line can lack its newline: an interrupted append. It is tolerated and
            // never repaired here; the next append starts a fresh line.
            let issue = |kind: JournalIssueKind, detail: String| JournalDiagnostic {
                kind: if line.terminated {
                    kind
                } else {
                    JournalIssueKind::TornTail
                },
                line: line_no,
                entry_id: None,
                detail,
            };
            if line.oversized {
                s.recovery.push(issue(
                    JournalIssueKind::OversizedInterior,
                    format!("line is longer than {MAX_LINE} bytes and was not loaded"),
                ));
                continue;
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let mut v = match serde_json::from_slice::<Value>(&buf) {
                Ok(v) => v,
                Err(error) => {
                    s.recovery.push(issue(
                        JournalIssueKind::MalformedInterior,
                        format!("not valid JSON: {error}"),
                    ));
                    continue;
                }
            };
            if v.get("type").and_then(Value::as_str) == Some("session") {
                if !saw_header && s.entries.is_empty() {
                    saw_header = true;
                    schema = migrate::header_version(&v)
                        .with_context(|| format!("session {}", path.display()))?;
                    header_id = v
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|i| valid_id(i))
                        .map(str::to_string);
                    s.model = v
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    s.started = v
                        .get("started")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    s.parent_id = v
                        .get("forked_from")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    // Headers written before this field existed load with no extra roots.
                    s.additional_dirs = v
                        .get("additional_dirs")
                        .and_then(Value::as_array)
                        .map(|dirs| {
                            dirs.iter()
                                .filter_map(Value::as_str)
                                .map(PathBuf::from)
                                .collect()
                        })
                        .unwrap_or_default();
                }
                continue;
            }
            migrate::upgrade_record(schema, &mut v);
            let Some(entry) = parse_entry(&v) else {
                if let Some(kind) = v.get("type").and_then(Value::as_str)
                    && KNOWN_ENTRY_TYPES.contains(&kind)
                {
                    s.recovery.push(issue(
                        JournalIssueKind::MalformedInterior,
                        format!("{kind} record has an invalid shape and was not loaded"),
                    ));
                }
                continue;
            };
            if s.by_id.contains_key(&entry.id) {
                s.recovery.push(issue(
                    JournalIssueKind::MalformedInterior,
                    format!("duplicate entry id {} was not loaded", entry.id),
                ));
                continue;
            }
            s.leaf = Some(entry.id.clone());
            s.by_id.insert(entry.id.clone(), s.entries.len());
            s.observe_history_generation(&entry.kind);
            entry_lines.push(line_no);
            s.entries.push(entry);
        }
        // A parent outside this file leaves the chain above the entry unreachable. Nothing is
        // deleted: the active branch stops at the gap, as it always has.
        let missing: Vec<JournalDiagnostic> = s
            .entries
            .iter()
            .zip(&entry_lines)
            .filter_map(|(entry, &line)| {
                let parent = entry.parent.as_deref()?;
                (!s.by_id.contains_key(parent)).then(|| JournalDiagnostic {
                    kind: JournalIssueKind::MissingParent,
                    line,
                    entry_id: Some(entry.id.clone()),
                    detail: format!(
                        "parent {parent} is not in this journal; entries above it are unreachable from {}",
                        entry.id
                    ),
                })
            })
            .collect();
        s.recovery.extend(missing);
        s.pending_newline = !ends_with_newline;
        s.id = header_id
            .or(name_id)
            .unwrap_or_else(|| "unknown".to_string());
        s.rebuild_active();
        s.restore_runtime()?;
        Ok(s)
    }
}
