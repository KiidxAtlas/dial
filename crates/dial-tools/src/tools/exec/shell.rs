//! The `bash` tool: platform-native shell execution, process ownership, live output and artifacts.

use super::sandbox::inherit_allowed_env;
#[cfg(not(windows))]
use super::sandbox::{Policy, Sandbox};
use crate::config::types::ShellBackend;
use crate::tools::{ToolOutput, Workspace, opt_bool, opt_u64, req_str};
use crate::workspace_copy::SCRATCH_DIR;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
pub mod session;
use session::{SessionClaim, admit, read_published};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 300;

/// The command's working directory: the primary root by default, or `cwd` / a leading simple
/// `cd path && ...` resolved through [`Workspace::resolve`], so explicitly added roots work and
/// symlink escapes, protected Dial storage and undeclared directories stay refused.
fn working_directory(
    ws: &Workspace,
    args: &Value,
    command: &str,
) -> Result<(std::path::PathBuf, String), String> {
    let mut cwd = ws.root().to_path_buf();
    if let Some(value) = args.get("cwd") {
        let value = value.as_str().ok_or("cwd must be a string")?;
        cwd = ws.resolve(value)?;
        if !cwd.is_dir() {
            return Err("cwd must be an existing directory inside a workspace root".into());
        }
    }
    #[cfg(not(windows))]
    {
        // Retain the bash convenience of accepting an initial `cd path && ...`; PowerShell
        // commands stay untouched and use the explicit `cwd` argument/current directory.
        let trimmed = command.trim_start();
        if let Some(rest) = trimmed.strip_prefix("cd ")
            && let Some((target, script)) = rest.split_once(" && ")
        {
            let target = target.trim();
            if !target.is_empty() && !target.contains(['\'', '"', '`', '$', ';', '|']) {
                // Relative targets are relative to the effective cwd, not the primary root.
                let candidate = cwd.join(target);
                if let Some(path) = candidate
                    .to_str()
                    .and_then(|candidate| ws.resolve(candidate).ok())
                    && path.is_dir()
                {
                    return Ok((path, script.to_owned()));
                }
            }
        }
    }
    Ok((cwd, command.to_owned()))
}

/// Refuses a `sqlite3` CLI command that would open a database read-write while its `-wal`
/// holds data SQLite would discard (an invalid header or checksum chain, or an uncommitted
/// tail). Such an open checkpoints, truncates or deletes the WAL, destroying the only copy of
/// those bytes, so the refusal comes before the command runs. `-readonly`, `mode=ro` and
/// `immutable=1` opens, databases whose WAL SQLite would apply in full, and opens after an
/// earlier step of the same command names the WAL (replacing or backing it up first) are left
/// alone.
fn sqlite_wal_guard(ws: &Workspace, args: &Value, command: &str) -> Option<String> {
    use std::io::Read as _;
    if !command.contains("sqlite3") {
        return None;
    }
    let (cwd, script) = working_directory(ws, args, command).ok()?;
    let heads = crate::tools::exec::bash_interceptor::pipeline_heads(&script);
    for (index, (head, _)) in heads.iter().enumerate() {
        let words: Vec<&str> = head
            .split_whitespace()
            .map(|word| word.trim_matches(['\'', '"']))
            .collect();
        let Some(at) = words.iter().position(|word| !word.contains('=')) else {
            continue;
        };
        if words[at] != "sqlite3" && !words[at].ends_with("/sqlite3") {
            continue;
        }
        let operands = &words[at + 1..];
        let read_only = |word: &&str| {
            matches!(*word, "-readonly" | "--readonly")
                || word.contains("mode=ro")
                || word.contains("immutable=1")
        };
        if operands.iter().any(read_only) {
            continue;
        }
        for operand in operands.iter().filter(|word| !word.starts_with('-')) {
            let operand = operand.strip_prefix("file:").unwrap_or(operand);
            let database = operand.split('?').next().unwrap_or(operand);
            if database.is_empty() {
                continue;
            }
            let wal_name = format!("{database}-wal");
            let handled_earlier = heads[..index].iter().any(|(earlier, _)| {
                earlier.split_whitespace().any(|word| {
                    word.trim_matches(['\'', '"'])
                        .trim_end_matches('/')
                        .ends_with(&wal_name)
                })
            });
            if handled_earlier {
                continue;
            }
            let Some(wal_path) = cwd
                .join(&wal_name)
                .to_str()
                .and_then(|path| ws.resolve_read(path).ok())
            else {
                continue;
            };
            let mut bytes = Vec::new();
            let Ok(file) = std::fs::File::open(&wal_path) else {
                continue;
            };
            if file.take(64 * 1024 * 1024).read_to_end(&mut bytes).is_err()
                || !crate::tools::fs::binary::wal_discards_data(&bytes)
            {
                continue;
            }
            let report = crate::tools::fs::binary::wal(&bytes);
            let summary = report.lines().next().unwrap_or_default();
            let summary = summary.split(". ").next().unwrap_or(summary);
            return Some(format!(
                "Blocked before running: `{wal_name}` ({} bytes) holds data SQLite would discard ({summary}). This read-write `sqlite3` open would checkpoint, truncate or delete it, and the bytes could not be recovered afterwards. Inspect without changing anything: `read {database}` (schema, rows and WAL status) or `read {wal_name}:bytes=0+64`; query with `sqlite3 -readonly` or a `file:{database}?mode=ro` URI. Repair the WAL before a writable open, or move it aside first if it should really be discarded.",
                bytes.len()
            ));
        }
    }
    None
}
const MAX_COMMAND: usize = 64 * 1024;
// Keep the model-facing preview small; complete output is spooled independently below.
const BASH_HEAD: usize = 4 * 1024;
const BASH_TAIL: usize = 4 * 1024;
/// Complete outputs up to this size are re-read from their artifact so the output filter can
/// apply even when the inline view had to be cut to head and tail.
const FULL_FILTER_MAX: u64 = 4 * 1024 * 1024;
/// Diagnostic lines kept from the omitted middle (a compiler's first error is often there).
const BASH_DIAGNOSTICS: usize = 3 * 1024;
/// Farthest a cut moves to reach a line boundary; a longer line is cut by bytes instead.
const BASH_LINE_SLACK: usize = 512;
#[cfg(not(windows))]
const CLT_DIR: &str = "/Library/Developer/CommandLineTools";

/// Adoption hint when `bg` is offered: the job's result comes from one `bg wait`.
pub const BG_OFFERED_HINT: &str =
    " Do independent work first. When blocked, `bg wait` returns its output; no polling.";
/// Adoption hint when `bg` is loadable but not offered. The agent replaces it with
/// [`BG_OFFERED_HINT`] when it loads `bg` for the next request itself.
pub const BG_LOAD_HINT: &str = " Load the Dial `bg` tool with `tool_search` names:[\"bg\"] to wait for or stop it; no polling.";

/// Helper languages the session prompt reports, each with the program names that provide it.
const SHELL_INTERPRETERS: &[(&str, &[&str])] = &[
    ("python3", &["python3"]),
    ("node", &["node", "bun", "deno"]),
    ("perl", &["perl"]),
    ("ruby", &["ruby"]),
    ("C compiler", &["cc", "gcc", "clang"]),
    ("go", &["go"]),
    ("rustc", &["rustc"]),
    ("java", &["java"]),
];

/// Which helper languages the shell's `PATH` provides, for the session prompt. Models otherwise
/// write a whole Python or Node helper before discovering the image has neither; the bash
/// missing-program notice arrives only after that output has been generated and paid for.
pub fn interpreter_guidance(ws: &Workspace) -> String {
    let path = std::env::var_os("PATH");
    #[cfg(not(windows))]
    let path = if !ws.unsandboxed && matches!(ws.policy, None | Some(Policy::Strict { .. })) {
        Some(bash_path_for(ws).into())
    } else {
        path
    };
    interpreter_guidance_for(ws, path.as_deref())
}

fn interpreter_guidance_for(ws: &Workspace, path: Option<&std::ffi::OsStr>) -> String {
    let dirs: Vec<PathBuf> = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|dir| ws.root().join(dir))
        .collect();
    let found = |program: &str| {
        dirs.iter().any(|dir| {
            crate::platform::fs::executable_candidate(&dir.join(program))
                .is_some_and(|p| !ws.sandbox_hides(&p))
        })
    };
    let (mut available, mut missing) = (Vec::new(), Vec::new());
    for (language, programs) in SHELL_INTERPRETERS {
        match programs.iter().copied().find(|program| found(program)) {
            Some(program) if program == *language => available.push((*language).to_owned()),
            Some(program) => available.push(format!("{language} ({program})")),
            None if *language == "python3" && bundled_path::offered(ws, path) => {
                available.push(format!(
                    "python3 (Dial's bundled CPython {}, standard library only; after every PATH entry)",
                    crate::setup::deps::bundled_python::version().unwrap_or("3")
                ));
            }
            None => missing.push(*language),
        }
    }
    // A bare `python` is not evidence of Python 3 (it may be Python 2 or a different interpreter),
    // so it is advertised as an unverified launcher and never as `python3`.
    let bare_python = !found("python3") && found("python");
    if bare_python {
        available.push("python (unverified version; no python3 launcher)".to_owned());
    }
    let validation = if missing.contains(&"python3") {
        " No python3 launcher resolves on the shell PATH. For Python-specific behavior, use Python Eval if offered; a different engine is only partial evidence."
    } else {
        " Use the resolved command for helpers; lookup does not test runtime compatibility."
    };
    let available = if available.is_empty() {
        "none".to_owned()
    } else {
        available.join(", ")
    };
    let missing = if missing.is_empty() {
        String::new()
    } else {
        format!("; not found: {}", missing.join(", "))
    };
    format!("- Shell PATH languages: {available}{missing}.{validation}\n")
}

