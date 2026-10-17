//! Filesystem watch for the working-tree panel. Events are filtered down to changes that can move
//! `git status` (worktree files outside ignored paths, and the git state files `index`, `HEAD`,
//! `packed-refs` and `refs/`), then debounced so a burst of writes triggers one refresh.

use crate::tools::vcs::git::run_checked;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

/// A refresh fires once events have been quiet this long...
const QUIET: Duration = Duration::from_millis(200);
/// ...or this long after the first event of a burst, whichever comes first.
const MAX_WAIT: Duration = Duration::from_secs(1);

pub(crate) struct Watch {
    /// Kept alive for the lifetime of the overlay; dropping it stops the watch.
    _watcher: RecommendedWatcher,
    rx: Receiver<()>,
    /// First and most recent event of the burst being debounced.
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Watch {
    /// Watches the worktree at `root` and its git state. Fails when git cannot name the git
    /// directory or the OS watcher cannot be created; the caller then falls back to polling.
    pub(super) fn start(root: &Path) -> Result<Self, String> {
        let gitdir = run_checked(root, &["rev-parse", "--absolute-git-dir"], false)
            .map(|out| PathBuf::from(out.trim()))?;
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let gitdir = gitdir.canonicalize().map_err(|e| e.to_string())?;

        let mut builder = GitignoreBuilder::new(&root);
        let _ = builder.add(root.join(".gitignore"));
        let ignore = builder.build().map_err(|e| e.to_string())?;
        let filter = Filter {
            root: root.clone(),
            gitdir: gitdir.clone(),
            ignore,
        };

        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let relevant = match result {
                    Ok(event) => {
                        !matches!(event.kind, EventKind::Access(_))
                            && (event.paths.is_empty()
                                || event.paths.iter().any(|path| filter.relevant(path)))
                    }
                    // Watcher errors may mean missed events; refresh rather than go stale.
                    Err(_) => true,
                };
                if relevant {
                    let _ = tx.send(());
                }
            })
            .map_err(|e| e.to_string())?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        // A gitdir inside the worktree is already covered by the recursive watch above.
        if !gitdir.starts_with(&root) {
            watcher
                .watch(&gitdir, RecursiveMode::NonRecursive)
                .map_err(|e| e.to_string())?;
            let refs = gitdir.join("refs");
            if refs.is_dir() {
                watcher
                    .watch(&refs, RecursiveMode::Recursive)
                    .map_err(|e| e.to_string())?;
            }
        }

        Ok(Self {
            _watcher: watcher,
            rx,
            first: None,
            last: None,
        })
    }

    /// Returns true once per debounced burst of relevant changes. Call on every overlay tick.
    pub(crate) fn take_ready(&mut self, now: Instant) -> bool {
        while self.rx.try_recv().is_ok() {
            self.first.get_or_insert(now);
            self.last = Some(now);
        }
        let (Some(first), Some(last)) = (self.first, self.last) else {
            return false;
        };
        if now.duration_since(last) < QUIET && now.duration_since(first) < MAX_WAIT {
            return false;
        }
        self.first = None;
        self.last = None;
        true
    }

    /// Drains pending events and reports whether any arrived. Undebounced; for tests.
    #[cfg(test)]
    fn poll(&self) -> bool {
        let mut any = false;
        while self.rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

/// Decides which watched paths can change `git status` or the HEAD commit.
struct Filter {
    root: PathBuf,
    gitdir: PathBuf,
    ignore: Gitignore,
}

impl Filter {
    fn relevant(&self, path: &Path) -> bool {
        if let Ok(inside) = path.strip_prefix(&self.gitdir) {
            return inside == Path::new("index")
                || inside == Path::new("HEAD")
                || inside == Path::new("packed-refs")
                || inside.starts_with("refs");
        }
        if path.strip_prefix(&self.root).is_err() {
            return false;
        }
        !self
            .ignore
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }
}

#[cfg(test)]
mod tests;
