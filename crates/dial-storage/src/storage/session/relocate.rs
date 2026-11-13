//! Moving a session between project layouts, together with its side-thread companions and removal.

use super::*;

/// Companion files a session move created at its destination. Discarding a moved copy removes only
/// these: a companion that already existed there belongs to someone else and is never touched.
#[derive(Clone, Copy, Default)]
pub(super) struct CreatedCompanions {
    pub(super) streams: bool,
    pub(super) side: bool,
}

/// Removes a partially moved session. The move created the main file, so it always goes; companions
/// go only when the move created them. Source files are never touched. Best effort; the caller
/// already has the error to report.
pub(super) fn discard_moved_copy(dst: &Path, created: CreatedCompanions) {
    if created.streams {
        let _ = crate::runtime::stream_journal::remove(dst);
    }
    if created.side {
        let _ = fs::remove_file(side_thread::path_for(dst));
    }
    let _ = fs::remove_file(dst);
}

/// Refuses a destination whose side-thread path is taken, even by a dangling symlink. A file there
/// is never adopted or overwritten.
pub fn ensure_side_thread_free(dst: &Path) -> Result<()> {
    let side = side_thread::path_for(dst);
    match fs::symlink_metadata(&side) {
        Ok(_) => bail!("side thread destination exists: {}", side.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", side.display())),
    }
}

/// Copies the source's side-thread file verbatim, pending work included, to the destination's side
/// path. Returns whether this call created the file; a failed copy removes only its own output.
fn copy_side_thread(src: &Path, dst: &Path) -> Result<bool> {
    let from = side_thread::path_for(src);
    let to = side_thread::path_for(dst);
    let mut input =
        match platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), &from) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).with_context(|| format!("reading {}", from.display())),
        };
    ensure!(
        input.metadata()?.is_file(),
        "side thread {} is not a regular file",
        from.display()
    );
    let mut out =
        platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), &to)
            .with_context(|| format!("creating {}", to.display()))?;
    let written = (|| -> io::Result<()> {
        io::copy(&mut input, &mut out)?;
        crate::platform::fs::sync_file(&out)?;
        #[cfg(unix)]
        if let Some(dir) = to.parent() {
            crate::platform::fs::sync_file(&fs::File::open(dir)?)?;
        }
        Ok(())
    })();
    drop(out);
    if let Err(error) = written {
        let _ = fs::remove_file(&to);
        return Err(error).with_context(|| format!("copying side thread to {}", to.display()));
    }
    Ok(true)
}

