//! Local prompt handoff. Only the writer holds the ephemeral inbox; a stale viewer can
//! never address a replacement owner, even if the same journal is reopened immediately.
use super::directory;
use crate::config::paths::Layout;
use crate::platform::{atomic, procinfo::ProcId};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Endpoint {
    process: ProcId,
    generation: String,
}

impl Endpoint {
    pub(crate) fn lookup(layout: &Layout, session: &str) -> Result<Self> {
        let dir = directory(layout, session)?;
        let bytes = atomic::read_private(&dir.join("attach.json"))?
            .context("No attachable owner. Resume the session if it ended, or update its running Dial engine.")?;
        let endpoint: Self = serde_json::from_slice(&bytes)?;
        ensure!(endpoint.generation.starts_with("attach-") && endpoint.generation.len() == 23 && endpoint.generation.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'), "invalid attachment generation");
        ensure!(endpoint.process.alive() && dir.join(&endpoint.generation).is_dir(), "The session owner has closed; resume the session instead");
        Ok(endpoint)
    }

    pub(crate) fn queue(layout: &Layout, session: &str, text: &str) -> Result<()> {
        Self::lookup(layout, session)?.send(layout, session, text)
    }

    pub(crate) fn send(&self, layout: &Layout, session: &str, text: &str) -> Result<()> {
        ensure!(!text.trim().is_empty() && text.len() <= 65536, "A prompt must be 1 to 65536 bytes");
        ensure!(*self == Self::lookup(layout, session)?, "Session ownership changed; reopen the view");
        let dir = directory(layout, session)?.join(&self.generation);
        atomic::validate_private_path(&dir)?;
        let mut staged = tempfile::NamedTempFile::new_in(&dir)?;
        staged.write_all(text.as_bytes())?;
        staged.persist_noclobber(dir.join("prompt")).map_err(|error| match error.error.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow::anyhow!("Another prompt is already queued; try again after it starts"),
            _ => anyhow::Error::new(error.error),
        })?;
        Ok(())
    }
}

pub(crate) struct Owner {
    session: String,
    inbox: tempfile::TempDir,
}

impl Owner {
    pub(crate) fn open(layout: &Layout, session: &str) -> Result<Self> {
        let dir = directory(layout, session)?;
        atomic::private_dir(&dir)?;
        let inbox = tempfile::Builder::new().prefix("attach-").rand_bytes(16).tempdir_in(&dir)?;
        let endpoint = Endpoint { process: ProcId::current().context("cannot identify session owner")?, generation: inbox.path().file_name().context("missing inbox name")?.to_string_lossy().into_owned() };
        atomic::write_private_volatile(&dir.join("attach.json"), &serde_json::to_vec(&endpoint)?)?;
        Ok(Self { session: session.into(), inbox })
    }

    pub(crate) fn session(&self) -> &str { &self.session }

    pub(crate) fn pending(&self) -> bool { self.inbox.path().join("prompt").is_file() }

    pub(crate) fn take(&self) -> Result<Option<String>> {
        let path: PathBuf = self.inbox.path().join("prompt");
        if !path.exists() { return Ok(None); }
        let bytes = atomic::read_private(&path)?;
        std::fs::remove_file(path)?;
        let text = String::from_utf8(bytes.context("attached prompt vanished")?)?;
        ensure!(!text.trim().is_empty() && text.len() <= 65536, "invalid attached prompt");
        Ok(Some(text))
    }
}
