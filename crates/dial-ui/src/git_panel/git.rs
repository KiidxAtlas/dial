//! Git commands behind the `/git` panel: status, commits, diffs, patch application and blob reads.

use super::*;
use crate::tools::{vcs::git::run_checked, vcs::git_patch::line_patch};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::RecvTimeoutError,
    time::Duration,
};

/// Untracked files above this size are listed but not diffed.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
// ---- git data ----

pub(super) fn resolve(root: &Path, rev: &str) -> Result<String, String> {
    if rev.starts_with('-') {
        return Err(format!("Cannot resolve revision: {rev}"));
    }
    let spec = format!("{rev}^{{commit}}");
    run_checked(
        root,
        &["rev-parse", "--verify", "--quiet", spec.as_str()],
        false,
    )
    .ok()
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| format!("Cannot resolve revision: {rev}"))
}

pub(super) fn load_commit(root: &Path, sha: &str) -> Result<Commit, String> {
    let text = run_checked(
        root,
        &[
            "show",
            "-s",
            "--format=%H%x1f%h%x1f%s%x1f%b%x1f%an%x1f%aI",
            sha,
        ],
        false,
    )?;
    let fields: Vec<&str> = text.trim_end().splitn(6, '\u{1f}').collect();
    if fields.len() < 6 {
        return Err(format!("Cannot read commit {sha}"));
    }
    Ok(Commit {
        sha: fields[0].to_owned(),
        short: fields[1].to_owned(),
        subject: fields[2].to_owned(),
        body: fields[3].trim().to_owned(),
        author: fields[4].to_owned(),
        date: fields[5].to_owned(),
    })
}

pub(super) fn load_commit_files(root: &Path, sha: &str) -> Result<Vec<Change>, String> {
    let text = run_checked(
        root,
        &[
            "show",
            "--no-renames",
            "--name-status",
            "-z",
            "--format=",
            sha,
        ],
        false,
    )?;
    let mut out = Vec::new();
    let mut fields = text.split('\0').map(|field| field.trim_start_matches('\n'));
    while let Some(code) = fields.next() {
        if code.is_empty() {
            continue;
        }
        let Some(path) = fields.next() else {
            break;
        };
        let kind = match code.chars().next() {
            Some('A') => Kind::Added,
            Some('D') => Kind::Deleted,
            _ => Kind::Modified,
        };
        out.push(Change {
            path: path.to_owned(),
            orig: None,
            kind,
            area: Area::Commit,
        });
    }
    Ok(out)
}

/// Working-tree status as staged, unstaged, untracked and conflicted entries.
pub(super) fn load_status(root: &Path) -> Result<Vec<Change>, String> {
    let text = run_checked(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        false,
    )?;
    let mut out = Vec::new();
    let mut fields = text.split('\0');
    while let Some(entry) = fields.next() {
        let bytes = entry.as_bytes();
        if bytes.len() < 4 {
            continue;
        }
        let (x, y) = (bytes[0] as char, bytes[1] as char);
        let Some(path) = entry.get(3..).map(str::to_owned) else {
            continue;
        };
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            fields.next().map(str::to_owned)
        } else {
            None
        };
        if x == '?' && y == '?' {
            out.push(Change {
                path,
                orig: None,
                kind: Kind::Untracked,
                area: Area::Unstaged,
            });
            continue;
        }
        if conflicted(x, y) {
            out.push(Change {
                path,
                orig: None,
                kind: Kind::Conflicted,
                area: Area::Unstaged,
            });
            continue;
        }
        if x != ' ' {
            out.push(Change {
                path: path.clone(),
                orig: orig.clone(),
                kind: kind_of(x),
                area: Area::Staged,
            });
        }
        if y != ' ' {
            out.push(Change {
                path,
                orig,
                kind: kind_of(y),
                area: Area::Unstaged,
            });
        }
    }
    Ok(out)
}

