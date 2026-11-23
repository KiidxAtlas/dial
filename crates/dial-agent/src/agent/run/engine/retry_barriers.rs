//! Failed-operation retry barriers with per-call release dependencies.
//!
//! Each blocked normalized call owns its original diagnostic and every dependency that can
//! release it (file targets and observed content versions, shell tree hash, native edit
//! witnesses, workspace access grants, eval namespaces, or background-job lifecycles). Keeping
//! all attributes on one `BlockedCall` record guarantees atomic removal and eliminates parallel
//! map drift across continuations and compaction.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::llm::ToolCall;
use crate::runtime::execution::{EvalNamespace, NativeEditRetry};
use crate::tools::Workspace;

#[derive(Default)]
struct FileTargets {
    paths: Vec<PathBuf>,
    versions: Vec<Option<[u8; 32]>>,
}

#[derive(Default)]
struct BlockedCall {
    /// Bounded original diagnostic, captured before harness feedback and never from refusals.
    diagnostic: Option<String>,
    /// Tree a failed shell command ran against. Another session can repair the checkout, so a
    /// later tree (different HEAD or uncommitted content) releases that command's barrier.
    shell_tree: Option<String>,
    /// Known files a failed file operation (`read`/`edit`/`write`) targeted plus their identities
    /// at settlement. External repairs release only calls whose own inputs changed.
    file_targets: Option<FileTargets>,
    /// Only native validators can declare missing witnesses. Reads of a path, prose and payload
    /// claims never substitute for exact current source and all required complete rows.
    native_witness: Option<NativeEditRetry>,
    /// Read targets refused by workspace confinement, plus normalized call key for checkpoint
    /// release once a matching grant admits one of them.
    access: Option<(String, Vec<String>)>,
    /// Eval namespace dependency, plus normalized call key repaired by an actual reset.
    eval: Option<(EvalNamespace, String)>,
    /// Background-job dependency (job name, call key) repaired by that job's lifecycle change.
    job: Option<(String, String)>,
}

/// Dependencies captured when a tool call settles with a failed status.
#[derive(Default)]
pub(super) struct FailureRecord {
    pub diagnostic: Option<String>,
    pub shell_tree: Option<String>,
    pub file_targets: Option<(Vec<PathBuf>, Vec<Option<[u8; 32]>>)>,
    pub native_witness: Option<NativeEditRetry>,
    pub access: Option<(String, Vec<String>)>,
    pub eval: Option<(EvalNamespace, String)>,
    pub job: Option<(String, String)>,
}

/// Single authoritative table of failed normalized calls withheld until their dependencies change.
#[derive(Default)]
pub(super) struct RetryBarriers {
    entries: BTreeMap<[u8; 32], BlockedCall>,
}

impl RetryBarriers {
    /// Record or update a failed normalized call and its release dependencies.
    /// The first non-empty original diagnostic is preserved so later updates cannot overwrite it.
    pub(super) fn record(&mut self, identity: [u8; 32], record: FailureRecord) {
        let entry = self.entries.entry(identity).or_default();
        if let Some(eval) = record.eval {
            entry.eval = Some(eval);
        }
        if let Some(job) = record.job {
            entry.job = Some(job);
        }
        // Native retry witnesses reflect the latest failure settlement, never a stale one.
        entry.native_witness = record.native_witness;
        if let Some(access) = record.access {
            entry.access = Some(access);
        }
        if let Some(diagnostic) = record.diagnostic.filter(|text| !text.trim().is_empty()) {
            entry.diagnostic.get_or_insert(diagnostic);
        }
        if let Some(tree) = record.shell_tree {
            entry.shell_tree = Some(tree);
        }
        if let Some((paths, versions)) = record.file_targets {
            entry.file_targets = Some(FileTargets { paths, versions });
        }
    }

    /// Snapshot blocked call identities before experimental early reads can start.
    pub(super) fn identities(&self) -> BTreeSet<[u8; 32]> {
        self.entries.keys().copied().collect()
    }

    /// Check whether `identity` is still blocked; if an external change (tree, tracked files, or
    /// native edit witnesses) has already satisfied its release condition, drop the entry and
    /// return `None`. Otherwise return `Some(original_diagnostic)` so the caller can format a
    /// retry-barrier refusal.
    pub(super) fn active_diagnostic(
        &mut self,
        workspace: &Workspace,
        call: &ToolCall,
        identity: &[u8; 32],
    ) -> Option<Option<String>> {
        let released = {
            let entry = self.entries.get(identity)?;
            Self::tree_changed(entry, workspace, call)
                || Self::sources_changed(entry, workspace)
                || Self::witnesses_ready(entry, workspace)
        };
        if released {
            self.entries.remove(identity);
            return None;
        }
        Some(self.entries.get(identity)?.diagnostic.clone())
    }

