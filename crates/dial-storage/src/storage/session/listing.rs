//! Session listing, pickers and summaries: the metadata shown by the home screen and resume picker.

use super::*;

pub(super) const TITLE_CHARS: usize = 80;
pub(super) const PICKER_PREVIEW_CHARS: usize = 1200;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// Not cached: the listing fills it from the journal it read.
    #[serde(skip)]
    pub path: PathBuf,
    pub started: String,
    /// Timestamp of the last accepted journal entry, falling back to `started`.
    pub updated: String,
    pub title: String,
    pub messages: usize,
    pub model: String,
    pub parent_id: Option<String>,
    pub first_message: String,
    /// Latest real user request on the active branch (bounded preview).
    pub last_message: String,
    /// Latest nonempty assistant text on the active branch (bounded preview).
    pub last_answer: String,
    /// Real user requests on the active branch, including compacted history.
    pub user_messages: usize,
    /// Working directory recorded in the header (the project root), or the project directory name.
    pub project: String,
}

impl SessionInfo {
    /// A conversation has at least one user request on its active branch. Empty sessions do not
    /// count for resume, `-c`, or the picker's default view.
    pub fn is_conversation(&self) -> bool {
        self.user_messages > 0
    }
}

/// The most recently active conversation. Callers pass a catalogue in any order; `-c` and the
/// resume picker share this single selection.
pub fn latest_conversation(sessions: &[SessionInfo]) -> Option<&SessionInfo> {
    sessions
        .iter()
        .filter(|s| s.is_conversation())
        .max_by(|a, b| a.updated.cmp(&b.updated).then_with(|| a.id.cmp(&b.id)))
}

/// Session id from `<ts>_<id>.jsonl`.
fn path_session_id(path: &Path) -> &str {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit_once('_'))
        .map_or("", |(_, id)| id)
}

/// Archived sessions keep using the same project artifact store when reopened for appending.
pub(super) fn artifact_root(path: &Path) -> Result<PathBuf> {
    let sessions = path
        .ancestors()
        .skip(1)
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "sessions"))
        .or_else(|| path.parent())
        .context("session has no storage directory")?;
    Ok(sessions
        .parent()
        .context("cannot locate session artifact storage")?
        .join("artifacts"))
}

/// Pinned sessions first; each group keeps its newest-first order.
fn pinned_first(paths: Vec<PathBuf>, pinned: &[String]) -> Vec<PathBuf> {
    if pinned.is_empty() {
        return paths;
    }
    session_pins::sort_pinned_first(paths, pinned, |p| path_session_id(p))
}

/// Ids are only ever used as labels and map keys, never joined into a path; still, anything read from
/// disk that is not plain `[A-Za-z0-9_-]` is replaced.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<ts>_<id>.jsonl` -> `<id>` (None for any other file name).
pub(super) fn file_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    valid_id(id).then_some(id)
}

// ---------------------------------------------------------------- file helpers

/// Session files in a directory, newest first. Symlinks and odd names are ignored.
pub(super) fn session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| file_id(n).is_some())
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names.into_iter().map(|n| dir.join(n)).collect())
}

impl Session {
    /// Pinned sessions first (picker order). For "latest" selections use `list_newest`.
    pub fn list(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        let pinned = session_pins::load(&layout.user_dir)?;
        let paths = pinned_first(session_files(&layout.sessions_dir())?, &pinned);
        Self::list_paths(paths, limit)
    }

    /// Newest first by creation, ignoring pins, so `.first()` is the most recent session.
    pub fn list_newest(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        Self::list_paths(session_files(&layout.sessions_dir())?, limit)
    }

    /// Activity-ordered picker snapshot of this project, pinned first. Reads journals once without
    /// mutating them. `limit` budgets conversations and empty sessions separately (at most `2 * limit`
    /// rows), so recent empty journals cannot displace conversations and callers can toggle empties.
    /// Original active-branch records supply previews; bootstrap/compaction prefixes do not.
    pub fn list_for_picker(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        Self::picker_snapshot(session_files(&layout.sessions_dir())?, layout, limit)
    }

