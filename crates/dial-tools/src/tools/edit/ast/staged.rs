//! Dry-run previews held for `ast_edit`'s `apply_token`.
//!
//! A preview that found changes is kept (bounded in count and bytes, oldest evicted first) under a
//! short random token. Applying a token runs the same commit as a full `dry_run: false` call, which
//! compares every file's current content with the content the preview was planned from, so a file
//! changed since the preview is refused rather than overwritten.

use super::Planned;
use parking_lot::Mutex;
use std::collections::VecDeque;

/// Previews held at once.
const MAX_STAGED: usize = 4;
/// Original plus rewritten text of all held previews.
const MAX_STAGED_BYTES: usize = 32 * 1024 * 1024;

struct Staged {
    token: String,
    files: Vec<Planned>,
    footer: String,
    bytes: usize,
}

/// Staged previews of one workspace, shared by its clones.
#[derive(Default)]
pub(crate) struct StagedPreviews(Mutex<VecDeque<Staged>>);

impl std::fmt::Debug for StagedPreviews {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StagedPreviews({})", self.0.lock().len())
    }
}

impl StagedPreviews {
    /// Hold a preview of `bytes` bytes and return its token, or `None` when it is too large to hold.
    pub(super) fn stage(
        &self,
        files: Vec<Planned>,
        footer: String,
        bytes: usize,
    ) -> Option<String> {
        if bytes > MAX_STAGED_BYTES {
            return None;
        }
        let token = format!("{:08x}", rand::random::<u32>());
        let mut held = self.0.lock();
        while held.len() >= MAX_STAGED
            || held.iter().map(|staged| staged.bytes).sum::<usize>() + bytes > MAX_STAGED_BYTES
        {
            held.pop_front();
        }
        held.push_back(Staged {
            token: token.clone(),
            files,
            footer,
            bytes,
        });
        Some(token)
    }

    /// The planned files and coverage footer held under `token`.
    pub(super) fn get(&self, token: &str) -> Option<(Vec<Planned>, String)> {
        self.0
            .lock()
            .iter()
            .find(|staged| staged.token == token)
            .map(|staged| (staged.files.clone(), staged.footer.clone()))
    }

    /// Drop a spent token.
    pub(super) fn forget(&self, token: &str) {
        self.0.lock().retain(|staged| staged.token != token);
    }
}
