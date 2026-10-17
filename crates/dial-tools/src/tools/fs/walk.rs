//! The one workspace file walker behind `glob`, `grep` and `ast_search`/`ast_edit`.
//!
//! * `.gitignore` rules apply (also without a `.git` directory), so build output and virtualenvs
//!   are not searched and do not eat the entry budget. Dotfiles are still visited.
//! * `.git`, `node_modules`, `target`, `.venv`, `__pycache__` and the top-level scratch directory are
//!   skipped even when no ignore file lists them. An explicit start path inside one still works.
//! * Nested linked git worktrees (a `.git` file pointing into another repository's `worktrees/`)
//!   are separate checkouts, not project files, and are skipped the same way.
//! * Symlinks are never followed or reported; entries are visited in sorted order depth-first.

use crate::tools::Workspace;
use crate::workspace_copy::SCRATCH_DIR;
use ignore::WalkBuilder;
use std::path::Path;

pub(in crate::tools) const MAX_WALK_ENTRIES: usize = 50_000;
pub(in crate::tools) const MAX_WALK_DEPTH: usize = 40;
pub(in crate::tools) const SKIPPED_DIRS: &[&str] =
    &[".git", "node_modules", "target", ".venv", "__pycache__"];

/// A nested linked worktree: its `.git` is a small file whose `gitdir:` points into some
/// repository's `worktrees/` directory. Submodules (`gitdir: …/modules/…`) are project content.
pub(crate) fn is_linked_worktree(directory: &Path) -> bool {
    let marker = directory.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&marker) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 4096 {
        return false;
    }
    std::fs::read_to_string(&marker).is_ok_and(|text| {
        text.trim().strip_prefix("gitdir:").is_some_and(|gitdir| {
            Path::new(gitdir.trim())
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "worktrees")
        })
    })
}

pub(crate) struct Walk<'a> {
    ws: &'a Workspace,
    entries: usize,
    /// The walk stopped early (entry budget) or did not descend (depth limit): results may be incomplete.
    pub truncated: bool,
}

impl<'a> Walk<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        Walk {
            ws,
            entries: 0,
            truncated: false,
        }
    }

    /// Visit every regular file under `start`. `visit` returns false to stop.
    pub fn run(&mut self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        let ws = self.ws;
        self.run_with_prefix(start, &ws.root, visit);
    }

    /// Keep only directories on the way to, or below, `prefix`. Start at the original root so
    /// pruning cannot bypass ancestor ignore rules, skipped directories, or symlink checks.
    pub fn run_with_prefix(
        &mut self,
        start: &Path,
        prefix: &Path,
        visit: &mut dyn FnMut(&Path) -> bool,
    ) {
        let prefix = prefix.to_path_buf();
        let root = self.ws.root.clone();
        let ws = self.ws.clone();
        let walker = WalkBuilder::new(start)
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .git_global(false)
            .ignore(false)
            .max_depth(Some(MAX_WALK_DEPTH))
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(move |entry| {
                if ws.is_protected(entry.path()) {
                    return false;
                }
                if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
                    return true;
                }
                let name = entry.file_name();
                let scratch = name == SCRATCH_DIR && entry.path().parent() == Some(root.as_path());
                !(scratch || SKIPPED_DIRS.iter().any(|skip| name == *skip))
                    && (entry.path().starts_with(&prefix) || prefix.starts_with(entry.path()))
                    && !is_linked_worktree(entry.path())
            })
            .build();
        for entry in walker {
            let Ok(entry) = entry else {
                // Unreadable entries leave the result incomplete; say so instead of hiding them.
                self.truncated = true;
                continue;
            };
            self.entries += 1;
            if self.entries > MAX_WALK_ENTRIES {
                self.truncated = true;
                return;
            }
            let Some(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && entry.depth() >= MAX_WALK_DEPTH {
                self.truncated = true;
            } else if kind.is_file() && !visit(entry.path()) {
                return;
            }
        }
    }
}

/// Search-only paging walker. Directory admission is bounded before sorting;
/// ignored/policy directories are pruned before their contents are inspected.
/// The legacy walker retains its callers' defaults.
pub(in crate::tools) struct SearchWalk<'a> {
    ws: &'a Workspace,
    hidden: bool,
    gitignore: bool,
    resume: Option<std::path::PathBuf>,
    entries: usize,
    directory_entries: usize,
    ignore_bytes: usize,
    unreadable: usize,
    unreadable_directories: usize,
    oversized_directories: usize,
    excluded: usize,
    symlinks: usize,
    depth_capped: usize,
    stopped: bool,
    omissions: Vec<String>,
    pub next: Option<std::path::PathBuf>,
}