/// Builds a diff for an untracked file as a new-file patch so it can be staged hunk by hunk.
pub(super) fn untracked_doc(root: &Path, path: &str) -> Result<Doc, String> {
    let full = root.join(path);
    let size = std::fs::metadata(&full)
        .map_err(|e| format!("{path}: {e}"))?
        .len();
    let head = vec![format!("diff --git a/{path} b/{path}")];
    if size > MAX_FILE_BYTES {
        let mut doc = Doc {
            head,
            ..Doc::default()
        };
        doc.head
            .push(format!("Untracked file too large to diff ({size} bytes)"));
        return Ok(doc);
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("{path}: {e}"))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        _ => {
            let mut doc = Doc {
                head,
                ..Doc::default()
            };
            doc.head.push("Binary file (untracked)".to_owned());
            return Ok(doc);
        }
    };
    // Split on `\n` only: `str::lines` would strip the `\r` of CRLF files and change the staged bytes.
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let lines: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    let mut patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
    );
    if !lines.is_empty() {
        patch.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
        for line in &lines {
            patch.push('+');
            patch.push_str(line);
            patch.push('\n');
        }
        if !text.ends_with('\n') {
            patch.push_str("\\ No newline at end of file\n");
        }
    }
    Ok(parse_diff(&patch))
}

pub(super) fn apply_patch(
    root: &Path,
    patch: &str,
    cached: bool,
    reverse: bool,
) -> Result<(), String> {
    let mut args = vec!["apply", "--recount"];
    if cached {
        args.push("--cached");
    }
    if reverse {
        args.push("--reverse");
    }
    git_stdin(root, &args, Some(patch)).map(drop)
}

/// Upper bound for one panel git command (commit hooks included).
const GIT_STDIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs git with an optional stdin payload and returns stdout. Failures return git's own message.
pub(super) fn git_stdin(root: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    let (write_error, output) = run_git_bounded(root, args, input)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return match write_error {
            Some(error) => Err(error),
            None => Ok(stdout),
        };
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let message = if stderr.is_empty() {
        stdout.trim().to_owned()
    } else {
        stderr
    };
    Err(if message.is_empty() {
        "git failed".to_owned()
    } else {
        message
    })
}

/// Runs git to completion within `GIT_STDIN_TIMEOUT`, killing its process group on timeout. `input`
/// is written on a worker thread so a timeout can kill git while it is blocked on stdin. The stdin
/// write error is returned with the output: it must not skip reaping the child.
fn run_git_bounded(
    root: &Path,
    args: &[&str],
    input: Option<&str>,
) -> Result<(Option<String>, std::process::Output), String> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        crate::platform::process::spawn_group(&mut command).map_err(|e| format!("git: {e}"))?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let payload = input.map(str::to_owned);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A write error (EPIPE when git exits early) must not skip reaping the child:
        // git's stderr explains the failure and is reported below.
        let write_error = match (stdin, payload) {
            (Some(mut stdin), Some(data)) => stdin
                .write_all(data.as_bytes())
                .err()
                .map(|e| e.to_string()),
            _ => None,
        };
        let output = child.wait_with_output();
        let _ = tx.send((write_error, output));
    });
    let (write_error, output) = match rx.recv_timeout(GIT_STDIN_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            let _ = crate::platform::process::kill_group(pid);
            return Err(format!(
                "git timed out after {}s",
                GIT_STDIN_TIMEOUT.as_secs()
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err("git: worker thread stopped".to_owned());
        }
    };
    Ok((write_error, output.map_err(|e| e.to_string())?))
}
/// Bytes read from the start of a file to sniff its type and dimensions.
const SNIFF_BYTES: u64 = 1 << 20;
/// Largest image the overlay reads and decodes; bigger blobs keep the text placeholder.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Before/after file bytes of a change; `None` for a missing side.
type SidePair = (Option<Vec<u8>>, Option<Vec<u8>>);

fn blob_head(root: &Path, change: &Change, rev: Option<&str>) -> Result<(u64, Vec<u8>), String> {
    if change.area == Area::Unstaged
        && let Ok(file) = std::fs::File::open(root.join(&change.path))
    {
        let size = file
            .metadata()
            .map_err(|error| format!("read {}: {error}", change.path))?
            .len();
        let mut head = Vec::new();
        if size <= MAX_IMAGE_BYTES {
            file.take(SNIFF_BYTES)
                .read_to_end(&mut head)
                .map_err(|error| format!("read {}: {error}", change.path))?;
        }
        return Ok((size, head));
    }
    let mut spec = match rev {
        Some(sha) => format!("{sha}:{}", change.path),
        None => format!(":{}", change.path),
    };
    // Size first, so an oversized blob is never loaded whole. A deleted file exists only in the
    // parent of the viewed commit.
    let size = match blob_size(root, &spec) {
        Ok(size) => size,
        Err(error) => match rev {
            Some(sha) => {
                spec = format!("{sha}^:{}", change.path);
                blob_size(root, &spec)?
            }
            None => return Err(error),
        },
    };
    if size > MAX_IMAGE_BYTES {
        return Ok((size, Vec::new()));
    }
    let bytes = git_bytes(root, &["cat-file", "blob", &spec])?;
    let head = bytes[..bytes.len().min(SNIFF_BYTES as usize)].to_vec();
    Ok((size, head))
}