/// Entries under `dir` keyed by relative path, or `None` when it is unreadable or too large to
/// compare. Nested directories are walked a few levels deep so a command that leaves a build
/// product beside a deliverable (`gcc main.c -o out/cmain`) is noticed, not only top-level files.
struct DirectoryEntries {
    /// Levels walked; the later snapshot uses the same depth so the two compare like for like.
    depth: usize,
    entries: std::collections::BTreeMap<std::path::PathBuf, Option<u64>>,
}

/// Levels below the working directory whose new entries are reported.
const ENTRY_DEPTH: usize = 3;
/// Total entries compared before the delta is abandoned as too large to be useful.
const ENTRY_LIMIT: usize = 4096;

fn directory_entries(dir: &Path) -> Option<DirectoryEntries> {
    // A large tree keeps the top-level comparison rather than losing the note entirely.
    entries_to_depth(dir, ENTRY_DEPTH).or_else(|| entries_to_depth(dir, 1))
}

fn entries_to_depth(dir: &Path, max_depth: usize) -> Option<DirectoryEntries> {
    let mut names = std::collections::BTreeMap::new();
    let mut pending = vec![(std::path::PathBuf::new(), 1_usize)];
    while let Some((relative, depth)) = pending.pop() {
        let read = fs::read_dir(dir.join(&relative));
        // Only the working directory itself must be readable; an unreadable subdirectory is
        // still listed as an entry, just not descended into.
        let entries = match read {
            Ok(entries) => entries,
            Err(_) if depth > 1 => continue,
            Err(_) => return None,
        };
        for entry in entries {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if depth == 1 && name == crate::workspace_copy::SCRATCH_DIR {
                continue;
            }
            let kind = entry.file_type().ok();
            let size = kind
                .filter(|kind| kind.is_file())
                .and_then(|_| entry.metadata().ok())
                .map(|meta| meta.len());
            let path = relative.join(&name);
            if depth < max_depth
                && kind.is_some_and(|kind| kind.is_dir())
                && !UNWALKED_DIRS.contains(&name.to_string_lossy().as_ref())
            {
                pending.push((path.clone(), depth + 1));
            }
            names.insert(path, size);
            if names.len() > ENTRY_LIMIT {
                return None;
            }
        }
    }
    Some(DirectoryEntries {
        depth: max_depth,
        entries: names,
    })
}

/// Dependency, VCS and cache trees: their creation is reported, their contents are not walked.
const UNWALKED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
];

/// Bounded directory deltas are observations, never proof that removal was unintended.
fn created_entries_note(before: Option<&DirectoryEntries>, dir: &Path) -> Option<String> {
    let (before, after) = (
        &before?.entries,
        entries_to_depth(dir, before?.depth)?.entries,
    );
    // Name a new directory once rather than every file inside it.
    let created: Vec<_> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .filter(|path| {
            path.parent()
                .is_none_or(|parent| parent.as_os_str().is_empty() || before.contains_key(parent))
        })
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect();
    // Removal and shrinkage are reported for the working directory's own entries only: nested
    // deletions are usually deliberate cleanup, and this note warns about lost task inputs.
    let lost: Vec<_> = before
        .iter()
        .filter(|(path, _)| path.components().count() == 1)
        .filter_map(|(name, size)| match after.get(name) {
            None => Some(format!("{} disappeared", name.to_string_lossy())),
            Some(Some(now)) if size.is_some_and(|old| *now < old) => Some(format!(
                "{} shrank from {} to {now} bytes",
                name.to_string_lossy(),
                size.unwrap_or(0)
            )),
            _ => None,
        })
        .collect();
    let mut note = String::new();
    if !created.is_empty() {
        let shown = created
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = created.len().saturating_sub(10);
        let suffix = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        note.push_str(&format!("[new in the working directory: {shown}{suffix}; remove scratch outputs the task does not want]\n"));
    }
    if !lost.is_empty() {
        // A copy exists only when input preservation retained one; that path adds its own
        // `input_evidence_changed` notice naming the artifacts. Never imply a copy otherwise.
        note.push_str(&format!("[working-directory inputs changed: {}. Dial kept a copy only if an input_evidence_changed notice names its artifact; then restore exact bytes in eval with dial.writeBytes(path, dial.readBytes(artifact)). Otherwise the original bytes are gone: do not search for copies or invent lost data.]\n", lost.iter().take(10).cloned().collect::<Vec<_>>().join("; ")));
    }
    (!note.is_empty()).then_some(note)
}

/// Transfers the process leader out of the foreground owner's drop guard.
pub(crate) fn release_process(group: ProcessGroup) -> Child {
    group.release()
}
/// A foreground process and its output stream, transferred intact to the managed background owner.
#[doc(hidden)]
pub struct ForegroundJob {
    pub(crate) workspace: Workspace,
    pub(crate) command: String,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) child: Child,
    pub(crate) reader: io::PipeReader,
    /// Output already read before adoption, so the adopted job keeps it.
    pub(crate) captured: Vec<u8>,
    pub(crate) captured_total: u64,
    /// Complete-output artifact already being written before adoption.
    pub(crate) spool: Option<crate::storage::artifact::ArtifactSpool>,
    /// Named session this command runs in; the claim keeps it busy until an adopted job settles.
    pub(crate) session: Option<SessionClaim>,
    /// The caller asked for the background (`background: true`), rather than a threshold or
    /// steering moving a command it expected to finish in the foreground.
    pub(crate) requested: bool,
}

/// The last lines of output captured before adoption, bounded so the adoption notice stays small.
fn output_tail(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 2000;
    const MAX_LINES: usize = 20;
    let text = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(MAX_BYTES)..]);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(MAX_LINES)..].join("\n")
}