    /// Drop every entry whose native edit witnesses are now satisfied against current source.
    pub(super) fn release_ready_witnesses(&mut self, workspace: &Workspace) {
        self.entries
            .retain(|_, entry| !Self::witnesses_ready(entry, workspace));
    }

    /// Release barriers that a novel successful mutation (or environment-changing shell when
    /// `changed` is empty) can repair. Access refusals stay barred until a matching grant.
    /// Entries tied to known files release only when one of those files is in `changed`.
    pub(super) fn release_for_change(&mut self, changed: &[PathBuf]) {
        self.entries.retain(|_, entry| {
            if entry.access.is_some() {
                return true;
            }
            match &entry.file_targets {
                Some(targets) => !targets.paths.iter().any(|target| changed.contains(target)),
                None => false,
            }
        });
    }

    /// Release unknown-dependency barriers after a background job starts, keeping job-specific
    /// barriers (released only by that job's own lifecycle), file-tied barriers, and access
    /// barriers intact.
    pub(super) fn release_for_service_start(&mut self) {
        self.entries.retain(|_, entry| {
            entry.access.is_some() || entry.job.is_some() || entry.file_targets.is_some()
        });
    }

    /// Release failures bound to `namespace` after an actual `eval` reset, returning their
    /// normalized checkpoint call keys so the recovery checkpoint can release them in lockstep.
    pub(super) fn release_eval_namespace(&mut self, namespace: EvalNamespace) -> Vec<String> {
        self.remove_matching(|entry| {
            entry
                .eval
                .as_ref()
                .filter(|(ns, _)| *ns == namespace)
                .map(|(_, key)| key.clone())
        })
    }

    /// Release failures bound to a background job whose lifecycle changed (`None` = `kill all`),
    /// returning their normalized checkpoint call keys.
    pub(super) fn release_job_lifecycle(&mut self, job: Option<&str>) -> Vec<String> {
        self.remove_matching(|entry| {
            entry
                .job
                .as_ref()
                .filter(|(name, _)| job.is_none_or(|target| target == name))
                .map(|(_, key)| key.clone())
        })
    }

    /// Release confinement-refused reads whose target now resolves inside `workspace`, returning
    /// their normalized checkpoint call keys.
    pub(super) fn release_granted_access(&mut self, workspace: &Workspace) -> Vec<String> {
        self.remove_matching(|entry| {
            entry
                .access
                .as_ref()
                .filter(|(_, targets)| targets.iter().any(|t| workspace.resolve(t).is_ok()))
                .map(|(key, _)| key.clone())
        })
    }

    #[cfg(test)]
    pub(super) fn has_access_barriers(&self) -> bool {
        self.entries.values().any(|entry| entry.access.is_some())
    }

    fn remove_matching(
        &mut self,
        mut predicate: impl FnMut(&BlockedCall) -> Option<String>,
    ) -> Vec<String> {
        let mut released_ids = Vec::new();
        let mut released_keys = Vec::new();
        for (&identity, entry) in &self.entries {
            if let Some(key) = predicate(entry) {
                released_ids.push(identity);
                released_keys.push(key);
            }
        }
        for identity in released_ids {
            self.entries.remove(&identity);
        }
        released_keys
    }

    fn tree_changed(entry: &BlockedCall, workspace: &Workspace, call: &ToolCall) -> bool {
        // Explicit check inputs take precedence over a whole-tree fallback. An unrelated edit
        // must not authorize a retry of a check whose own source is unchanged.
        if entry.file_targets.is_some() {
            return false;
        }
        let Some(before) = entry.shell_tree.as_deref() else {
            return false;
        };
        crate::agent::verify::shared_runs::shell_tree(workspace, call)
            .is_some_and(|now| now != before)
    }

    fn sources_changed(entry: &BlockedCall, workspace: &Workspace) -> bool {
        if entry.access.is_some() {
            return false;
        }
        let Some(targets) = entry.file_targets.as_ref() else {
            return false;
        };
        let current: Vec<_> = targets
            .paths
            .iter()
            .map(|path| workspace.read_identity(&workspace.rel(path)))
            .collect();
        current != targets.versions
    }

    fn witnesses_ready(entry: &BlockedCall, workspace: &Workspace) -> bool {
        if entry.access.is_some() {
            return false;
        }
        entry.native_witness.as_ref().is_some_and(|retry| {
            crate::tools::edit::hashline::native_edit_retry_ready(workspace, retry)
        })
    }
}

#[cfg(test)]
mod tests;