    /// The picker's "all projects" scope: the same snapshot over every project's journals.
    pub fn list_for_picker_all(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        Self::picker_snapshot(Self::all_session_files(layout)?, layout, limit)
    }

    /// Journal count for the picker scope, "M" in "showing N of M". Parses nothing.
    pub fn count_for_picker(layout: &Layout, all: bool) -> Result<usize> {
        Ok(if all {
            Self::all_session_files(layout)?.len()
        } else {
            session_files(&layout.sessions_dir())?.len()
        })
    }

    fn picker_snapshot(
        paths: Vec<PathBuf>,
        layout: &Layout,
        limit: usize,
    ) -> Result<Vec<SessionInfo>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let pinned = session_pins::load(&layout.user_dir)?;
        let mut sessions = Self::list_paths(paths, usize::MAX)?;
        // Stable sorting preserves newest-created order for equal activity timestamps.
        sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
        let mut sessions = session_pins::sort_pinned_first(sessions, &pinned, |s| &s.id);
        let (mut conversations, mut empty) = (0usize, 0usize);
        sessions.retain(|session| {
            let count = if session.is_conversation() {
                &mut conversations
            } else {
                &mut empty
            };
            if *count >= limit {
                return false;
            }
            *count += 1;
            true
        });
        Ok(sessions)
    }

    /// Journals of every project under the user dir, newest file first.
    fn all_session_files(layout: &Layout) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        let projects = crate::config::paths::projects_dir(&layout.user_dir);
        if projects.is_dir() {
            for entry in fs::read_dir(projects)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    paths.extend(session_files(&entry.path().join("sessions"))?);
                }
            }
        }
        paths.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
        Ok(paths)
    }

    /// Sessions across every project, newest file first, pinned first.
    pub fn list_all(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        let pinned = session_pins::load(&layout.user_dir)?;
        Self::list_paths(
            pinned_first(Self::all_session_files(layout)?, &pinned),
            limit,
        )
    }

    /// This project's sessions by activity (last accepted write first), ignoring pins. The first
    /// entry of `latest_conversation` over this list is the session `-c` resumes.
    pub fn list_by_activity(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut sessions = Self::list_paths(session_files(&layout.sessions_dir())?, usize::MAX)?;
        sessions.sort_by(|a, b| b.updated.cmp(&a.updated));
        sessions.truncate(limit);
        Ok(sessions)
    }

    /// Whether another process holds this journal's writer lock. The shared probe is released
    /// immediately; the file is never claimed or changed.
    pub fn open_elsewhere(path: &Path) -> bool {
        let Ok(file) = platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path)
        else {
            return false;
        };
        matches!(
            platform_fs::try_lock_journal(&file, false),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        )
    }

    /// Timestamp of the last accepted journal entry, falling back to the start time.
    pub fn updated_at(&self) -> String {
        self.entries
            .iter()
            .rev()
            .find(|e| !e.ts.is_empty())
            .map_or_else(|| self.started.clone(), |e| e.ts.clone())
    }

    /// Active-branch conversation turns as `(role, text)`, for the picker's preview pane.
    pub fn preview_turns(path: &Path) -> Result<Vec<(String, String)>> {
        let s = Self::read(path)?;
        let mut turns = Vec::new();
        for &i in &s.active.branch {
            let Kind::Message(message) = &s.entries[i].kind else {
                continue;
            };
            if is_conversation_message(message) {
                turns.push((
                    role_of(message).to_owned(),
                    truncate_chars(&content_text(message), PICKER_PREVIEW_CHARS),
                ));
            }
        }
        Ok(turns)
    }

    /// Project root recorded in a journal's header `cwd`, read from the header line only.
    pub fn project_root(path: &Path) -> Option<PathBuf> {
        let file =
            platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path).ok()?;
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        let line = read_bounded_line(&mut reader, &mut buf).ok()??;
        if line.oversized {
            return None;
        }
        let header: Value = serde_json::from_slice(&buf).ok()?;
        (header["type"] == "session")
            .then(|| header["cwd"].as_str().map(PathBuf::from))
            .flatten()
    }

    /// Notice for a listing that shows `shown` of `total` sessions; `None` when nothing is hidden.
    pub fn listing_notice(shown: usize, total: usize) -> Option<String> {
        (shown < total)
            .then(|| format!("Showing {shown} of {total} sessions; use --all for the full list"))
    }

    /// Sessions whose id starts with `prefix`, newest first, pinned first. Only journals whose file
    /// name carries a matching id are parsed, so an id lookup does not load the whole history.
    pub fn find_prefix(layout: &Layout, prefix: &str) -> Result<Vec<SessionInfo>> {
        let candidates: Vec<PathBuf> = session_files(&layout.sessions_dir())?
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(file_id)
                    .is_some_and(|id| id.starts_with(prefix))
            })
            .collect();
        let pinned = session_pins::load(&layout.user_dir)?;
        let mut found = Self::list_paths(pinned_first(candidates, &pinned), usize::MAX)?;
        found.retain(|s| s.id.starts_with(prefix));
        Ok(found)
    }

    /// Summaries of `paths` in order, up to `limit`; unreadable journals are skipped. Unchanged
    /// journals come from the listing index instead of being parsed again.
    fn list_paths(paths: Vec<PathBuf>, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut cache = index::Cache::default();
        let mut out = Vec::new();
        for path in paths {
            if out.len() >= limit {
                break;
            }
            // Stamped before reading, so a write racing the parse leaves the entry stale.
            let stamp = index::Stamp::of(&path);
            if let Some(info) = stamp.and_then(|stamp| cache.get(&path, stamp)) {
                out.push(info);
                continue;
            }
            let Ok(info) = Self::summarize(path.clone()) else {
                continue;
            };
            if let Some(stamp) = stamp {
                cache.put(&path, stamp, &info);
            }
            out.push(info);
        }
        cache.save();
        Ok(out)
    }

    /// Parse one journal into its listing summary.
    fn summarize(path: PathBuf) -> Result<SessionInfo> {
        let s = Self::load(&path)?;
        let first_message = s.first_message();
        let updated = s.updated_at();
        let (mut last_message, mut last_answer, mut user_messages) =
            (String::new(), String::new(), 0usize);
        for &i in &s.active.branch {
            let Kind::Message(message) = &s.entries[i].kind else {
                continue;
            };
            if !is_conversation_message(message) {
                continue;
            }
            if role_of(message) == "user" {
                user_messages += 1;
                last_message = truncate_chars(&content_text(message), PICKER_PREVIEW_CHARS);
            } else {
                let original = message
                    .get("dial_child_original_response")
                    .filter(|original| role_of(original) == "assistant")
                    .unwrap_or(message);
                let text = content_text(original);
                if !text.trim().is_empty() {
                    last_answer = truncate_chars(&text, PICKER_PREVIEW_CHARS);
                }
            }
        }
        // The header's working directory names the project for display; the directory name is
        // the fallback for headers that lack one.
        let project = Self::project_root(&path).map_or_else(
            || {
                path.parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            |root| root.display().to_string(),
        );
        Ok(SessionInfo {
            title: s.title(),
            messages: s.message_count(),
            first_message,
            updated,
            last_message,
            last_answer,
            user_messages,
            project,
            id: s.id,
            path,
            started: s.started,
            model: s.model,
            parent_id: s.parent_id,
        })
    }

    pub(super) fn first_message(&self) -> String {
        self.entries
            .iter()
            .find_map(|e| match &e.kind {
                Kind::Message(m) if role_of(m) == "user" => Some(content_text(m)),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn title(&self) -> String {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Kind::Title(title) => Some(title.clone()),
                _ => None,
            })
            .unwrap_or_else(|| {
                truncate_chars(
                    &self
                        .first_message()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    TITLE_CHARS,
                )
            })
    }
}
