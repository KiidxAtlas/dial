//! Session construction and opening: create, open, ephemeral sessions, fork snapshots and recovery reads.

use super::*;

/// What an interactive resume got: the journal itself, or a sibling fork of it.
pub enum Resumed {
    /// The journal's writer lock was claimed; the caller resumes this session in place.
    Owned(Session),
    /// Another live writer holds the journal. `session` is a new file holding the active branch
    /// and its runtime streams, with `forked_from` set to `source_id`.
    Forked { source_id: String, session: Session },
}

impl Session {
    /// A new session file with its header written.
    pub fn create(layout: &Layout, meta: SessionMeta) -> Result<Session> {
        Self::create_imported(layout, meta, &ImportOrigin::default())
    }

    /// A new session whose header carries the source's `started` and `cwd` (foreign-session import).
    pub fn create_imported(
        layout: &Layout,
        meta: SessionMeta,
        origin: &ImportOrigin,
    ) -> Result<Session> {
        Self::create_file_at(layout, &meta.model, None, origin)
    }

    pub(super) fn create_file(
        layout: &Layout,
        model: &str,
        forked_from: Option<&str>,
    ) -> Result<Session> {
        Self::create_file_at(layout, model, forked_from, &ImportOrigin::default())
    }

    pub(super) fn create_file_at(
        layout: &Layout,
        model: &str,
        forked_from: Option<&str>,
        origin: &ImportOrigin,
    ) -> Result<Session> {
        let dir = layout.sessions_dir();
        crate::platform::atomic::private_dir(&dir)?;
        let ms = now_ms();
        let started = origin.started.clone().unwrap_or_else(|| iso(ms));
        let id = random_hex(8);
        let stamp = iso(ms).replace(['-', ':', '.'], "");
        let path = dir.join(format!("{stamp}_{id}.jsonl"));
        let mut header = json!({
            "type": "session", "id": id, "started": started,
            "cwd": origin.cwd.clone().unwrap_or_else(|| layout.project_root.clone()),
            "model": model, "version": env!("CARGO_PKG_VERSION"),
            "schema_version": migrate::CURRENT_SCHEMA_VERSION,
        });
        if let Some(parent) = forked_from {
            header["forked_from"] = json!(parent);
        }
        // Read access is required for the writer lock on Windows: an append-only handle lacks
        // FILE_WRITE_DATA, and LockFileEx refuses it with ERROR_ACCESS_DENIED (os error 5).
        let mut file = platform_fs::open_owner_only(
            OpenOptions::new().read(true).append(true).create_new(true),
            &path,
        )
        .with_context(|| format!("creating session {}", path.display()))?;
        let created = (|| -> Result<crate::runtime::stream_journal::Journal> {
            platform_fs::try_lock_journal(&file, true).context("claiming new session writer")?;
            file.write_all(format!("{header}\n").as_bytes())
                .context("writing session header")?;
            Ok(crate::runtime::stream_journal::Journal::create_new(&path)?)
        })();
        let journal = match created {
            Ok(journal) => journal,
            Err(error) => {
                // This call created the session file, so a failed claim or header leaves nothing behind.
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let runtime = crate::runtime::Tree::default();
        runtime.journal(journal);
        Ok(Session {
            runtime,
            id,
            model: model.to_string(),
            started,
            parent_id: forked_from.map(str::to_owned),
            path: Some(path),
            file: Some(file),
            pending_newline: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            active: ActiveView::default(),
            history_generation_floor: 0,
            additional_dirs: Vec::new(),
            recovery: Vec::new(),
            io: SessionIoMetrics::default(),
        })
    }

    /// In-memory only: nothing is written or indexed.
    pub fn ephemeral() -> Session {
        Session {
            runtime: crate::runtime::Tree::default(),
            id: random_hex(8),
            model: String::new(),
            started: iso_now(),
            parent_id: None,
            path: None,
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
        }
    }

    /// Opens an existing session for exclusive appending. A competing writer is
    /// refused before loading or repairing history; read-only inspection is separate.
    pub fn open(path: &Path) -> Result<Session> {
        match Self::claim_open(path)? {
            Claimed::Owned(session) => Ok(*session),
            Claimed::Contended(error) => Err(busy_session_error(path, error)),
        }
    }

    /// Interactive resume. An owned journal opens for appending exactly as `open` does. Only when
    /// another live writer holds the journal lock does the caller get a sibling fork of the active
    /// branch: a new file whose header names this session as `forked_from`, carrying the branch's
    /// runtime streams. The owner's files are never repaired, rewritten, or truncated, and any
    /// integrity finding (malformed or oversized line, missing parent) is an error, never a fork.
    pub fn open_or_fork(layout: &Layout, path: &Path) -> Result<Resumed> {
        // A journal whose recorded move left this path is stale: its history lives at the
        // destination, so resuming it in place would fork the destination's writer. Fork instead.
        let moved = transfers::moved_away_from(layout, path)?;
        if !moved.is_empty() {
            let snapshot = Self::read(path)?;
            if moved.iter().any(|t| t.session_id == snapshot.id) {
                return Self::fork_snapshot(layout, path, snapshot, "moved away", "");
            }
        }
        match Self::claim_open(path)? {
            Claimed::Owned(session) => Ok(Resumed::Owned(*session)),
            Claimed::Contended(error) => {
                let snapshot = Self::read(path)?;
                Self::fork_snapshot(
                    layout,
                    path,
                    snapshot,
                    "open in another Dial process",
                    &format!(": {error}"),
                )
            }
        }
    }

    /// Forks an unowned snapshot after refusing any integrity finding other than a torn tail.
    fn fork_snapshot(
        layout: &Layout,
        path: &Path,
        snapshot: Session,
        state: &str,
        detail: &str,
    ) -> Result<Resumed> {
        if let Some(issue) = snapshot
            .recovery
            .iter()
            .find(|d| d.kind != JournalIssueKind::TornTail)
        {
            bail!(
                "session {} is {state} and has an integrity issue at line {} ({}); refusing to fork it{detail}",
                path.display(),
                issue.line,
                issue.detail
            );
        }
        let source_id = snapshot.id.clone();
        let session = snapshot
            .fork(layout)
            .with_context(|| format!("forking session {}", path.display()))?;
        Ok(Resumed::Forked { source_id, session })
    }

    /// Inspect a persisted journal without claiming writer ownership or repairing it.
    /// Mutating methods refuse this snapshot; `fork` may create a separately owned journal.
    pub fn read(path: &Path) -> Result<Session> {
        Self::load(path)
    }

    /// Load-time findings about this journal, drained once. Reading never repairs or rewrites the file.
    pub fn take_recovery_diagnostics(&mut self) -> Vec<JournalDiagnostic> {
        std::mem::take(&mut self.recovery)
    }
}