impl<'a> SearchWalk<'a> {
    pub fn new(
        ws: &'a Workspace,
        hidden: bool,
        gitignore: bool,
        resume: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            ws,
            hidden,
            gitignore,
            resume,
            entries: 0,
            directory_entries: 0,
            ignore_bytes: 0,
            unreadable: 0,
            unreadable_directories: 0,
            oversized_directories: 0,
            excluded: 0,
            symlinks: 0,
            depth_capped: 0,
            omissions: Vec::new(),
            next: None,
            stopped: false,
        }
    }

    pub fn run(&mut self, start: &Path, visit: &mut dyn FnMut(&Path) -> bool) {
        let ws = self.ws;
        self.run_with_prefix(start, &ws.root, visit);
    }

    pub fn run_with_prefix(
        &mut self,
        start: &Path,
        prefix: &Path,
        visit: &mut dyn FnMut(&Path) -> bool,
    ) {
        let kind = match std::fs::symlink_metadata(start) {
            Ok(meta) => meta.file_type(),
            Err(_) => {
                self.unreadable += 1;
                self.omit(start, "unreadable scope");
                return;
            }
        };
        if !self.walk_path(start, kind, prefix, 0, &mut Vec::new(), visit) {
            self.stopped = true;
        }
    }

    fn omit(&mut self, path: &Path, reason: &str) {
        if self.omissions.len() < 8 {
            let relative = self.ws.rel(path);
            let shown: String = relative.chars().take(300).collect();
            self.omissions.push(format!(
                "{reason}: {shown}{}",
                if shown.len() < relative.len() {
                    " …[path truncated]"
                } else {
                    ""
                }
            ));
        }
    }

    fn walk_path(
        &mut self,
        path: &Path,
        kind: std::fs::FileType,
        prefix: &Path,
        depth: usize,
        rules: &mut Vec<ignore::gitignore::Gitignore>,
        visit: &mut dyn FnMut(&Path) -> bool,
    ) -> bool {
        if self.ws.is_protected(path) {
            self.excluded += 1;
            return true;
        }
        let directory = kind.is_dir();
        if depth > 0 && !path.starts_with(prefix) && !(directory && prefix.starts_with(path)) {
            return true;
        }
        if let Some(after) = &self.resume
            && path < after.as_path()
            && !(directory && after.starts_with(path))
        {
            return true;
        }
        if self
            .resume
            .as_ref()
            .is_none_or(|after| path >= after.as_path())
        {
            if self.entries >= MAX_WALK_ENTRIES {
                self.next = Some(path.to_path_buf());
                return false;
            }
            self.entries += 1;
        }
        if depth > 0 {
            let name = path.file_name().unwrap_or_default();
            let policy = directory
                && (name == ".git"
                    || (name == SCRATCH_DIR && path.parent() == Some(self.ws.root.as_path()))
                    || (self.gitignore && SKIPPED_DIRS.iter().any(|skip| name == *skip))
                    || is_linked_worktree(path));
            let hidden = !self.hidden && name.to_string_lossy().starts_with('.');
            let ignored = self.gitignore
                && rules
                    .iter()
                    .rev()
                    .find_map(|rule| {
                        let matched = rule.matched(path, directory);
                        if matched.is_none() {
                            None
                        } else {
                            Some(matched.is_ignore())
                        }
                    })
                    .unwrap_or(false);
            if policy || hidden || ignored {
                self.excluded += 1;
                return true;
            }
        }
        if kind.is_symlink() {
            self.symlinks += 1;
            return true;
        }
        if kind.is_file() {
            return visit(path);
        }
        if !directory {
            return true;
        }
        if depth >= MAX_WALK_DEPTH {
            self.depth_capped += 1;
            self.omit(path, "depth-capped directory");
            return true;
        }
        if !self
            .ws
            .resolve(&self.ws.rel(path))
            .is_ok_and(|resolved| resolved == path)
        {
            self.unreadable += 1;
            self.unreadable_directories += 1;
            self.omit(path, "unconfined or changed directory");
            return true;
        }
        let mut iterator = match std::fs::read_dir(path) {
            Ok(iterator) => iterator,
            Err(_) => {
                self.unreadable += 1;
                self.unreadable_directories += 1;
                self.omit(path, "unreadable directory");
                return true;
            }
        };
        // Collect only a bounded admitted directory, never an unbounded
        // underlying walker sort. Store each name/type once, not full paths.
        let mut children = Vec::new();
        loop {
            if self.directory_entries >= MAX_WALK_ENTRIES * (MAX_WALK_DEPTH + 1) {
                self.next = Some(path.to_path_buf());
                return false;
            }
            let Some(entry) = iterator.next() else { break };
            self.directory_entries += 1;
            if children.len() >= MAX_WALK_ENTRIES {
                self.oversized_directories += 1;
                self.omit(
                    path,
                    "oversized directory (>50000 immediate entries); narrow scope",
                );
                return true;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    self.unreadable += 1;
                    self.unreadable_directories += 1;
                    self.omit(path, "unreadable directory entry; directory omitted");
                    return true;
                }
            };
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    self.unreadable += 1;
                    self.unreadable_directories += 1;
                    self.omit(path, "unreadable directory entry type; directory omitted");
                    return true;
                }
            };
            children.push((entry.file_name(), kind));
        }
        children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let inherited = rules.len();
        if self.gitignore {
            match bounded_ignore(path, &path.join(".gitignore"), &mut self.ignore_bytes) {
                Ok(Some(rule)) => rules.push(rule),
                Ok(None) => {}
                Err(()) => {
                    self.unreadable += 1;
                    self.omit(path, "unavailable local ignore rules");
                }
            }
        }
        for (name, kind) in children {
            if !self.walk_path(&path.join(name), kind, prefix, depth + 1, rules, visit) {
                rules.truncate(inherited);
                return false;
            }
        }
        rules.truncate(inherited);
        true
    }

    /// Nothing was stopped, capped, oversized or unreadable: the walk covered its whole scope
    /// except entries excluded by ignore rules or policy.
    pub fn is_complete(&self) -> bool {
        !self.stopped
            && self.oversized_directories == 0
            && self.unreadable == 0
            && self.depth_capped == 0
            && self.omissions.is_empty()
    }

    /// Full counters only when something limited the walk; a complete walk gets one short line.
    pub fn coverage(&self) -> String {
        if self.is_complete() {
            return format!(
                "walk complete; excluded_by_ignore_or_policy={}; hidden={}; gitignore={}",
                self.excluded, self.hidden, self.gitignore
            );
        }
        format!(
            "walk_entries={}; directory_entries_read={}; directory_entry_limit={}; oversized_directories={}; unreadable_directories={}; unreadable_entries_or_ignore_rules={}; excluded_hidden_policy_or_ignore_entries={}; symlinks={}; depth_capped_directories={}; hidden={}; gitignore={}; ignored_subtrees={} (not enumerated); local_ignore_limit=1MiB/file,4MiB/call; truncated={}; omitted_paths={}",
            self.entries,
            self.directory_entries,
            MAX_WALK_ENTRIES * (MAX_WALK_DEPTH + 1),
            self.oversized_directories,
            self.unreadable_directories,
            self.unreadable,
            self.excluded,
            self.symlinks,
            self.depth_capped,
            self.hidden,
            self.gitignore,
            if self.gitignore {
                "excluded by ignore rules"
            } else {
                "included"
            },
            self.stopped
                || self.oversized_directories > 0
                || self.unreadable > 0
                || self.depth_capped > 0,
            serde_json::to_string(&self.omissions).unwrap_or_else(|_| "[]".into()),
        )
    }
}

/// At most 1 MiB per ignore file and 4 MiB per walk; never open a symlink.
pub(in crate::tools) fn bounded_ignore(
    directory: &Path,
    path: &Path,
    consumed: &mut usize,
) -> Result<Option<ignore::gitignore::Gitignore>, ()> {
    use std::io::Read;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.len() > 1024 * 1024
        || *consumed + 1024 * 1024 + 1 > 4 * 1024 * 1024
    {
        return Err(());
    }
    let mut source = String::new();
    let mut file = open_search_file(path).map_err(|_| ())?;
    let result = file
        .by_ref()
        .take(1024 * 1024 + 1)
        .read_to_string(&mut source);
    *consumed += source.len();
    result.map_err(|_| ())?;
    if source.len() > 1024 * 1024 {
        return Err(());
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
    for line in source.lines() {
        builder
            .add_line(Some(path.to_path_buf()), line)
            .map_err(|_| ())?;
    }
    builder.build().map(Some).map_err(|_| ())
}

/// Final-component no-follow closes the common metadata/open symlink race.
/// Ancestor replacement races remain subject to Workspace's documented boundary.
pub(in crate::tools) fn open_search_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}