/// Byte size of a git object, read without loading its contents.
fn blob_size(root: &Path, spec: &str) -> Result<u64, String> {
    let text = run_checked(root, &["cat-file", "-s", spec], false)?;
    text.trim()
        .parse()
        .map_err(|_| format!("Cannot read size of {spec}"))
}

pub(super) fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let (_, output) = run_git_bounded(root, args, None)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// The git object spec of the before side: the index, HEAD, or the parent of the viewed commit.
fn before_spec(area: Area, rev: Option<&str>, path: &str) -> Option<String> {
    match area {
        Area::Unstaged => Some(format!(":{path}")),
        Area::Staged => Some(format!("HEAD:{path}")),
        Area::Commit => Some(format!("{}^:{path}", rev?)),
    }
}

fn side_bytes(root: &Path, spec: String) -> Option<Vec<u8>> {
    let bytes = git_bytes(root, &["cat-file", "blob", &spec]).ok()?;
    (bytes.len() as u64 <= MAX_IMAGE_BYTES).then_some(bytes)
}

fn worktree_bytes(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Text of a whole file diff, as `git diff` printed it, rebuilt from the parsed doc.
fn doc_text(doc: &Doc) -> String {
    let mut out = String::new();
    let lines = doc
        .head
        .iter()
        .map(String::as_str)
        .chain(doc.hunks.iter().flat_map(|hunk| {
            std::iter::once(hunk.header.as_str()).chain(hunk.lines.iter().map(String::as_str))
        }));
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Removes `paths` from the index, keeping the worktree. `restore --staged` needs a HEAD; on an
/// unborn branch the entries are removed from the index instead. Without a HEAD, a failed restore
/// must not fall through to `rm --cached`, which would stage a deletion of a tracked file.
fn unstage_paths(root: &Path, paths: &[&str]) -> Result<(), String> {
    let has_head = run_checked(root, &["rev-parse", "--verify", "--quiet", "HEAD"], false).is_ok();
    if has_head {
        run_checked(
            root,
            &with_paths(&["restore", "--staged", "--"], paths),
            false,
        )
        .map(drop)
    } else {
        run_checked(
            root,
            &with_paths(
                &["rm", "-r", "--cached", "-q", "--ignore-unmatch", "--"],
                paths,
            ),
            false,
        )
        .map(drop)
    }
}

impl Repo {
    pub(crate) fn status(&self) -> Result<Vec<Change>, String> {
        load_status(&self.root)
    }

    pub(crate) fn resolve(&self, rev: &str) -> Result<String, String> {
        resolve(&self.root, rev)
    }

    pub(crate) fn commit_info(&self, sha: &str) -> Result<Commit, String> {
        load_commit(&self.root, sha)
    }

    pub(crate) fn commit_files(&self, sha: &str) -> Result<Vec<Change>, String> {
        load_commit_files(&self.root, sha)
    }

    /// The current branch (None when detached) and the short sha and subject of HEAD.
    pub(crate) fn branch_and_head(&self) -> (Option<String>, Option<(String, String)>) {
        let branch = run_checked(
            &self.root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            false,
        )
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
        let head = run_checked(&self.root, &["log", "-1", "--format=%h%x1f%s"], false)
            .ok()
            .and_then(|text| {
                let (sha, subject) = text.trim_end().split_once('\u{1f}')?;
                Some((sha.to_owned(), subject.to_owned()))
            });
        (branch, head)
    }

    pub(crate) fn untracked_doc(&self, path: &str) -> Result<Doc, String> {
        untracked_doc(&self.root, path)
    }

    /// Runs git with `args` in the project root and parses the diff output.
    pub(crate) fn diff(&self, args: &[&str]) -> Result<Doc, String> {
        run_checked(&self.root, args, false).map(|text| parse_diff(&text))
    }

    pub(crate) fn binary_head(
        &self,
        change: &Change,
        rev: Option<&str>,
    ) -> Result<(u64, Vec<u8>), String> {
        blob_head(&self.root, change, rev)
    }

    /// Raw bytes of both sides of an image change; None when the change has no before side.
    pub(crate) fn image_sides(&self, change: &Change, rev: Option<&str>) -> Option<SidePair> {
        let before_path = change.orig.as_deref().unwrap_or(&change.path);
        let before = side_bytes(&self.root, before_spec(change.area, rev, before_path)?);
        let after = match change.area {
            Area::Unstaged => worktree_bytes(&self.root.join(&change.path)),
            Area::Staged => side_bytes(&self.root, format!(":{}", change.path)),
            Area::Commit => side_bytes(&self.root, format!("{}:{}", rev?, change.path)),
        };
        Some((before, after))
    }

    /// Stages (`cached`) or discards one hunk of `doc`; None when the hunk does not exist.
    pub(crate) fn apply_hunk(
        &self,
        doc: &Doc,
        index: usize,
        cached: bool,
        reverse: bool,
    ) -> Option<Result<(), String>> {
        let patch = hunk_patch(doc, index)?;
        Some(apply_patch(&self.root, &patch, cached, reverse))
    }

    /// Stages or discards the selected body lines of one hunk; None when the selection has no change.
    pub(crate) fn apply_lines(
        &self,
        doc: &Doc,
        hunk: usize,
        picks: &[usize],
        reverse: bool,
        cached: bool,
    ) -> Option<Result<(), String>> {
        let file = doc_text(doc);
        let patch = line_patch(&file, hunk, picks, reverse)?;
        Some(apply_patch(&self.root, &patch, cached, false))
    }

    /// Stages an unstaged change or unstages a staged one.
    pub(crate) fn set_staged(&self, change: &Change) -> Result<(), String> {
        let paths = change_paths(change);
        match change.area {
            Area::Unstaged => {
                run_checked(&self.root, &with_paths(&["add", "--"], &paths), false).map(drop)
            }
            Area::Staged => unstage_paths(&self.root, &paths),
            Area::Commit => Err("The commit view is read-only".to_owned()),
        }
    }

    pub(crate) fn stage_all(&self) -> Result<(), String> {
        run_checked(&self.root, &["add", "-A"], false).map(drop)
    }

    pub(crate) fn unstage_all(&self) -> Result<(), String> {
        unstage_paths(&self.root, &["."])
    }

    /// Discards a change from the worktree (unstaged) or from the index and worktree (staged).
    pub(crate) fn discard(&self, change: &Change) -> Result<(), String> {
        let paths = change_paths(change);
        match (change.area, change.kind) {
            (Area::Commit, _) => Err("The commit view is read-only".to_owned()),
            (_, Kind::Conflicted) => Err("Resolve the conflict before discarding".to_owned()),
            (Area::Unstaged, Kind::Untracked) => std::fs::remove_file(self.root.join(&change.path))
                .map_err(|e| format!("{}: {e}", change.path)),
            (Area::Unstaged, _) => {
                run_checked(&self.root, &with_paths(&["restore", "--"], &paths), false).map(drop)
            }
            (Area::Staged, Kind::Added) => {
                run_checked(&self.root, &with_paths(&["rm", "-f", "--"], &paths), false).map(drop)
            }
            (Area::Staged, _) => run_checked(
                &self.root,
                &with_paths(
                    &["restore", "--source=HEAD", "--staged", "--worktree", "--"],
                    &paths,
                ),
                false,
            )
            .map(drop),
        }
    }

    /// Commits the index with `message`. An empty message amends the previous commit without editing.
    pub(crate) fn commit(&self, message: &str, amend: bool) -> Result<String, String> {
        let bypass = self.config.git.bypass_hooks;
        let mut args: Vec<&str> = Vec::new();
        if bypass {
            args.extend(crate::tools::vcs::git::hooks_off_args());
        }
        args.push("commit");
        if bypass {
            args.push("--no-verify");
        }
        if message.trim().is_empty() {
            args.extend(["--amend", "--no-edit"]);
            return git_stdin(&self.root, &args, None);
        }
        args.extend(["--cleanup=strip", "-F", "-"]);
        if amend {
            args.push("--amend");
        }
        git_stdin(&self.root, &args, Some(message))
    }
}