/// Advice for a command sent to `bg`: when it sends its stdout to a file, `bg wait until=` only
/// sees what the job itself prints, so a pattern echoed into that file never matches; and a fixed
/// `sleep N` beside forked work delays the job's own exit and output by N seconds. Both make a
/// wait last far longer than the work. A hint only: heredoc bodies and quoted text are skipped.
pub(crate) fn background_output_note(command: &str) -> Option<String> {
    let script = command.split("<<").next().unwrap_or(command);
    let mut unquoted = String::with_capacity(script.len());
    let mut quote = None;
    for ch in script.chars() {
        match (quote, ch) {
            (None, '\'' | '"') => quote = Some(ch),
            (Some(open), _) if ch == open => quote = None,
            (None, _) => unquoted.push(ch),
            _ => {}
        }
    }
    let bytes = unquoted.as_bytes();
    let mut files = Vec::new();
    let mut at = 0;
    while let Some(offset) = unquoted[at..].find('>') {
        let index = at + offset;
        at = index + 1;
        let before = index.checked_sub(1).map(|i| bytes[i]);
        // `2>`, `>&2`, `&>` stays on the job's output only for stdout; `>>` is handled once.
        if matches!(before, Some(b'2' | b'>' | b'<')) {
            continue;
        }
        let rest = unquoted[at..].trim_start_matches('>');
        if rest.starts_with('&') || rest.starts_with('|') {
            continue;
        }
        let target = rest.split_whitespace().next().unwrap_or("");
        let target = target.trim_end_matches([';', ')', '&']);
        if target.is_empty() || target == "/dev/null" || files.iter().any(|f| f == target) {
            continue;
        }
        files.push(target.to_owned());
    }
    let mut notes = Vec::new();
    if !files.is_empty() {
        let shown: Vec<String> = files.iter().take(3).map(|f| format!("`{f}`")).collect();
        notes.push(format!(
            "It writes output to {}; `bg wait until=` matches only what the job itself prints, never that file's contents, so wait for its exit (no `until`) or read the file.",
            shown.join(", ")
        ));
    }
    let forks = unquoted.replace("&&", "").contains('&');
    if forks {
        let words: Vec<&str> = unquoted
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
            .filter(|w| !w.is_empty())
            .collect();
        if let Some(seconds) = words
            .windows(2)
            .filter(|pair| pair[0] == "sleep")
            .filter_map(|pair| pair[1].trim_end_matches('s').parse::<u64>().ok())
            .find(|seconds| *seconds >= 10)
        {
            notes.push(format!(
                "Its fixed `sleep {seconds}` delays the job's own exit and output by {seconds}s even if the forked work ends sooner; `bg wait` already blocks without polling."
            ));
        }
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}
/// The moment a foreground command is handed to the background owner, or `None` when it may not be.
/// A zero threshold disables automatic backgrounding, so it gets no deadline. Otherwise the deadline
/// is the configured threshold, or the command's explicit `timeout` when that is longer: a caller
/// that said how long to wait gets the result in this call instead of a `bg wait` round trip, and
/// a command still running at that point moves to `bg` rather than being killed. Steering and an
/// explicit background request still adopt at once.
fn adoption_deadline(auto_background: Duration, requested: Option<Duration>) -> Option<Duration> {
    (!auto_background.is_zero())
        .then(|| requested.map_or(auto_background, |timeout| timeout.max(auto_background)))
}

/// Whether the timeout kills the command on this pass. A timeout at or after the adoption deadline
/// never kills, because the command is handed to `bg` at the deadline first. Only a timeout shorter
/// than the threshold kills, or any timeout when backgrounding is disabled.
fn timeout_kills(timeout: Duration, elapsed: Duration, adopt_after: Option<Duration>) -> bool {
    !timeout.is_zero() && elapsed >= timeout && adopt_after.is_none_or(|adopt| adopt > timeout)
}

/// Whether this pass should adopt: an explicit background request, steering, or the deadline.
fn adoption_due(
    asynchronous: bool,
    steering: bool,
    elapsed: Duration,
    deadline: Option<Duration>,
) -> bool {
    asynchronous || steering || deadline.is_some_and(|deadline| elapsed >= deadline)
}

/// Silence before a foreground command is checked for a reader stuck on a daemon's pipe, and the
/// interval between checks; each check reads the process table. The silence is long enough that a
/// reader deliberately watching a detached server's output (`setsid server | grep -m1 Ready`)
/// usually sees it first; a daemon's start-up command prints within seconds or not at all.
const PIPE_STALL_IDLE: Duration = Duration::from_secs(20);
const PIPE_STALL_EVERY: Duration = Duration::from_secs(5);

/// Stops a reader that only a detached process keeps waiting. Unix only; other platforms never
/// report one.
fn stop_reader(pid: u32) -> bool {
    #[cfg(unix)]
    {
        crate::platform::process::signal_pid(pid, 15).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn pipe_stall_note(stall: &crate::platform::pipe_stall::PipeStall, elapsed: Duration) -> String {
    let writers = stall
        .writers
        .iter()
        .map(|(pid, name)| format!("`{name}` (pid {pid})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[after {}s Dial stopped `{}` (pid {}): it was waiting for the end of a pipe that only {writers} keeps open, a process that left this command's process group and so outlives it. That process keeps running; the rest of the command went on. Output the stopped reader would have printed is lost: send a daemon's start-up output to a file, not through a pipe.]\n",
        elapsed.as_secs(),
        stall.reader_name,
        stall.reader,
    )
}

/// Takes over a foreground command that should keep running in the background.
pub trait Adopter: Sync {
    /// Whether a hand-off would currently be accepted. The command is handed over only when this
    /// holds, because a refused hand-off drops the job and so stops the command.
    fn available(&self) -> bool {
        true
    }
    fn adopt(&self, job: ForegroundJob) -> Result<String, String>;
}

impl<F: Fn(ForegroundJob) -> Result<String, String> + Sync> Adopter for F {
    fn adopt(&self, job: ForegroundJob) -> Result<String, String> {
        self(job)
    }
}

/// Hard cancellation outranks adoption on the same pass: a cancelled command is never transferred.
fn may_adopt(cancelled: bool, due: bool) -> bool {
    !cancelled && due
}

/// The parts of a [`ForegroundJob`] that an adopter takes over; the job itself is not dropped.
pub(crate) struct ForegroundParts {
    pub(crate) workspace: Workspace,
    pub(crate) command: String,
    pub(crate) child: Child,
    pub(crate) reader: io::PipeReader,
    pub(crate) captured: Vec<u8>,
    pub(crate) captured_total: u64,
    pub(crate) spool: Option<crate::storage::artifact::ArtifactSpool>,
    pub(crate) session: Option<SessionClaim>,
    pub(crate) requested: bool,
}

impl ForegroundJob {
    pub(crate) fn into_parts(self) -> ForegroundParts {
        let job = std::mem::ManuallyDrop::new(self);
        // SAFETY: ManuallyDrop suppresses ForegroundJob::drop and all field drops. cwd is
        // read exactly once and dropped here; the remaining fields are moved below, never cwd.
        drop(unsafe { std::ptr::read(&job.cwd) });
        // SAFETY: job is an initialized, aligned ForegroundJob that will never be dropped or
        // accessed again. Each non-Copy field below is read once into its sole new owner;
        // no fallible calls intervene, and the child's kill/reap obligation transfers with it.
        unsafe {
            ForegroundParts {
                workspace: std::ptr::read(&job.workspace),
                command: std::ptr::read(&job.command),
                child: std::ptr::read(&job.child),
                reader: std::ptr::read(&job.reader),
                captured: std::ptr::read(&job.captured),
                captured_total: job.captured_total,
                spool: std::ptr::read(&job.spool),
                session: std::ptr::read(&job.session),
                requested: job.requested,
            }
        }
    }
}

impl Drop for ForegroundJob {
    fn drop(&mut self) {
        let _ = crate::platform::process::kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

// Compatibility path for existing process-owning adapters.
pub use crate::runtime::process::ProcessGroup;

/// Keeps the first and last bytes of a stream, counting what was dropped, plus the diagnostic lines
/// (errors, panics, failed tests) found in the dropped middle.
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    newlines: usize,
    diagnostics: crate::tools::code::diagnostics::Collector,
}

impl Capture {
    fn new() -> Self {
        Capture {
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            newlines: 0,
            diagnostics: crate::tools::code::diagnostics::Collector::new(BASH_DIAGNOSTICS),
        }
    }

    /// The head and tail bytes kept so far, in order; the dropped middle is not retained.
    fn retained(&self) -> Vec<u8> {
        let mut bytes = self.head.clone();
        bytes.extend_from_slice(&self.tail);
        bytes
    }

    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        self.newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        if self.head.len() < BASH_HEAD {
            let take = (BASH_HEAD - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > 2 * BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
    }

    fn drop_front(&mut self, excess: usize) {
        self.diagnostics.feed(&self.tail[..excess]);
        self.tail.drain(..excess);
    }

    fn finish(mut self) -> String {
        if self.tail.len() > BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
        let mut dropped = self.total - self.head.len() - self.tail.len();
        if dropped == 0 {
            // Head and tail are contiguous: decode them together so a character that straddles
            // the head cut is not split into replacement characters.
            let mut all = std::mem::take(&mut self.head);
            all.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        // Move both cuts to nearby line boundaries, so the view shows whole lines and the
        // omission can be named as a line range of the complete output.
        let slack = self.tail.len().min(BASH_LINE_SLACK);
        let tail_at_line = match self.tail[..slack].iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                self.drop_front(newline + 1);
                dropped += newline + 1;
                true
            }
            None => false,
        };
        self.diagnostics.finish();
        let floor = self.head.len().saturating_sub(BASH_LINE_SLACK);
        if let Some(newline) = self.head[floor..].iter().rposition(|byte| *byte == b'\n') {
            let keep = floor + newline + 1;
            // The collector never saw the head, so a diagnostic line that starts there stays.
            let fragment = String::from_utf8_lossy(&self.head[keep..]);
            if !crate::tools::code::diagnostics::is_diagnostic(&fragment) {
                dropped += self.head.len() - keep;
                self.head.truncate(keep);
            }
        }
        let count = |bytes: &[u8]| bytes.iter().filter(|byte| **byte == b'\n').count();
        let first_line = count(&self.head) + 1;
        // A tail that starts inside a line leaves that line partly omitted.
        let last_line = self.newlines - count(&self.tail) + usize::from(!tail_at_line);
        let range = if last_line > first_line {
            format!(" (lines {first_line}-{last_line})")
        } else {
            format!(" (line {first_line})")
        };
        // Around an omission, trim each side back to whole characters and count the rest as omitted.
        let head_end = complete_utf8_prefix(&self.head);
        let tail_start = self
            .tail
            .iter()
            .take(3)
            .take_while(|byte| (**byte & 0xC0) == 0x80)
            .count();
        dropped += (self.head.len() - head_end) + tail_start;
        self.head.truncate(head_end);
        self.tail.drain(..tail_start);
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let (kept, lines, overflow) = self.diagnostics.kept();
        if dropped > 0 && lines > 0 {
            let more = if overflow > 0 {
                format!(" ({overflow} more not shown)")
            } else {
                String::new()
            };
            text.push_str(&format!("[... {dropped} bytes of output omitted{range}; {lines} diagnostic line(s) from that part follow{more} ...]\n{kept}[... end of omitted-part diagnostics ...]\n"));
        } else if dropped > 0 {
            text.push_str(&format!(
                "[... {dropped} bytes of output omitted{range} ...]\n"
            ));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        text
    }
}

/// Length of `bytes` without an incomplete UTF-8 sequence at its end.
pub(in crate::tools::exec) fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => bytes.len(),
    }
}

/// Read what is available without blocking. Returns true at EOF.
fn drain_pipe(
    reader: &mut (impl Read + crate::platform::io::Pipe),
    capture: &mut Capture,
    spool: &mut Option<crate::storage::artifact::ArtifactSpool>,
    artifact_error: &mut Option<String>,
    on_output: &(dyn Fn(&[u8]) + Sync),
) -> io::Result<bool> {
    let mut buffer = [0u8; 16 * 1024];
    // Bounded work per pass so a noisy child cannot starve the deadline check.
    for _ in 0..16 {
        match crate::platform::io::read_nonblocking(reader, &mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if let Some(writer) = spool
                    && let Err(error) = writer.write_all(&buffer[..count])
                {
                    *artifact_error = Some(format!("capturing complete output: {error}"));
                    *spool = None;
                }
                capture.push(&buffer[..count]);
                on_output(&buffer[..count]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// After the process (group) is dead, collects what it wrote. A process that escaped may still hold
/// the pipe, so the wait is bounded by a 300ms grace; missing EOF is reported as a capture error.
#[allow(clippy::too_many_arguments)]
fn drain_after_exit<R: Read + crate::platform::io::Pipe>(
    reader: &mut R,
    capture: &mut Capture,
    spool: &mut Option<crate::storage::artifact::ArtifactSpool>,
    artifact_error: &mut Option<String>,
    capture_error: &mut Option<String>,
    eof: &mut bool,
    waiter: &crate::platform::wake::Waiter,
    on_output: &(dyn Fn(&[u8]) + Sync),
    grace_limit: Duration,
) -> Result<(), String> {
    let grace = Instant::now();
    // Probe once more after a wait, even if scheduling used up the grace. Readiness (including EOF)
    // may already be available; the deadline bounds additional waiting, not this nonblocking probe.
    while !*eof && capture_error.is_none() {
        match drain_pipe(reader, capture, spool, artifact_error, on_output) {
            Ok(closed) => *eof = closed,
            Err(error) => {
                *capture_error = Some(format!("reading command output: {error}"));
                break;
            }
        }
        if !*eof {
            if grace.elapsed() >= grace_limit {
                break;
            }
            waiter
                .wait(
                    Some(&*reader),
                    None::<&Child>,
                    Some(grace_limit.saturating_sub(grace.elapsed())),
                )
                .map_err(|e| format!("waiting for command output: {e}"))?;
        }
    }
    if !*eof && capture_error.is_none() {
        *capture_error = Some(format!(
            "pipe did not reach EOF within the {}ms cleanup grace; output may be incomplete",
            grace_limit.as_millis(),
        ));
    }
    Ok(())
}

/// The captured stream as result text: `(no output)` when empty, always newline-terminated.
fn captured_text(capture: Capture) -> String {
    let mut text = capture.finish();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn output_integrity(
    original: u64,
    inline: u64,
    elided: Option<u64>,
    artifact: Option<String>,
    complete: bool,
    minimized: bool,
) -> crate::runtime::execution::OutputIntegrity {
    use crate::runtime::execution::{Completeness, OutputIntegrity};
    let retained = if artifact.is_some() {
        original.saturating_sub(elided.unwrap_or(0))
    } else {
        inline
    };
    OutputIntegrity {
        capture: if complete {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        retention: if retained == original {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        presentation: if original <= inline && !minimized {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        original_bytes: Some(original),
        retained_bytes: Some(retained),
        omitted_bytes: Some(original.saturating_sub(retained)),
        presented_bytes: (!minimized).then_some(inline),
        artifact,
    }
}

/// Finishes the artifact spool and appends the `[complete|partial output: uri]` block plus any
/// capture/artifact error lines. A failed spool finish becomes the artifact error.
fn append_output_notes(
    text: &mut String,
    spool: Option<crate::storage::artifact::ArtifactSpool>,
    capture_error: &Option<String>,
    artifact_error: &mut Option<String>,
) -> Option<String> {
    let mut artifact = None;
    if let Some(spool) = spool {
        let elided = spool.elided_bytes();
        match spool.finish() {
            Ok(uri) => {
                let completeness = if capture_error.is_some() || elided > 0 {
                    "partial"
                } else {
                    "complete"
                };
                text.push_str(&format!("[{completeness} output: {uri}]\n"));
                artifact = Some(uri);
            }
            Err(error) => {
                *artifact_error = Some(format!("saving complete command output artifact: {error}"));
            }
        }
    }
    if let Some(error) = capture_error {
        text.push_str(&format!("[output capture error: {error}]\n"));
    }
    if let Some(error) = artifact_error {
        text.push_str(&format!(
            "[output artifact error: {error}; inline output retained]\n"
        ));
    }
    artifact
}

/// Replaces the first `body_len` bytes of `text` (the captured output) with their minimized form
/// when `shell.minimize_output` is on and the complete output is saved as an artifact, which the
/// `[complete output: ...]` line that follows the note names. Without that line the raw output
/// would be unrecoverable, so the text is left alone.
fn minimize_body(
    ws: &Workspace,
    command: &str,
    body_len: usize,
    text: &mut String,
    capture_complete: bool,
    full: Option<&str>,
) -> bool {
    if !capture_complete
        || !ws.minimizing_output()
        || !text[body_len..].contains("[complete output: artifact://")
        || text.contains("[output capture error:")
        || text.contains("[output artifact error:")
    {
        return false;
    }
    let Some(minimized) = super::minimizer::minimize(command, full.unwrap_or(&text[..body_len]))
    else {
        return false;
    };
    // A filtered complete output that still exceeds the inline view keeps the head/tail view and
    // its middle diagnostics instead.
    if full.is_some() && minimized.text.len() > BASH_HEAD + BASH_TAIL {
        return false;
    }
    let mut body = minimized.text;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!(
        "[output filtered ({}): {} -> {} bytes; the complete output is at the artifact below]\n",
        minimized.filter, minimized.before, minimized.after
    ));
    text.replace_range(..body_len, &body);
    true
}

/// Deletes the one-shot script an embedded-shell command line refers to. The host removes it when
/// it runs; a command that is never spawned would otherwise leak it into the scratch directory.
pub(in crate::tools) fn discard_unspawned_script(spawn: &std::process::Command) {
    crate::embedded_shell::discard_script(spawn.get_args());
}

/// The strict sandboxed bash's PATH on Unix: exactly the trusted package bin directories that exist
/// and are not hidden (Homebrew `/opt/homebrew/bin`, `/usr/local/bin`), the Command Line Tools,
/// then the system bins. The operator's inherited `PATH` is never extended into the sandbox.
#[cfg(not(windows))]
fn bash_path_for(ws: &Workspace) -> String {
    let mut paths: Vec<PathBuf> = Vec::new();
    for candidate in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let path = Path::new(candidate);
        if path.is_dir() && !ws.sandbox_hides(path) {
            paths.push(path.to_path_buf());
        }
    }
    if Path::new(CLT_DIR).is_dir() {
        paths.push(PathBuf::from("/Library/Developer/CommandLineTools/usr/bin"));
    }
    paths.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    std::env::join_paths(paths)
        .ok()
        .and_then(|joined| joined.into_string().ok())
        .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin".into())
}

/// Recognize only simple, conventional preview commands. This is a routing diagnostic, never an
/// approval exemption; compound shell scripts and one-shot flags retain normal bash handling.
fn preview_server(command: &str) -> bool {
    if command.chars().any(|c| ";&|`$\n\r".contains(c)) {
        return false;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    let Some(program) = words.first() else {
        return false;
    };
    if words
        .iter()
        .any(|w| matches!(*w, "--help" | "-h" | "--version" | "-v" | "--once"))
    {
        return false;
    }
    let args = &words[1..];
    let program = Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    match program {
        "pnpm" | "yarn" | "bun" | "npm" => {
            let args = args.strip_prefix(&["run"]).unwrap_or(args);
            args.first()
                .is_some_and(|arg| matches!(*arg, "dev" | "serve" | "preview" | "watch"))
        }
        "nuxt" | "nuxi" | "next" | "astro" => args.first() == Some(&"dev"),
        "vite" => {
            args.is_empty()
                || args.first().is_some_and(|arg| {
                    arg.starts_with('-') || matches!(*arg, "dev" | "serve" | "preview")
                })
        }
        "python" | "python3" => args.starts_with(&["-m", "http.server"]),
        _ => false,
    }
}

pub(in crate::tools) fn bash_tool(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
) -> Result<ToolOutput, String> {
    bash_tool_streaming(ws, args, timeout_secs, &|_| {}, &AtomicBool::new(false))
}

/// The deadline for one command: `requested` is the model's `timeout` argument, `cap` the
/// configured maximum (0 = no cap and, per the documented contract, no deadline for an omitted
/// timeout). An explicit 0 asks for the cap itself; 0 as result means no deadline.
fn effective_timeout(requested: Option<u64>, cap: u64) -> u64 {
    match requested {
        None => DEFAULT_BASH_TIMEOUT_SECS.min(cap),
        Some(0) => cap,
        Some(requested) if cap == 0 => requested,
        Some(requested) => requested.min(cap),
    }
}

/// The opt-in silence limit for one foreground command. Absent keeps the wall-time contract. It is
/// a watchdog, not a longer deadline: it must be 1..=the command's wall timeout, and it never
/// applies to background, named-session or PTY commands.
fn idle_timeout_secs(
    args: &Value,
    asynchronous: bool,
    named_session: bool,
    wall_secs: u64,
) -> Result<Option<Duration>, String> {
    let Some(idle) = opt_u64(args, "idle_timeout")? else {
        return Ok(None);
    };
    if idle == 0 {
        return Err("idle_timeout needs at least 1 second".into());
    }
    if asynchronous || named_session {
        return Err("idle_timeout is foreground-only; background and named-session commands keep the wall-time contract".into());
    }
    if wall_secs != 0 && idle > wall_secs {
        return Err(format!(
            "idle_timeout ({idle}s) cannot exceed the command timeout ({wall_secs}s)"
        ));
    }
    Ok(Some(Duration::from_secs(idle)))
}

/// Explicit command-result semantics, never inferred from diagnostic text. Acceptance checks
/// and managed jobs retain their own unmodified exit contracts.
fn expected_exit_codes(args: &Value) -> Result<Vec<i32>, String> {
    let Some(value) = args.get("expected_exit_codes") else {
        return Ok(vec![0]);
    };
    let values = value
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 8)
        .ok_or("expected_exit_codes needs 1..8 integers in 0..255")?;
    let codes = values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|code| *code <= 255)
                .map(|code| code as i32)
                .ok_or_else(|| "expected_exit_codes needs 1..8 integers in 0..255".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    for field in ["check", "background", "pty", "trace"] {
        if opt_bool(args, field)? {
            return Err(format!(
                "expected_exit_codes is for foreground inspection, not {field}=true"
            ));
        }
    }
    Ok(codes)
}

fn append_exit_note(text: &mut String, command: &str, code: i32, expected: &[i32]) {
    let silent = text == "(no output)\n";
    text.push_str(&format!("[exit code: {code}]\n"));
    if code != 0 && expected.contains(&code) {
        text.push_str("[accepted command result: exit code explicitly listed in expected_exit_codes; not verification evidence]\n");
    }
    if code == 0
        && let Some(failure) = super::minimizer::masked_failure(command, text)
    {
        text.push_str(&format!(
            "[exit 0 is the last step's status; earlier output reports a failure: {failure}. Confirm that step succeeded before relying on its result]\n"
        ));
    }
    if silent && let Some(meaning) = super::minimizer::silent_exit_meaning(command, code) {
        text.push_str(&format!(
            "[exit {code} without output conventionally means {meaning}]\n"
        ));
    }
}

/// Executes the platform-native shell with bounded live chunks, artifact capture, and cooperative cancellation.
/// The callback runs before the final result, on the calling thread, with at most 16 KiB.
/// Cancellation is checked at least once per wait slice (100 ms) and after every pipe-drain pass (at most 256 KiB).
/// Artifact failures retain inline output and exit status, but make the tool result an error.
/// Cleanup waits up to 300ms for pipe EOF; a saved stream without EOF is labeled partial.
pub fn bash_tool_streaming(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    if opt_bool(args, "background")? {
        return Err("managed background adoption is unavailable".into());
    }
    // Without an adoption owner, cooperative steering must not kill foreground effects.
    // Hard cancellation still travels through `cancel`.
    super::signals::scope(&super::signals::CallSignals::new(), || {
        bash_tool_streaming_with_adopter(
            ws,
            args,
            timeout_secs,
            0,
            on_output,
            cancel,
            &|mut job: ForegroundJob| {
                let _ = crate::platform::process::kill_group(job.child.id());
                let _ = job.child.wait();
                Err("managed background adoption is unavailable".into())
            },
        )
    })
}

pub fn bash_tool_streaming_with_adopter(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
    auto_background_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
    on_adopt: &dyn Adopter,
) -> Result<ToolOutput, String> {
    if opt_bool(args, "pty")? {
        return Err("bash pty=true requires the managed bg registry; use the native execution boundary or bg start with pty=true".into());
    }
    with_bash_interceptor(ws, args, |command, expected_codes| {
        let trace = trace::prepare(ws, args, command)?;
        let mut output = run_bash(
            ws,
            args,
            trace
                .as_ref()
                .map_or(command, |trace| trace.command.as_str()),
            expected_codes,
            timeout_secs,
            auto_background_secs,
            on_output,
            cancel,
            on_adopt,
        )?;
        if let Some(trace) = trace {
            trace.finish(&mut output);
        }
        Ok(output)
    })
}

/// Validate and decorate both launch modes at the same bash interceptor boundary.
fn with_bash_interceptor(
    ws: &Workspace,
    args: &Value,
    run: impl FnOnce(&str, &[i32]) -> Result<ToolOutput, String>,
) -> Result<ToolOutput, String> {
    let expected_codes = expected_exit_codes(args)?;
    let command = req_str(args, "command")?;
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    if command.len() > MAX_COMMAND {
        return Err(format!("command is longer than {MAX_COMMAND} bytes"));
    }
    if let Some(message) = sqlite_wal_guard(ws, args, command) {
        return Ok(ToolOutput::err(message).with_effects(crate::runtime::execution::Effects::None));
    }
    use crate::tools::exec::bash_interceptor::{self, Verdict};
    let interceptor = ws.bash_interceptor().map(|(config, offered)| {
        (
            config,
            offered.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    });
    let hint = match &interceptor {
        Some((config, offered)) => {
            match bash_interceptor::check_in(config, offered, command, &|path| {
                ws.resolve(path).is_ok()
            }) {
                Some(Verdict::Block(message)) => {
                    return Ok(ToolOutput::err(message)
                        .with_effects(crate::runtime::execution::Effects::None));
                }
                Some(Verdict::Hint(line)) => Some(line),
                None => None,
            }
        }
        None => None,
    };
    let mut output = run(command, &expected_codes)?;
    if let Some(codes) = args.get("expected_exit_codes") {
        let data = output.data.get_or_insert_with(|| serde_json::json!({}));
        if let Some(data) = data.as_object_mut() {
            data.insert("expected_exit_codes".into(), codes.clone());
        }
    }
    if let Some(note) = bash_interceptor::exit_masking_hint(command) {
        output.text.push('\n');
        output.text.push_str(note);
    }
    if let Some((config, offered)) = interceptor
        && let Some(line) = hint
            .or_else(|| bash_interceptor::ci_poll_hint(config, &offered, command, &output.text))
            .or_else(|| bash_interceptor::trim_hint(config, command))
            .or_else(|| bash_interceptor::notes_hint(config, command))
    {
        if !output.text.ends_with('\n') {
            output.text.push('\n');
        }
        output.text.push_str(&line);
    }
    Ok(output)
}

/// A PTY always belongs to `bg`; the caller supplies its registry-backed launch, never a runner.
pub(super) fn bash_tool_pty(
    ws: &Workspace,
    args: &Value,
    start: impl FnOnce(&Value) -> ToolOutput,
) -> Result<ToolOutput, String> {
    for field in ["session", "timeout", "idle_timeout"] {
        if args.get(field).is_some_and(|value| !value.is_null()) {
            return Err(format!(
                "bash pty=true does not accept {field}; it starts a managed bg terminal immediately, controlled with bg input, output, wait, resize or kill"
            ));
        }
    }
    // These flags cannot delay a PTY launch, but malformed values must not be silently ignored.
    opt_bool(args, "background")?;
    with_bash_interceptor(ws, args, |command, _| {
        let (cwd, command) = working_directory(ws, args, command)?;
        let cwd = cwd.to_str().ok_or("working directory is not UTF-8")?;
        // Forward only supported launch fields. Explicit stdin keeps ConPTY input and resize
        // available too; terminal input is implicit in the Unix background runner.
        let mut output = start(&serde_json::json!({
            "action": "start", "command": command, "cwd": cwd, "pty": true, "stdin": true
        }));
        if output
            .data
            .as_ref()
            .and_then(|data| data.get("job"))
            .is_some()
        {
            let bg_offered = ws
                .bash_interceptor()
                .is_none_or(|(_, offered)| offered.iter().any(|tool| tool == "bg"));
            if !bg_offered {
                output.text.push_str("\nLoad the Dial `bg` tool with `tool_search` names:[\"bg\"] (it is not a shell command) to control this terminal.");
            }
            if !output
                .process_outcome()
                .is_some_and(|process| process.exit_code.is_some() || process.signal.is_some())
            {
                output.text.push_str("\nPTY running; this is not command completion. Use the job name for bg input, output, wait, resize or kill. Do independent work first.");
            }
        }
        Ok(output)
    })
}

#[allow(clippy::too_many_arguments)]
fn run_bash(
    ws: &Workspace,
    args: &Value,
    command: &str,
    expected_codes: &[i32],
    timeout_secs: u64,
    auto_background_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
    on_adopt: &dyn Adopter,
) -> Result<ToolOutput, String> {
    let session = crate::tools::opt_str(args, "session")?;
    let asynchronous = opt_bool(args, "background")?;
    let explicit_exit_codes = args.get("expected_exit_codes").is_some();
    if let Some(name) = session {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(
                "shell session name needs 1..64 ASCII letters, digits, underscores or hyphens"
                    .into(),
            );
        }
        // A named session may move to the background (explicitly or at the threshold); its state is
        // written back when the job settles. Only the embedded runner keeps that state.
        if runner_for(ws)? != Runner::Embedded {
            return Err("named shell state requires the embedded backend".into());
        }
    }
    let mut sessions = session
        .map(|_| {
            ws.shell_sessions
                .try_lock()
                .map_err(|_| "another named shell command is active in this workspace".to_string())
        })
        .transpose()?;
    if let (Some(name), Some(sessions)) = (session, sessions.as_mut()) {
        admit(ws, sessions, name)?;
    }
    let seed = session.and_then(|name| sessions.as_ref()?.get(name).cloned());
    if sessions.as_ref().is_some_and(|sessions| {
        sessions.len() >= 128 && session.is_some_and(|name| !sessions.contains_key(name))
    }) {
        return Err("workspace shell state limit is 128 named sessions".into());
    }
    let mut effective = args.clone();
    if effective.get("cwd").is_none()
        && let Some(seed) = &seed
    {
        effective["cwd"] = serde_json::json!(seed.cwd);
    }
    let (cwd, command) = working_directory(ws, &effective, command)?;
    let exchange = if session.is_some() {
        let mut seed = seed.unwrap_or_else(|| crate::embedded_shell::state::State::new(&cwd));
        seed.cwd = cwd.clone();
        Some(crate::embedded_shell::state::Exchange::new(
            &ws.root.join(SCRATCH_DIR),
            &seed,
        )?)
    } else {
        None
    };
    if let Some(refusal) = super::git_guard::guard(ws, &command, &cwd) {
        return Ok(refusal);
    }
    if preview_server(&command) {
        let start = serde_json::json!({"action":"start","name":"preview","command":command});
        return Ok(ToolOutput::err(format!(
            "Command not started: preview servers must use the managed background tool `bg`: {start}"
        )));
    }
    let requested_timeout = opt_u64(args, "timeout")?;
    let timeout_secs = effective_timeout(requested_timeout, timeout_secs);
    let idle_timeout = idle_timeout_secs(args, asynchronous, session.is_some(), timeout_secs)?;

    if cancel.load(Ordering::Acquire) {
        return Ok(ToolOutput::err("[cancelled before command started]\n"));
    }
    let command_observations = if !asynchronous {
        Some(ws.snapshot_command_observations(&cwd, &command))
    } else {
        None
    };
    // Background requests need a child process, so they take the child-host path below.
    if !asynchronous
        && session.is_none()
        && idle_timeout.is_none()
        && runner_for(ws)? == Runner::InProcess
    {
        let result = inprocess::run(
            ws,
            &command,
            &cwd,
            timeout_secs,
            expected_codes,
            on_output,
            cancel,
        );
        if let Some(snapshot) = &command_observations {
            ws.acknowledge_command_observations(snapshot);
        }
        return result;
    }
    let entries_before = directory_entries(&cwd);
    let mut spool = ws
        .artifacts()
        .map(|store| store.spool())
        .transpose()
        .map_err(|e| format!("creating command output artifact: {e}"))?;
    let mut spawn = shell_command_with_state(
        ws,
        &command,
        &cwd,
        None,
        exchange.as_ref().map(|exchange| exchange.path()),
        false,
        None,
    )?;
    let spawned = (|| {
        let (reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
        let writer_err = writer
            .try_clone()
            .map_err(|e| format!("creating output pipe: {e}"))?;
        spawn.stdin(Stdio::null()).stdout(writer).stderr(writer_err);
        let child = crate::platform::process::spawn_group_limited(&mut spawn, ws.resource_limits())
            .map_err(|e| format!("starting sandboxed shell: {e}"))?;
        Ok::<_, String>((reader, child))
    })();
    let (mut reader, child) = match spawned {
        Ok(started) => started,
        Err(error) => {
            discard_unspawned_script(&spawn);
            return Err(error);
        }
    };
    // The Command owns the pipe's write ends; release them so EOF is observable.
    drop(spawn);

    let mut group = ProcessGroup::new(child);
    crate::platform::io::set_nonblocking(&reader)
        .map_err(|e| format!("configuring output pipe: {e}"))?;

    let timeout = Duration::from_secs(timeout_secs);
    let started = Instant::now();
    let mut last_output = started;
    let mut capture = Capture::new();
    let mut artifact_error = None;
    let mut capture_error = None;
    let mut eof = false;
    let mut timed_out = false;
    let mut idle_killed = false;
    let mut cancelled = false;
    let auto_background = Duration::from_secs(auto_background_secs);
    let requested = requested_timeout
        .filter(|secs| *secs > 0)
        .map(|_| timeout)
        .filter(|timeout| !timeout.is_zero());
    let mut adopt_after = if explicit_exit_codes || idle_timeout.is_some() {
        None
    } else {
        adoption_deadline(auto_background, requested)
    };
    // Set once no background owner could take the command (none registered, or every slot in use).
    // It then stays here under its own timeout: a refused hand-off would drop, and so stop, it.
    let mut adoption_refused = false;
    // Readers stopped because only a detached process held their input pipe, with the note for each.
    let mut stall_checked = started;
    let mut stalled_readers = std::collections::BTreeSet::new();
    let mut stall_notes = String::new();
    let waiter = crate::platform::wake::Waiter::new()
        .map_err(|e| format!("creating command wait channel: {e}"))?;
    loop {
        // Hard cancellation is read once per pass and outranks adoption: a cancelled command stays
        // with this owner, and the group kill below reaps it.
        let cancel_now = cancel.load(Ordering::Acquire);
        let adoption_wanted = !explicit_exit_codes
            && !adoption_refused
            && adoption_due(
                asynchronous,
                crate::tools::exec::signals::steering_requested(),
                started.elapsed(),
                adopt_after,
            );
        if may_adopt(cancel_now, adoption_wanted) && !on_adopt.available() {
            adoption_refused = true;
            adopt_after = None;
        } else if may_adopt(cancel_now, adoption_wanted) {
            crate::platform::io::set_blocking(&reader)
                .map_err(|e| format!("restoring adopted output pipe mode: {e}"))?;
            let mut captured = capture.retained();
            if captured.len() < capture.total {
                // Only a contiguous suffix can be placed in bg's offset-addressed ring.
                captured = capture.tail.clone();
            }
            let so_far = output_tail(&captured);
            let job = ForegroundJob {
                workspace: ws.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                child: release_process(group),
                reader,
                captured,
                captured_total: capture.total as u64,
                spool: spool.take(),
                session: session
                    .zip(exchange)
                    .map(|(name, exchange)| SessionClaim::new(ws, name, exchange)),
                requested: asynchronous,
            };
            return on_adopt.adopt(job).map(|id| {
                let so_far = if so_far.is_empty() {
                    String::new()
                } else {
                    format!("Output so far:\n{so_far}\n")
                };
                // The interceptor carries this request's offer; without it (direct tool use) `bg`
                // is assumed callable, as before.
                let offered = |tool: &str| {
                    ws.bash_interceptor()
                        .is_none_or(|(_, offered)| offered.iter().any(|name| name == tool))
                };
                let manage = if offered("bg") {
                    BG_OFFERED_HINT
                } else if offered("tool_search") || offered("tool_load") {
                    BG_LOAD_HINT
                } else {
                    " No polling."
                };
                let advice = background_output_note(&command)
                    .map(|note| format!(" {note}"))
                    .unwrap_or_default();
                let mut output = ToolOutput::ok(format!(
                    "command is running as background job {id} (after {}s): {command}\n{so_far}You are notified when it finishes.{manage}{advice}",
                    started.elapsed().as_secs()
                ));
                // Structured so the controller can follow the job without parsing this prose.
                output.data = Some(serde_json::json!({"background_job": id}));
                output.with_shell_launch(crate::runtime::execution::ShellLaunch {
                    stdin: crate::runtime::execution::ShellStdin::Eof,
                    terminal_allocated: false,
                    job: Some(id),
                })
            });
        }
        if cancel_now {
            cancelled = true;
            break;
        }
        if !eof {
            let before = capture.total;
            match drain_pipe(
                &mut reader,
                &mut capture,
                &mut spool,
                &mut artifact_error,
                on_output,
            ) {
                Ok(closed) => {
                    eof = closed;
                    if capture.total != before {
                        last_output = Instant::now();
                    }
                }
                Err(error) => {
                    capture_error = Some(format!("reading command output: {error}"));
                    break;
                }
            }
        }
        if group
            .exited()
            .map_err(|e| format!("observing command exit: {e}"))?
        {
            break;
        }
        // A reader whose input pipe only a detached daemon holds open (`postfix start | tail`)
        // waits until that daemon exits, which for a service is never, and the shell waits for the
        // reader. Stopping the reader lets the rest of the command run; the daemon keeps running.
        if last_output.elapsed() >= PIPE_STALL_IDLE && stall_checked.elapsed() >= PIPE_STALL_EVERY {
            stall_checked = Instant::now();
            for stall in crate::platform::pipe_stall::detached_pipe_stalls(group.child().id()) {
                if stalled_readers.insert(stall.reader) && stop_reader(stall.reader) {
                    stall_notes.push_str(&pipe_stall_note(&stall, started.elapsed()));
                }
            }
        }
        if timeout_kills(timeout, started.elapsed(), adopt_after) {
            timed_out = true;
            break;
        }
        // Opt-in silence limit: only output (not elapsed wall time) resets it, and the wall deadline
        // above still bounds the command. A quiet command shorter than the limit is never touched.
        if idle_timeout.is_some_and(|idle| last_output.elapsed() >= idle) {
            timed_out = true;
            idle_killed = true;
            break;
        }
        // Block until output arrives, the leader exits, a cancel is signalled, or a deadline
        // passes; the loop re-checks each condition on every return.
        // Cap each slice so a cancel flag stored without a wake signal is still seen promptly.
        let slice = Duration::from_millis(100);
        let mut limit = (!timeout.is_zero()).then(|| timeout.saturating_sub(started.elapsed()));
        if let Some(adopt_after) = adopt_after {
            let until = adopt_after.saturating_sub(started.elapsed());
            limit = Some(limit.map_or(until, |l| l.min(until)));
        }
        let limit = Some(limit.map_or(slice, |l| l.min(slice)));
        waiter
            .wait((!eof).then_some(&reader), Some(group.child()), limit)
            .map_err(|e| format!("waiting for command output: {e}"))?;
    }
    group.terminate();
    let status = group.wait().map_err(|e| format!("reaping command: {e}"))?;
    // Everything in the group is dead; collect what it wrote.
    drain_after_exit(
        &mut reader,
        &mut capture,
        &mut spool,
        &mut artifact_error,
        &mut capture_error,
        &mut eof,
        &waiter,
        on_output,
        Duration::from_millis(300),
    )?;
    if let Some(snapshot) = &command_observations {
        ws.acknowledge_command_observations(snapshot);
    }

    use crate::runtime::execution::Completeness;
    let original = capture.total as u64;
    let inline_retained = (capture.head.len() + capture.tail.len().min(BASH_TAIL)) as u64;
    let elided = spool.as_ref().map(|spool| spool.elided_bytes());
    let mut text = captured_text(capture);
    let body_len = text.len();
    let success = status
        .code()
        .is_some_and(|code| expected_codes.contains(&code))
        && !timed_out
        && !cancelled;
    if cancelled {
        text.push_str("[cancelled; process group killed]\n");
    } else if idle_killed {
        text.push_str(&format!(
            "[no output for {}s; process group killed by idle_timeout]\n",
            idle_timeout.map_or(0, |idle| idle.as_secs())
        ));
        text.push_str("[idle_timeout only stops a command that is silent for its whole limit; for long quiet work use bg, or raise idle_timeout]\n");
    } else if timed_out {
        text.push_str(&format!(
            "[timed out after {timeout_secs}s; process group killed]\n"
        ));
        text.push_str("[foreground commands do not survive this timeout; for a dev server/watcher discover/load bg, reuse an existing process or start it with bg, then wait for readiness before browser navigation]\n");
    } else if let Some(code) = status.code() {
        append_exit_note(&mut text, &command, code, expected_codes);
    } else if let Some(signal) = crate::platform::process::exit_signal(&status) {
        text.push_str(&format!("[terminated by signal {signal}]\n"));
    }
    text.push_str(&stall_notes);
    if !success && text.contains("Operation not permitted") && text.contains("/tmp/") {
        text.push_str("[sandbox: /tmp is outside the project and not writable; use $TMPDIR, a scratch directory inside the project]\n");
    }
    let artifact = append_output_notes(&mut text, spool, &capture_error, &mut artifact_error);
    let complete_capture = eof && capture_error.is_none();
    // A long output was cut to head and tail, so the inline text cannot be filtered faithfully.
    // When the artifact holds all of it, filter the complete output instead.
    let full = if complete_capture && original != inline_retained && elided == Some(0) {
        artifact.as_deref().and_then(|uri| {
            ws.artifacts()?
                .read_text(uri, FULL_FILTER_MAX)
                .ok()
                .flatten()
        })
    } else {
        None
    };
    let minimized = minimize_body(
        ws,
        &command,
        body_len,
        &mut text,
        complete_capture && (original == inline_retained || full.is_some()),
        full.as_deref(),
    );
    // Output shown in full inline needs no pointer to the same bytes; the artifact URI stays in
    // the structured output integrity record. Truncated or filtered output keeps the link.
    if !minimized
        && original == inline_retained
        && let Some(uri) = &artifact
    {
        text = text.replacen(&format!("[complete output: {uri}]\n"), "", 1);
    }
    // A failed build or test whose failing tests or first errors are not visible near the top of
    // the shown text (filtered, cut to head and tail, or pages down) gets them up front, taken from
    // the complete output when it was read; the shown output still follows (and stays in the artifact).
    if !success
        && !cancelled
        && !timed_out
        && let Some(digest) =
            super::minimizer::failure_digest(&command, full.as_deref().unwrap_or(&text), &text)
    {
        text.insert_str(
            0,
            &format!("[failure digest: failing tests and first errors; the output follows]\n{digest}[end of digest]\n"),
        );
    }
    let integrity = output_integrity(
        original,
        inline_retained,
        elided,
        artifact,
        eof && capture_error.is_none(),
        minimized,
    );
    let mut state_error = None;
    if let (Some(name), Some(sessions), Some(exchange)) =
        (session, sessions.as_mut(), exchange.as_ref())
    {
        sessions.remove(name);
        if !cancelled && !timed_out && capture_error.is_none() {
            match read_published(ws, exchange) {
                Ok(state) => {
                    sessions.insert(name.to_owned(), state);
                }
                Err(error) => {
                    text.push_str(&format!("[shell state invalidated: {error}]\n"));
                    state_error = Some(error);
                }
            }
        } else {
            text.push_str("[shell state invalidated by incomplete execution]\n");
        }
    }
    if !cancelled && let Some(note) = created_entries_note(entries_before.as_ref(), &cwd) {
        text.push_str(&note);
    }
    let output = ToolOutput {
        execution: Default::default(),
        data: None,
        text,
        is_error: !success
            || capture_error.is_some()
            || artifact_error.is_some()
            || state_error.is_some(),
    };
    let output = output.with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: status.code(),
        signal: crate::platform::process::exit_signal(&status),
        timed_out,
        cancelled,
        output_complete: capture_error.is_none()
            && artifact_error.is_none()
            && eof
            && integrity.retention == Completeness::Complete,
    });
    let output = output.with_output_integrity(integrity).with_shell_launch(
        crate::runtime::execution::ShellLaunch {
            stdin: crate::runtime::execution::ShellStdin::Eof,
            terminal_allocated: false,
            job: None,
        },
    );
    // The group was killed and its leader reaped above, so a timeout is a known (failed)
    // outcome the model can inspect and retry; only a user cancel stays uncertain.
    Ok(if cancelled {
        output
            .with_status(crate::tools::ToolStatus::Aborted)
            .with_effects(crate::tools::surface::outcome::Effects::Unknown)
    } else if timed_out {
        output.with_status(crate::tools::ToolStatus::Timeout)
    } else {
        output
    })
}

/// Build the platform shell command in `cwd`. Commands use the workspace sandbox policy or its
/// explicit unsandboxed opt-out; on Windows the sandbox is the Low-integrity Dev backend.
/// Stdio and process ownership belong to the caller.
#[cfg(not(windows))]
pub(in crate::tools) fn shell_command(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
) -> Result<std::process::Command, String> {
    shell_command_mode(ws, script, cwd, None)
}

/// Eval's mandatory process fence is independent of the filesystem sandbox opt-out.
#[cfg(not(windows))]
pub(super) fn shell_command_fenced(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
    bundle: Option<&crate::setup::deps::bundled_python::Runtime>,
) -> Result<std::process::Command, String> {
    shell_command_with_state(ws, script, cwd, None, None, true, bundle)
}

/// Which program runs a `bash` command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Runner {
    /// bash with `-c script`: `/bin/bash`, or Git for Windows' `bash.exe` on Windows.
    Bash(PathBuf),
    /// PowerShell (Windows only).
    PowerShell,
    /// Dial's built-in shell, hosted by a child Dial process.
    Embedded,
    /// Dial's built-in shell run inside this process; allowed only with `sandbox.mode = "off"`.
    InProcess,
}

/// Maps `shell.backend` to a runner. `auto` uses the built-in shell on every OS when compiled in;
/// slim builds fall back to system bash, then PowerShell on Windows.
fn resolve_runner(
    backend: ShellBackend,
    system_bash: Option<PathBuf>,
    embedded_built: bool,
    windows: bool,
) -> Result<Runner, String> {
    match backend {
        ShellBackend::Auto if embedded_built => Ok(Runner::Embedded),
        ShellBackend::Auto => match system_bash {
            Some(bash) => Ok(Runner::Bash(bash)),
            None if windows => Ok(Runner::PowerShell),
            None => Err("no bash was found and the built-in shell is not in this build".into()),
        },
        ShellBackend::System => system_bash.map(Runner::Bash).ok_or_else(|| {
            if windows {
                "shell.backend is \"system\" but no bash was found; install Git for Windows, or set shell.backend to \"embedded\" or \"powershell\"".into()
            } else {
                "shell.backend is \"system\" but no bash was found (/bin/bash is missing); set shell.backend to \"embedded\"".into()
            }
        }),
        ShellBackend::PowerShell if windows => Ok(Runner::PowerShell),
        ShellBackend::PowerShell => Err(
            "shell.backend is \"powershell\" but PowerShell is only available on Windows; set shell.backend to \"system\" or \"embedded\""
                .into(),
        ),
        ShellBackend::InProcess if embedded_built => Ok(Runner::InProcess),
        ShellBackend::InProcess => Err(
            "the in-process shell is not in this build (the embedded-shell feature is disabled); set shell.backend to \"system\""
                .into(),
        ),
        ShellBackend::Embedded if embedded_built => Ok(Runner::Embedded),
        ShellBackend::Embedded => Err(
            "the built-in shell is not in this build (the embedded-shell feature is disabled); set shell.backend to \"system\""
                .into(),
        ),
    }
}

/// The runner for this workspace. The in-process shell shares Dial's address space, credentials
/// and file access, so it is refused unless the operator turned the OS sandbox off; it never
/// falls back to another backend or runs beside a configured sandbox.
fn runner_for(ws: &Workspace) -> Result<Runner, String> {
    let runner = resolve_runner(
        ws.shell_backend,
        find_system_bash(),
        cfg!(feature = "embedded-shell"),
        cfg!(windows),
    )?;
    if runner == Runner::InProcess && !ws.unsandboxed {
        return Err(
            "shell.backend is \"inprocess\", which runs commands inside the Dial process and is allowed only when sandbox.mode is \"off\"; nothing was run. Set sandbox.mode to \"off\", or set shell.backend to \"auto\", \"system\" or \"embedded\""
                .into(),
        );
    }
    Ok(runner)
}

/// One variable a caller adds to a command's environment through [`environment_prefix`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvEntry {
    /// `name=value`.
    Set(String, String),
    /// `name=value` followed by the variable's current value, if any, joined with the platform
    /// path-list separator (`;` on Windows, `:` elsewhere).
    Prepend(String, String),
}

/// `command` preceded by statements that add `entries` to its environment, in the syntax of the
/// shell this workspace runs commands with: POSIX `export` for bash and the built-in shell,
/// `$env:` assignments for PowerShell. Dial rebuilds a command's environment from an allowlist, so
/// the shell line is the only place an extra variable can enter without changing the tool schema.
pub fn environment_prefix(
    ws: &Workspace,
    entries: &[EnvEntry],
    command: &str,
) -> Result<String, String> {
    Ok(render_environment(&runner_for(ws)?, entries, command))
}

fn render_environment(runner: &Runner, entries: &[EnvEntry], command: &str) -> String {
    let separator = if cfg!(windows) { ';' } else { ':' };
    if *runner == Runner::PowerShell {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
        let statements: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                EnvEntry::Set(name, value) => format!("$env:{name} = {}", quote(value)),
                EnvEntry::Prepend(name, value) => format!(
                    "$env:{name} = {} + $(if ($env:{name}) {{ '{separator}' + $env:{name} }} else {{ '' }})",
                    quote(value)
                ),
            })
            .collect();
        return format!("{}\n{command}", statements.join("; "));
    }
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    let assignments: Vec<String> = entries
        .iter()
        .map(|entry| match entry {
            EnvEntry::Set(name, value) => format!("{name}={}", quote(value)),
            EnvEntry::Prepend(name, value) => {
                format!("{name}={}\"${{{name}:+{separator}${name}}}\"", quote(value))
            }
        })
        .collect();
    format!("export {}\n{command}", assignments.join(" "))
}

/// The bash `auto` and `system` use: `/bin/bash` on Unix; on Windows Git for Windows' `bash.exe`
/// from its standard install folders or PATH. WSL's `bash.exe` launcher is not Git Bash and is
/// never returned.
fn find_system_bash() -> Option<PathBuf> {
    if !cfg!(windows) {
        let bash = Path::new("/bin/bash");
        return bash.is_file().then(|| bash.to_path_buf());
    }
    let mut bin_dirs = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            bin_dirs.push(PathBuf::from(dir).join("Git").join("bin"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bin_dirs.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin"),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        bin_dirs.extend(std::env::split_paths(&path));
    }
    git_for_windows_bash(bin_dirs)
}

/// The first `bin` folder holding `bash.exe` whose Git installation (its parent) also has
/// `cmd\git.exe`. That pair identifies Git for Windows and rejects WSL's launcher.
fn git_for_windows_bash(bin_dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    bin_dirs.into_iter().find_map(|bin| {
        let bash = bin.join("bash.exe");
        let git = bin.parent()?.join("cmd").join("git.exe");
        (bash.is_file() && git.is_file()).then_some(bash)
    })
}

/// Windows PowerShell running `script`, exiting with the last native command's failure status.
fn powershell_command(script: &str) -> Result<std::process::Command, String> {
    let system_root = std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
    let powershell = Path::new(&system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let command = format!(
        "$global:LASTEXITCODE = 0; {script}; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}"
    );
    let mut spawn = std::process::Command::new(powershell);
    spawn
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(command);
    Ok(spawn)
}

/// A command that runs a user-written script outside the `bash` tool, such as `/loop --while`
/// conditions and ratchet flow commands: `sh -c` on Unix. Windows has no `sh`, so it uses Git for
/// Windows' bash when installed and otherwise Windows PowerShell, the `bash` tool's `auto` order
/// without the built-in shell. Environment, stdio and process ownership belong to the caller.
pub fn host_script_command(script: &str) -> Result<std::process::Command, String> {
    if !cfg!(windows) {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", script]);
        return Ok(command);
    }
    match find_system_bash() {
        Some(bash) => {
            let mut command = std::process::Command::new(bash);
            command.args(["-c", script]);
            Ok(command)
        }
        None => powershell_command(script),
    }
}

/// Writes `script` for the built-in shell and returns the host program and its arguments.
fn embedded_invocation(
    scratch: &Path,
    script: &str,
    state: Option<&Path>,
) -> Result<(String, Vec<String>), String> {
    let (program, mut args) = crate::embedded_shell::invocation(scratch, script)
        .map_err(|e| format!("preparing the built-in shell: {e}"))?;
    let prepared = (|| {
        if let Some(state) = state {
            args.push(
                state
                    .to_str()
                    .ok_or("shell state path is not UTF-8")?
                    .to_owned(),
            );
        }
        Ok::<_, String>(
            program
                .to_str()
                .ok_or("Dial executable path is not UTF-8")?
                .to_owned(),
        )
    })();
    match prepared {
        Ok(program) => Ok((program, args)),
        Err(error) => {
            crate::embedded_shell::discard_script(&args);
            Err(error)
        }
    }
}

/// Refuses Windows sandbox settings the Low-integrity backend cannot enforce.
#[cfg(windows)]
pub(in crate::tools) fn windows_enforceable(ws: &Workspace) -> Result<(), String> {
    if ws.network_rules().is_some() {
        return Err("sandbox.network_allow and sandbox.network_deny cannot be enforced on Windows; clear them or set sandbox.mode = \"off\"".into());
    }
    Ok(())
}

/// `program args…` under the workspace's Windows sandbox. The Low-integrity launcher rebuilds
/// the allowlisted environment and TEMP itself.
#[cfg(windows)]
pub(in crate::tools) fn windows_sandboxed(
    ws: &Workspace,
    program: &str,
    args: &[String],
) -> Result<std::process::Command, String> {
    windows_enforceable(ws)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    ws.sandbox
        .clone()
        .unwrap_or_else(super::sandbox::Sandbox::detect)
        .command_with_policy(&ws.root, &ws.sandbox_policy(), program, &arg_refs)
}

/// Background PTYs alone retain a new private terminal session through the sandbox.
pub(in crate::tools) fn shell_command_mode(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
    owned_terminal: Option<i32>,
) -> Result<std::process::Command, String> {
    shell_command_with_state(ws, script, cwd, owned_terminal, None, false, None)
}

/// Command that runs `script` in `ws`'s root through the workspace's configured shell backend (the
/// embedded engine by default) and sandbox. It serves callers outside a tool turn, such as learning
/// verifiers and autoresearch benchmarks. The caller owns stdio, capture and the exit-status verdict;
/// an `Err` means the command could not be built, not that the script failed.
pub fn verifier_command(ws: &Workspace, script: &str) -> Result<std::process::Command, String> {
    shell_command_mode(ws, script, ws.root(), None)
}
fn shell_command_with_state(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
    owned_terminal: Option<i32>,
    state: Option<&Path>,
    fork_fence: bool,
    bundle: Option<&crate::setup::deps::bundled_python::Runtime>,
) -> Result<std::process::Command, String> {
    let runner = runner_for(ws)?;
    #[cfg(windows)]
    let _ = (fork_fence, bundle);
    // Background jobs and tool helpers need a child process; they run the same shell in a child
    // host (only reachable with the sandbox off, which `runner_for` verified).
    let runner = if runner == Runner::InProcess {
        Runner::Embedded
    } else {
        runner
    };
    #[cfg(windows)]
    {
        if !ws.unsandboxed {
            windows_enforceable(ws)?;
        }
        if owned_terminal.is_some() {
            return Err("Windows PTY commands require the ConPTY process interface".into());
        }
        let scratch = ws.root.join(SCRATCH_DIR);
        fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
        let mut spawn = match runner {
            Runner::Bash(bash) => {
                let mut spawn = std::process::Command::new(bash);
                spawn.args(["-c", &missing_program::decorate(script)]);
                spawn
            }
            // `InProcess` was mapped to `Embedded` above; both run the child shell host here.
            Runner::Embedded | Runner::InProcess => {
                let (program, args) = embedded_invocation(&scratch, script, state)?;
                let mut spawn = std::process::Command::new(program);
                spawn.args(args);
                spawn
            }
            Runner::PowerShell => powershell_command(script)?,
        };
        let mut spawn = if ws.unsandboxed {
            inherit_allowed_env(&mut spawn);
            spawn
        } else {
            let args: Vec<String> = spawn
                .get_args()
                .map(|arg| arg.to_str().map(str::to_owned))
                .collect::<Option<_>>()
                .ok_or("command arguments are not UTF-8")?;
            let built = spawn
                .get_program()
                .to_str()
                .ok_or_else(|| "shell path is not UTF-8".to_string())
                .and_then(|program| windows_sandboxed(ws, program, &args));
            match built {
                Ok(sandboxed) => sandboxed,
                Err(error) => {
                    crate::embedded_shell::discard_script(&args);
                    return Err(error);
                }
            }
        };
        if bundle.is_none()
            && let Some((_, path)) = bundled_path::fallback(ws, std::env::var_os("PATH").as_deref())
        {
            spawn.env("PATH", path);
        }
        crate::tools::exec::cargo_target::apply(&mut spawn, ws.cargo_target_dir());
        spawn
            .current_dir(cwd)
            .env("TEMP", &scratch)
            .env("TMP", &scratch)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never");
        Ok(spawn)
    }
    #[cfg(not(windows))]
    {
        let root = ws.root.to_str().ok_or("workspace path is not UTF-8")?;
        let cwd_str = cwd.to_str().ok_or("working directory is not UTF-8")?;
        let scratch = ws.root.join(SCRATCH_DIR);
        fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
        let scratch_str = scratch
            .to_str()
            .ok_or("scratch path is not UTF-8")?
            .to_owned();
        let strict = matches!(ws.policy, None | Some(Policy::Strict { .. }));
        // bubblewrap starts every sandboxed program in the workspace root (`--chdir`), so the
        // process working directory set below does not reach the shell; enter `cwd` explicitly.
        #[cfg(target_os = "linux")]
        let entered;
        #[cfg(target_os = "linux")]
        let script = if !ws.unsandboxed && cwd != ws.root.as_path() {
            entered = format!(
                "cd {} || exit 1\n{script}",
                crate::tools::exec::kernel::shell_quote(cwd_str)
            );
            entered.as_str()
        } else {
            script
        };
        // Without a resolvable `python3`, bash falls back to Dial's bundled CPython (eval
        // launches carry their own bundle and keep their exact interpreter).
        let shell_path: Option<std::ffi::OsString> = if !ws.unsandboxed && strict {
            Some(bash_path_for(ws).into())
        } else {
            std::env::var_os("PATH")
        };
        let python = match bundle {
            None if !fork_fence => bundled_path::fallback(ws, shell_path.as_deref()),
            _ => None,
        };
        let bundle = bundle.or(python.as_ref().map(|(runtime, _)| *runtime));
        let (program, args) = match runner {
            Runner::Bash(bash) => (
                bash.to_str().ok_or("bash path is not UTF-8")?.to_owned(),
                // Missing-program hints are for model commands. A fenced launch script is
                // controller-owned, and the hint prelude forks (`$(…)` on Bash 3), which the
                // macOS in-profile fork fence refuses before the interpreter is exec'd.
                vec![
                    "-c".to_owned(),
                    if fork_fence {
                        script.to_owned()
                    } else {
                        missing_program::decorate(script)
                    },
                ],
            ),
            Runner::Embedded | Runner::InProcess => embedded_invocation(&scratch, script, state)?,
            Runner::PowerShell => return Err("PowerShell is only available on Windows".into()),
        };
        let mut spawn;
        if ws.unsandboxed {
            spawn = std::process::Command::new(&program);
            spawn.args(&args);
            inherit_allowed_env(&mut spawn);
            spawn
                .env("TMPDIR", &scratch_str)
                .env("PAGER", "cat")
                .env("GIT_PAGER", "cat");
            #[cfg(target_os = "linux")]
            if fork_fence {
                super::sandbox::fork_fence::direct(&mut spawn)?;
            }
        } else {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let sandbox = ws.sandbox.clone().unwrap_or_else(Sandbox::detect);
            let policy = ws.sandbox_policy();
            let network = match &policy {
                Policy::Strict { allow_network, .. } | Policy::Dev { allow_network, .. } => {
                    *allow_network
                }
            };
            let proxy = match ws.network_proxy() {
                Ok(proxy) => proxy.filter(|_| network),
                Err(error) => {
                    crate::embedded_shell::discard_script(&args);
                    return Err(error);
                }
            };
            let built = sandbox.command_routed(
                &ws.root,
                &policy,
                proxy.as_deref(),
                &program,
                &arg_refs,
                super::sandbox::CommandMode {
                    owned_terminal,
                    fork_fence,
                    bundle,
                },
            );
            spawn = match built {
                Ok(spawn) => spawn,
                Err(error) => {
                    crate::embedded_shell::discard_script(&args);
                    return Err(error);
                }
            };
            if strict {
                spawn
                    .env_clear()
                    .env("PATH", bash_path_for(ws))
                    .env("HOME", root)
                    .env("TMPDIR", &scratch_str)
                    .env("PWD", cwd_str)
                    .env("SHELL", "/bin/bash")
                    .env("LANG", "en_US.UTF-8");
                if Path::new(CLT_DIR).is_dir() {
                    spawn.env("DEVELOPER_DIR", CLT_DIR);
                }
            } else {
                spawn.env("PAGER", "cat").env("GIT_PAGER", "cat");
            }
            // After the environment is final, so no inherited proxy setting can bypass it.
            if let Some(proxy) = &proxy {
                spawn.envs(crate::tools::exec::sandbox::netproxy::client_env(
                    proxy.client_port(),
                ));
            }
        }
        if let Some((_, path)) = &python {
            spawn.env("PATH", path);
        }
        crate::tools::exec::cargo_target::apply(&mut spawn, ws.cargo_target_dir());
        spawn
            .current_dir(cwd)
            .env("TERM", "dumb")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env("SSH_ASKPASS", "/usr/bin/false");
        Ok(spawn)
    }
}

mod bundled_path;
mod inprocess;
mod missing_program;
mod trace;

#[cfg(test)]
mod backend_tests;

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod inprocess_tests;

#[cfg(all(test, unix))]
mod drain_tests;
