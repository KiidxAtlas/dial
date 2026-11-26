//! Files that usually change together, read from recent git history. After an edit, Dial names the
//! files that were committed together with the edited one in most of its recent commits, so a
//! missed companion (a test, a schema, a changelog entry) is visible before the turn ends.
//! The note is advisory: history suggests where to look, it never requires a change.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Recent commits read from history.
const COMMITS: usize = 300;
/// Commits touching more files than this are sweeping changes (renames, formatting, releases) and
/// say nothing about which files belong together.
const MAX_FILES_PER_COMMIT: usize = 20;
/// Fewest shared commits for a partner to be named.
const MIN_SHARED: u32 = 3;
/// Smallest share of the edited file's commits that must also touch the partner.
const MIN_SHARE: f64 = 0.4;
/// Most partners named for one edited file.
const MAX_PARTNERS: usize = 3;
/// Longest wait for git while an edit result is being built.
const GIT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a read history is reused before HEAD is checked again.
const RECHECK: Duration = Duration::from_secs(30);

/// Commit counts over the recent history of one repository view.
#[derive(Default)]
pub(crate) struct History {
    /// Commits touching each file.
    touches: HashMap<String, u32>,
    /// Commits touching both files, keyed by each file of the pair.
    pairs: HashMap<String, HashMap<String, u32>>,
}

impl History {
    /// Counts from `git log --name-only --format=%x1e` output: each commit starts with `\x1e`,
    /// followed by the paths it changed, one per line.
    pub(crate) fn parse(log: &str) -> Self {
        let mut history = Self::default();
        for commit in log.split('\u{1e}') {
            let files: Vec<&str> = commit
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            if files.is_empty() || files.len() > MAX_FILES_PER_COMMIT {
                continue;
            }
            for file in &files {
                *history.touches.entry((*file).to_owned()).or_default() += 1;
            }
            for a in &files {
                let partners = history.pairs.entry((*a).to_owned()).or_default();
                for b in files.iter().filter(|b| *b != a) {
                    *partners.entry((*b).to_owned()).or_default() += 1;
                }
            }
        }
        history
    }

    /// Files that changed with `file` often enough to mention, strongest first, as
    /// `(path, shared commits, commits touching file)`. `skip` paths are left out.
    pub(crate) fn partners(
        &self,
        file: &str,
        skip: &dyn Fn(&str) -> bool,
    ) -> Vec<(String, u32, u32)> {
        let Some(&total) = self.touches.get(file) else {
            return Vec::new();
        };
        let mut found: Vec<(String, u32, u32)> = self
            .pairs
            .get(file)
            .into_iter()
            .flatten()
            .filter(|&(_, &shared)| {
                shared >= MIN_SHARED && f64::from(shared) >= MIN_SHARE * f64::from(total)
            })
            .filter(|(path, _)| !skip(path))
            .map(|(path, &shared)| (path.clone(), shared, total))
            .collect();
        found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        found.truncate(MAX_PARTNERS);
        found
    }
}

struct Cached {
    head: Option<String>,
    checked: Instant,
    history: std::sync::Arc<History>,
}

/// Read histories by workspace root, and the edited files already given a note this process.
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Cached>>> = LazyLock::new(Mutex::default);
static SHOWN: LazyLock<Mutex<HashSet<(PathBuf, String)>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The history of `root`, reread only when HEAD has moved. `None` outside a git repository, in a
/// repository without commits, or when git does not answer in time.
fn history(root: &Path) -> Option<std::sync::Arc<History>> {
    if let Some(cached) = lock(&CACHE).get(root)
        && cached.checked.elapsed() < RECHECK
    {
        return cached.head.as_ref().map(|_| cached.history.clone());
    }
    let head = super::git::run_read(
        root,
        &["rev-parse", "--verify", "-q", "HEAD"],
        &[],
        GIT_TIMEOUT,
    )
    .ok()
    .map(|out| out.trim().to_owned())
    .filter(|head| !head.is_empty());
    if let Some(cached) = lock(&CACHE).get_mut(root)
        && cached.head == head
    {
        cached.checked = Instant::now();
        return head.map(|_| cached.history.clone());
    }
    let history = match &head {
        Some(_) => {
            let count = format!("-n{COMMITS}");
            // Paths relative to the workspace, unquoted, so they compare with the edited paths.
            let unquoted = std::ffi::OsStr::new("false");
            let log = super::git::run_read(
                root,
                &[
                    "log",
                    "--no-merges",
                    "--no-renames",
                    "--relative",
                    "--name-only",
                    "--format=%x1e",
                    &count,
                ],
                &[
                    ("GIT_CONFIG_COUNT", std::ffi::OsStr::new("1")),
                    ("GIT_CONFIG_KEY_0", std::ffi::OsStr::new("core.quotePath")),
                    ("GIT_CONFIG_VALUE_0", unquoted),
                ],
                GIT_TIMEOUT,
            )
            .ok()?;
            History::parse(&log)
        }
        None => History::default(),
    };
    let history = std::sync::Arc::new(history);
    lock(&CACHE).insert(
        root.to_path_buf(),
        Cached {
            head: head.clone(),
            checked: Instant::now(),
            history: history.clone(),
        },
    );
    head.map(|_| history)
}

/// A one-line note per edited file naming the files that usually change with it, for files not given
/// one before in this process. `changed` are the workspace-relative paths of this edit (any
/// separator); partners among them, or missing from disk, are not named.
pub(crate) fn note(root: &Path, changed: &[String]) -> String {
    let changed: Vec<String> = changed.iter().map(|path| path.replace('\\', "/")).collect();
    let fresh: Vec<&String> = {
        let shown = lock(&SHOWN);
        changed
            .iter()
            .filter(|path| !shown.contains(&(root.to_path_buf(), (*path).clone())))
            .collect()
    };
    if fresh.is_empty() {
        return String::new();
    }
    let Some(history) = history(root) else {
        return String::new();
    };
    let skip =
        |path: &str| changed.iter().any(|edited| edited == path) || !root.join(path).exists();
    let mut note = String::new();
    for file in fresh {
        let partners = history.partners(file, &skip);
        lock(&SHOWN).insert((root.to_path_buf(), file.clone()));
        if partners.is_empty() {
            continue;
        }
        let named: Vec<String> = partners
            .iter()
            .map(|(path, shared, total)| format!("{path} ({shared} of {total} commits)"))
            .collect();
        note.push_str(&format!(
            "[history hint] {file} usually changes with {}; check whether they need updating too.\n",
            named.join(", ")
        ));
    }
    note
}

#[cfg(test)]
mod tests;
