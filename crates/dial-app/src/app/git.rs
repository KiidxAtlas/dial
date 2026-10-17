//! Git status and diff probes, cached off the UI thread with a short time-to-live.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A git call may never hold the interface hostage.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

const GIT_TTL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Git and learning status, cached so a repaint never spawns a process.
// ---------------------------------------------------------------------------

/// Git status shown in the status line: (branch, optional `(ahead, behind, dirty)` counts).
type GitStatus = (String, Option<(u32, u32, u32)>);

/// Refresh optional status on a worker, keeping the last value while a query is pending.
pub(super) struct StatusCache<T> {
    pub(super) at: Option<Instant>,
    pub(super) worker: Option<JoinHandle<T>>,
    pub(super) value: T,
}

impl<T: Default> Default for StatusCache<T> {
    fn default() -> Self {
        Self {
            at: None,
            worker: None,
            value: T::default(),
        }
    }
}

impl<T: Send + 'static> StatusCache<T> {
    pub(super) fn refresh(&mut self, ttl: Duration, query: impl FnOnce() -> T + Send + 'static) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            if let Ok(value) = self.worker.take().unwrap().join() {
                self.value = value;
            }
            self.at = Some(Instant::now());
        }
        if self.worker.is_some() || self.at.is_some_and(|at| at.elapsed() < ttl) {
            return;
        }
        self.at = Some(Instant::now());
        self.worker = std::thread::Builder::new()
            .name("dial-status".into())
            .spawn(query)
            .ok();
    }
}

#[derive(Default)]
pub(super) struct Git {
    pub(super) root: Option<Arc<Path>>,
    pub(super) cache: StatusCache<GitStatus>,
    pub(super) files: StatusCache<Vec<crate::ui::rail::FileDiff>>,
}

impl Git {
    fn bind_root(&mut self, root: &Path) -> Option<Arc<Path>> {
        // The first paint may precede status initialization. An empty `git -C`
        // targets the launch directory, not the selected workspace.
        if root.as_os_str().is_empty() {
            return None;
        }
        if self.root.as_deref() != Some(root) {
            self.cache = StatusCache::default();
            self.files = StatusCache::default();
            self.root = Some(Arc::from(root));
        }
        self.root.clone()
    }

    pub(super) fn files(&mut self, root: &Path) -> Vec<crate::ui::rail::FileDiff> {
        let Some(root) = self.bind_root(root) else {
            return Vec::new();
        };
        self.files.refresh(GIT_TTL, move || {
            let mut files = git_output(
                &root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--numstat",
                    "-z",
                    "HEAD",
                ],
            )
            .map(|s| parse_file_diffs(&s))
            .unwrap_or_else(|| {
                // A newly initialized repository has no HEAD; show staged files without invented counts.
                git_output(&root, &["ls-files", "-z"])
                    .unwrap_or_default()
                    .split('\0')
                    .filter(|p| !p.is_empty())
                    .map(|p| crate::ui::rail::FileDiff {
                        path: p.into(),
                        untracked: true,
                        ..Default::default()
                    })
                    .collect()
            });
            for path in git_output(&root, &["ls-files", "--others", "--exclude-standard", "-z"])
                .unwrap_or_default()
                .split('\0')
                .filter(|p| !p.is_empty())
            {
                files.push(crate::ui::rail::FileDiff {
                    path: path.into(),
                    untracked: true,
                    ..Default::default()
                });
            }
            files.sort_by(|a, b| a.path.cmp(&b.path));
            files.dedup_by(|a, b| a.path == b.path);
            files
        });
        self.files.value.clone()
    }

    pub(super) fn read(&mut self, root: &Path) -> GitStatus {
        let Some(root) = self.bind_root(root) else {
            return GitStatus::default();
        };
        self.cache.refresh(GIT_TTL, move || {
            let branch = git_output(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty() && b != "HEAD");
            let counts = branch
                .as_ref()
                .and_then(|_| {
                    git_output(
                        &root,
                        &["status", "--porcelain=v1", "--untracked-files=normal"],
                    )
                })
                .map(|out| parse_status(&out));
            (branch.unwrap_or_default(), counts)
        });
        self.cache.value.clone()
    }
}

/// (staged, modified, untracked) from `git status --porcelain=v1`.
pub(super) fn parse_status(output: &str) -> (u32, u32, u32) {
    let (mut staged, mut modified, mut untracked) = (0, 0, 0);
    for line in output.lines() {
        let mut chars = line.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        if x == '?' && y == '?' {
            untracked += 1;
            continue;
        }
        if x != ' ' && x != '?' {
            staged += 1;
        }
        if y != ' ' && y != '?' {
            modified += 1;
        }
    }
    (staged, modified, untracked)
}

pub(super) fn parse_file_diffs(output: &str) -> Vec<crate::ui::rail::FileDiff> {
    let mut fields = output.split('\0');
    let mut out = Vec::new();
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().unwrap_or("")
        } else {
            path
        };
        if !path.is_empty() {
            out.push(crate::ui::rail::FileDiff {
                path: path.into(),
                added: added.parse().ok(),
                removed: removed.parse().ok(),
                untracked: false,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub(super) fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    use std::io::ErrorKind;
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut group = crate::tools::exec::shell::ProcessGroup::new(
        crate::platform::process::spawn_group(&mut command).ok()?,
    );
    let mut stdout = group.child.stdout.take()?;
    crate::platform::io::set_nonblocking(&stdout).ok()?;
    // This already runs on a status worker. Drain here while checking the
    // deadline, rather than waiting for exit with a potentially full pipe.
    let deadline = Instant::now() + GIT_TIMEOUT;
    let mut cleanup = None;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut eof = false;
    loop {
        for _ in 0..16 {
            match crate::platform::io::read_nonblocking(&mut stdout, &mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        let now = Instant::now();
        if group.exited().ok()? {
            group.terminate();
            let until = cleanup.get_or_insert(now + Duration::from_millis(300));
            if eof {
                break;
            }
            if now >= *until {
                return None;
            }
        } else if now >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = group.child.wait().ok()?;
    if status.success() {
        String::from_utf8(bytes).ok()
    } else {
        None
    }
}
