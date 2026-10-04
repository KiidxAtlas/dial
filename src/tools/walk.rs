//! The one workspace file walker behind `glob`, `grep` and `ast_search`/`ast_edit`.
//!
//! * `.gitignore` rules apply (also without a `.git` directory), so build output and virtualenvs
//!   are not searched and do not eat the entry budget. Dotfiles are still visited.
//! * `.git`, `node_modules`, `target`, `.venv`, `__pycache__` and the top-level scratch directory are
//!   skipped even when no ignore file lists them. An explicit start path inside one still works.
//! * Symlinks are never followed or reported; entries are visited in sorted order depth-first.

use super::{SCRATCH_DIR, Workspace};
use ignore::WalkBuilder;
use std::path::Path;

pub(super) const MAX_WALK_ENTRIES: usize = 50_000;
pub(super) const MAX_WALK_DEPTH: usize = 40;
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

pub(super) struct Walk<'a> {
    ws: &'a Workspace,
    entries: usize,
    /// The walk stopped early (entry budget) or did not descend (depth limit): results may be incomplete.
    pub truncated: bool,
}

impl<'a> Walk<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        Walk { ws, entries: 0, truncated: false }
    }

    /// Visit every regular file under `start`. `visit` returns false to stop.
    pub fn run(&mut self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        let ws = self.ws;
        self.run_with_prefix(start, &ws.root, visit);
    }

    /// Keep only directories on the way to, or below, `prefix`. Start at the original root so
    /// pruning cannot bypass ancestor ignore rules, skipped directories, or symlink checks.
    pub fn run_with_prefix(&mut self, start: &Path, prefix: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        let prefix = prefix.to_path_buf();
        let root = self.ws.root.clone();
        let walker = WalkBuilder::new(start)
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .git_global(false)
            .ignore(false)
            .max_depth(Some(MAX_WALK_DEPTH))
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(move |entry| {
                if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
                    return true;
                }
                let name = entry.file_name();
                let scratch = name == SCRATCH_DIR && entry.path().parent() == Some(root.as_path());
                !(scratch || SKIPPED_DIRS.iter().any(|skip| name == *skip))
                    && (entry.path().starts_with(&prefix) || prefix.starts_with(entry.path()))
            })
            .build();
        for entry in walker.flatten() {
            self.entries += 1;
            if self.entries > MAX_WALK_ENTRIES {
                self.truncated = true;
                return;
            }
            let Some(kind) = entry.file_type() else { continue };
            if kind.is_dir() && entry.depth() >= MAX_WALK_DEPTH {
                self.truncated = true;
            } else if kind.is_file() && !visit(entry.path()) {
                return;
            }
        }
    }
}