/// Removes a session's side-thread file, if one exists.
fn remove_side_thread(session: &Path) -> io::Result<()> {
    match fs::remove_file(side_thread::path_for(session)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
/// Makes a directory entry change durable. Unix only: Windows has no unprivileged directory flush.
pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        crate::platform::fs::sync_file(&fs::File::open(dir)?)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Moves a session's side-thread file to its archived or restored path, never replacing an existing
/// destination. A missing source is not an error; a non-regular source is refused untouched.
pub fn relocate_side_thread(source: &Path, destination: &Path) -> io::Result<()> {
    let from = side_thread::path_for(source);
    match fs::symlink_metadata(&from) {
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::other(format!(
                "side thread {} is not a regular file",
                from.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let to = side_thread::path_for(destination);
    match crate::runtime::stream_journal::publish_no_replace(&from, &to) {
        // A source that vanished between the check and the publish has nothing left to move.
        Err(error) if error.kind() == io::ErrorKind::NotFound && !from.exists() => Ok(()),
        result => result,
    }
}

impl Session {
    /// Deletes this session's journal, stream supplement and side-thread file, and releases its
    /// writer lock. Ephemeral and read-only snapshots are refused. Callers drop the pin with
    /// `session_pins::unpin`.
    pub fn remove_file(self) -> Result<()> {
        let path = self
            .path
            .clone()
            .context("ephemeral session has no file to delete")?;
        ensure!(
            self.file.is_some(),
            "session {} is a read-only snapshot",
            path.display()
        );
        ensure!(
            platform_fs::identity(self.file.as_ref().context("session writer unavailable")?)?
                == platform_fs::path_identity(&path)?,
            "session path changed before deletion"
        );
        // The main file goes first: if its removal fails, the session and its companions all survive.
        fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
        drop(self);
        crate::runtime::stream_journal::remove(&path).with_context(|| {
            format!(
                "session {} was deleted but its stream journal could not be removed",
                path.display()
            )
        })?;
        remove_side_thread(&path).with_context(|| {
            format!(
                "session {} was deleted but its side thread file could not be removed",
                path.display()
            )
        })
    }

    /// Copies this journal, stream supplement, side-thread file and referenced project-local artifacts
    /// into `target`, with the header cwd rewritten, then opens the copy as its writer. The caller
    /// swaps the copy in and calls `remove_file` on the old session. A side-thread file already at the
    /// destination is refused before anything is written. Pre-publication failures remove the copies
    /// this call created and leave the source untouched. Uncertain ledger publication retains both
    /// journals and reports their state. Reusable immutable artifact imports may remain in the target.
    pub fn relocate_to(&self, target: &Layout) -> Result<Session> {
        self.relocate_with(target, Session::open)
    }

    /// `relocate_to` with the reopen step injected so tests can fail it after the copy succeeds.
    pub(super) fn relocate_with(
        &self,
        target: &Layout,
        reopen: impl FnOnce(&Path) -> Result<Session>,
    ) -> Result<Session> {
        self.relocate_with_record(target, reopen, transfers::record)
    }

    /// Injection at the final publication boundary distinguishes refusal from uncertain durability.
    pub(super) fn relocate_with_record(
        &self,
        target: &Layout,
        reopen: impl FnOnce(&Path) -> Result<Session>,
        record: impl FnOnce(&Layout, &str, &Path, &Path) -> Result<()>,
    ) -> Result<Session> {
        let src = self
            .path
            .as_ref()
            .context("ephemeral session cannot be moved")?;
        ensure!(
            self.file.is_some(),
            "read-only session snapshot cannot be moved"
        );
        let mut src_reader =
            BufReader::new(File::open(src).with_context(|| format!("reading {}", src.display()))?);
        let mut first = Vec::new();
        src_reader
            .read_until(b'\n', &mut first)
            .with_context(|| format!("reading {}", src.display()))?;
        ensure!(first.last() == Some(&b'\n'), "session has no header line");
        first.pop();
        let mut header: Value = serde_json::from_slice(&first).context("parsing session header")?;
        header
            .as_object_mut()
            .context("session header is not an object")?
            .insert("cwd".to_owned(), json!(target.project_root));
        let dir = target.sessions_dir();
        crate::platform::atomic::private_dir(&dir)?;
        let name = src.file_name().context("session path has no file name")?;
        let dst = dir.join(name);
        ensure_side_thread_free(&dst)?;
        let mut out =
            platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), &dst)
                .with_context(|| format!("creating {}", dst.display()))?;
        // Only the header is parsed; the remaining bytes are copied verbatim so a torn or
        // non-UTF-8 tail never blocks the move.
        let written = (|| -> io::Result<()> {
            out.write_all(header.to_string().as_bytes())?;
            out.write_all(b"\n")?;
            io::copy(&mut src_reader, &mut out)?;
            crate::platform::fs::sync_file(&out)
        })();
        drop(out);
        // The caller deletes the old session next, so the new directory entry must be durable.
        #[cfg(unix)]
        let written = written
            .and_then(|()| fs::File::open(&dir).and_then(|d| crate::platform::fs::sync_file(&d)));
        if let Err(err) = written {
            discard_moved_copy(&dst, CreatedCompanions::default());
            return Err(err).context("writing moved session");
        }
        let mut created = CreatedCompanions::default();
        if let Err(error) = crate::runtime::stream_journal::copy(src, &dst) {
            discard_moved_copy(&dst, created);
            return Err(error);
        }
        created.streams = true;
        match copy_side_thread(src, &dst) {
            Ok(side) => created.side = side,
            Err(error) => {
                discard_moved_copy(&dst, created);
                return Err(error);
            }
        }
        let transfer = (|| -> Result<()> {
            let source_root = artifact_root(src)?;
            let mut references = HashSet::new();
            let mut reader = BufReader::new(File::open(&dst)?);
            let mut line = Vec::new();
            // Use the normal journal reader's admission bounds. Malformed/torn/oversized rows
            // are not live dependencies; all admitted rows count, not just the active branch.
            while let Some(raw) = read_bounded_line(&mut reader, &mut line)? {
                if raw.terminated
                    && !raw.oversized
                    && let Ok(value) = serde_json::from_slice::<Value>(&line)
                {
                    crate::storage::artifact::transfer::collect_references(
                        &value,
                        &source_root,
                        &mut references,
                    );
                }
            }
            // Replay joins split append deltas and keeps nodes from inactive branches too.
            for node in crate::runtime::stream_journal::recover(&dst)? {
                crate::storage::artifact::transfer::collect_references(
                    &serde_json::to_value(node)?,
                    &source_root,
                    &mut references,
                );
            }
            if !references.is_empty() {
                let source = crate::storage::artifact::ArtifactStore::new(&source_root)?;
                source.expand_evidence_references(&mut references)?;
                let destination =
                    crate::storage::artifact::ArtifactStore::new(&artifact_root(&dst)?)?;
                for uri in references {
                    source
                        .transfer_to(&destination, &uri)
                        .with_context(|| format!("transferring moved session artifact {uri}"))?;
                }
            }
            Ok(())
        })();
        if let Err(error) = transfer {
            discard_moved_copy(&dst, created);
            return Err(error);
        }
        let moved = reopen(dst.as_path()).inspect_err(|_| discard_moved_copy(&dst, created))?;
        if let Err(error) = record(target, self.id(), src, &dst) {
            // Private-state publication may fail after rename. Only a failure known to precede
            // publication permits destination cleanup; unreadable or corrupt state proves nothing.
            if transfers::definitely_unpublished(&error) {
                return Err(match moved.remove_file() {
                    Ok(()) => error.context("recording moved session failed; source retained and destination removed"),
                    Err(cleanup) => error.context(format!("recording moved session failed; source retained, but destination cleanup failed: {cleanup:#}")),
                });
            }
            return Err(error.context(format!(
                "transfer ledger publication uncertain; source and destination {} retained; no rollback claimed",
                dst.display()
            )));
        }
        Ok(moved)
    }

    /// Reverts a relocation before workspace publication, retaining the source writer.
    ///
    /// The ledger is settled before deleting the copied journals. If ledger rollback cannot be
    /// acknowledged, the destination is retained and the error must not be presented as rollback.
    pub fn rollback_relocation(self, target: &Layout, source: &Session) -> Result<()> {
        ensure!(
            self.id() == source.id(),
            "relocation source has a different session id"
        );
        let from = source.path().context("relocation source has no journal")?;
        let to = self
            .path()
            .context("relocation destination has no journal")?;
        ensure!(
            from != to,
            "relocation source and destination are the same journal"
        );
        ensure!(
            source.file.is_some(),
            "relocation rollback requires the source writer"
        );
        ensure!(
            to.parent() == Some(target.sessions_dir().as_path()),
            "relocation target does not own the copied journal"
        );
        transfers::rollback(target, self.id(), from, to)
            .context("transfer ledger rollback uncertain; copied journals retained")?;
        self.remove_file()
            .context("transfer ledger rolled back, but copied journal cleanup failed")
    }
}
