//! The `git` tool: structured git, jj (jujutsu) and GitHub (`gh`) operations.
//!
//! Every action builds a fixed argument vector (no shell), validates refs and paths, and runs
//! with fsmonitor, external diff drivers, textconv and pagers switched off, because repository
//! configuration is untrusted. Approved commits/pushes honour the repository's hooks and signing
//! unless `git.bypass_hooks` is set, which the output then says. Reads need no approval;
//! anything that changes the repository or reaches the network does, except the read-only `gh`
//! queries (`pr_view`, `pr_checks`, `issue_view`), which are in `READS` and run unapproved. There
//! is deliberately no force-push, reset, clean, rebase or checkout of paths: those stay with
//! `bash`.
use super::gh_common::clip_with_hint;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64, req_str, tool_schema};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

/// Hard ceiling on captured bytes; pipes are still drained past it so git never sees SIGPIPE.
const MAX_CAPTURE: usize = 64 * 1024 * 1024;
const MAX_MESSAGE: usize = 8 * 1024;
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
const HOOKS_SKIPPED: &str =
    "[repository hooks and commit signing were skipped because git.bypass_hooks = true]";

const DESCRIPTION: &str = "Git, jj (jujutsu) and GitHub operations without shell quoting. Read actions: `status` \
(optional `path`; large git status returns counts and samples, complete output via artifact_read), `diff` \
(optional `staged`, `base` ref, `path`), `log` (`limit` ≤ 50, optional `path`), `show` (`rev`), `blame` (`path`, optional \
`start`/`end` lines, `rev`), `branches`, `pr_view`/`pr_checks`/`issue_view` (`number`; need the `gh` CLI). Changing actions \
(ask approval): `add` (`paths`), `unstage` (`paths`), `commit` (`message`; commits what is staged, or every tracked change \
with `all`), `branch_create` (`name`, optional `from`; switches to it), `switch` (`name`), `push` (optional `remote`, \
`branch`; never forces), `pr_create` (`title`, `body`, optional `base`, `draft`). In a jj repository (`.jj/`) status, diff, \
log and commit use jj unless `vcs` is \"git\"; jj has no staging area. Repository hooks and signing apply to commit/push \
unless the operator set git.bypass_hooks. Long output is cut with the full text kept as an artifact:// link. For reset, \
rebase, force-push or anything else, use bash.";

#[derive(Default)]
pub struct Git {
    bypass_hooks: bool,
}

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Git {
        bypass_hooks: config.git.bypass_hooks,
    })]
}

const READS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "branches",
    "pr_view",
    "pr_checks",
    "issue_view",
];

/// Ref, remote and branch names: conservative, never an option.
fn valid_ref(text: &str) -> Result<&str, String> {
    let ok = !text.is_empty()
        && text.len() <= 200
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/@{}^~-".contains(c));
    if ok {
        Ok(text)
    } else {
        Err(format!("invalid ref name {text:?}"))
    }
}

fn number(args: &Value) -> Result<String, String> {
    let n = opt_u64(args, "number")?.ok_or("missing required argument \"number\"")?;
    Ok(n.to_string())
}

fn paths(ws: &Workspace, args: &Value) -> Result<Vec<String>, String> {
    let list = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or("missing required array argument \"paths\"")?;
    if list.is_empty() || list.len() > 200 {
        return Err("`paths` needs 1..200 entries".into());
    }
    list.iter()
        .map(|p| one_path(ws, p.as_str().ok_or("`paths` entries must be strings")?))
        .collect()
}

/// A workspace-confined path as a repository-relative string, never an option.
fn one_path(ws: &Workspace, input: &str) -> Result<String, String> {
    let abs = ws.resolve(input)?;
    let rel = abs
        .strip_prefix(ws.root())
        .map_err(|_| format!("path {input:?} is outside the workspace"))?;
    let text = rel.to_string_lossy().into_owned();
    Ok(if text.is_empty() { ".".into() } else { text })
}

/// Hook/signing-sensitive invocation: whether repository hooks may run for this call.
#[derive(Clone, Copy, PartialEq)]
enum Hooks {
    Off,
    Repository,
}

/// `-c` overrides that point `core.hooksPath` at the null device, so repository hooks do not run.
/// Shared by the agent tool, the `/git` panel, and `dial commit` when `git.bypass_hooks` is set.
pub fn hooks_off_args() -> [&'static str; 2] {
    #[cfg(windows)]
    return ["-c", "core.hooksPath=NUL"];
    #[cfg(not(windows))]
    return ["-c", "core.hooksPath=/dev/null"];
}

fn combine(out: String, err: String) -> String {
    if err.trim().is_empty() {
        out
    } else if out.trim().is_empty() {
        err
    } else {
        format!("{out}\n{err}")
    }
}

/// One bounded external command: program, arguments, working root, time limit and guards.
struct Invocation<'a> {
    program: &'a str,
    args: &'a [String],
    root: &'a Path,
    timeout: Duration,
    hooks: Hooks,
    envs: &'a [(&'a str, &'a std::ffi::OsStr)],
    cancel: &'a AtomicBool,
    may_mutate: bool,
}

/// Run `program` with a bounded time. Returns (success, stdout, stderr), each complete up to
/// `MAX_CAPTURE`; callers clip for display.
fn run_streams(inv: Invocation<'_>) -> Result<(bool, String, String), String> {
    let Invocation {
        program,
        args,
        root,
        timeout,
        hooks,
        envs,
        cancel,
        may_mutate,
    } = inv;
    let mut command = Command::new(program);
    if program == "git" {
        // Repository config is untrusted: no fsmonitor, pager, external diff or textconv.
        if hooks == Hooks::Off {
            command.args(hooks_off_args());
        }
        command.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.pager=cat",
            "-c",
            "diff.external=",
            "-c",
            "protocol.ext.allow=never",
            "--no-pager",
        ]);
    } else if program == "jj" {
        command.args(["--no-pager", "--color=never"]);
    }
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut policy = crate::runtime::process::Policy::bounded(timeout, MAX_CAPTURE);
    policy.stderr_cap = MAX_CAPTURE;
    policy.may_mutate = may_mutate;
    let output =
        crate::runtime::process::run_with_policy(command, None, policy, cancel, &mut |_, _| {})?;
    if output.cancelled || output.timed_out || output.io_error.is_some() {
        return Err(output.failure(program, timeout));
    }
    if args.first().is_some_and(|arg| arg == "status")
        && (output.stdout_total > output.stdout.len()
            || output.stderr_total > output.stderr.len()
            || output.output_incomplete)
    {
        return Err("status output exceeded the capture limit or was incomplete; exact counts and full retention are unavailable. Retry with `path` to narrow the scope.".into());
    }
    Ok((
        output.success(),
        output.stdout_text(),
        output.text(crate::runtime::process::Stream::Stderr),
    ))
}

/// Run `program` with a bounded time. Returns (success, stdout+stderr combined), complete up to
/// `MAX_CAPTURE`; callers clip for display.
fn run(inv: Invocation<'_>) -> Result<(bool, String), String> {
    let (ok, out, err) = run_streams(inv)?;
    Ok((ok, combine(out, err)))
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// Run git with stdout and stderr returned separately. `mutating` calls honour repository hooks
/// unless `bypass_hooks`; reads never run hooks. `envs` lets callers use a scratch index.
fn git_streams(
    root: &Path,
    args: &[String],
    mutating: bool,
    bypass_hooks: bool,
    envs: &[(&str, &std::ffi::OsStr)],
    cancel: &AtomicBool,
) -> Result<(bool, String, String), String> {
    let hooks = if mutating && !bypass_hooks {
        Hooks::Repository
    } else {
        Hooks::Off
    };
    run_streams(Invocation {
        program: "git",
        args,
        root,
        timeout: LOCAL_TIMEOUT,
        hooks,
        envs,
        cancel,
        may_mutate: mutating,
    })
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Short branch name with `refs/heads/` stripped; `None` when detached or bare.
    pub branch: Option<String>,
    pub head: String,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
}

/// Parse `git worktree list --porcelain` into one entry per `worktree` block.
pub(crate) fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut trees: Vec<Worktree> = Vec::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            trees.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                head: String::new(),
                bare: false,
                detached: false,
                locked: false,
            });
            continue;
        }
        let Some(tree) = trees.last_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            tree.head = head.to_owned();
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            tree.branch = Some(branch.to_owned());
        } else if line == "bare" {
            tree.bare = true;
        } else if line == "detached" {
            tree.detached = true;
        } else if line == "locked" || line.starts_with("locked ") {
            tree.locked = true;
        }
    }
    trees
}

/// Never-set cancel flag for helpers that run outside a tool call and cannot be interrupted.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Run git and return its stdout, or git's own message on failure. Stderr stays out of the success
/// value because callers parse stdout; a warning there would corrupt a diff or a file list.
fn git_checked(root: &Path, args: &[String], mutating: bool) -> Result<String, String> {
    match git_streams(root, args, mutating, false, &[], &NEVER_CANCELLED)? {
        (true, out, _) => Ok(out),
        (false, out, err) => Err(combine(out, err).trim().to_owned()),
    }
}

/// Worktrees registered for the repository containing `root`; the primary checkout is first.
pub fn worktree_list(root: &Path) -> Result<Vec<Worktree>, String> {
    let text = git_checked(root, &strings(&["worktree", "list", "--porcelain"]), false)?;
    Ok(parse_worktrees(&text))
}

/// Create `path` as a worktree on the new branch `branch`, based on `base`.
pub(crate) fn worktree_add(
    root: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<(), String> {
    let args = [
        "worktree".to_owned(),
        "add".to_owned(),
        "-b".to_owned(),
        branch.to_owned(),
        path.to_string_lossy().into_owned(),
        base.to_owned(),
    ];
    git_checked(root, &args, true).map(drop)
}

/// Create `path` on the new branch `branch` from `HEAD` and carry the source checkout's uncommitted
/// changes into it: tracked edits, staged or not, are applied as one binary
/// patch, and untracked files are copied (symlinks as links). Returns whether anything was carried over.
pub fn worktree_add_keep_changes(root: &Path, path: &Path, branch: &str) -> Result<bool, String> {
    worktree_add(root, path, branch, "HEAD")?;
    match carry_changes(root, path) {
        Ok(carried) => Ok(carried),
        Err(error) => {
            // Roll back the registered worktree and its new branch so a retry is not blocked.
            let _ = worktree_remove(root, path, true);
            let _ = run_checked(root, &["branch", "-D", branch], true);
            Err(error)
        }
    }
}

fn carry_changes(root: &Path, path: &Path) -> Result<bool, String> {
    let top = repo_toplevel(root)?;
    let patch = run_checked(
        &top,
        &[
            "diff",
            "HEAD",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ],
        false,
    )?;
    let untracked = run_checked(
        &top,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        false,
    )?;
    let untracked: Vec<&str> = untracked
        .split('\0')
        .filter(|name| !name.is_empty())
        .collect();
    if patch.is_empty() && untracked.is_empty() {
        return Ok(false);
    }
    if !patch.is_empty() {
        // A private, exclusively created file: the patch holds the whole uncommitted diff.
        let mut patch_file = tempfile::Builder::new()
            .prefix("dial-wt-")
            .suffix(".patch")
            .tempfile()
            .map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut patch_file, patch.as_bytes()).map_err(|e| e.to_string())?;
        run_checked(
            path,
            &["apply", "--binary", &patch_file.path().to_string_lossy()],
            true,
        )?;
    }
    for name in untracked {
        let (from, to) = (top.join(name), path.join(name));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        copy_untracked(&from, &to).map_err(|e| format!("copying {name}: {e}"))?;
    }
    Ok(true)
}

/// Copies one untracked entry. A symlink stays a symlink: following it would copy content from
/// outside the repository, and a directory link cannot be copied as a file.
fn copy_untracked(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return Ok(());
    }
    std::fs::copy(from, to).map(drop)
}

/// Remove a registered worktree. A dirty or locked worktree is refused unless `force`.
pub fn worktree_remove(root: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut args = strings(&["worktree", "remove"]);
    if force {
        args.push("--force".to_owned());
    }
    args.push(path.to_string_lossy().into_owned());
    git_checked(root, &args, true).map(drop)
}

/// Worktree location for `branch`: `~/.dial/wt/<hash of root>/<branch with / as ->`.
pub fn worktree_path_for(root: &Path, branch: &str) -> Result<PathBuf, String> {
    let base = super::gh_pr::worktrees_root()?;
    let hash = super::gh_pr::hash_path(root);
    Ok(base.join(hash).join(branch.replace('/', "-")))
}

/// Run a git command and return stdout, or git's stderr on failure.
pub fn run_checked(root: &Path, args: &[&str], mutating: bool) -> Result<String, String> {
    git_checked(root, &strings(args), mutating)
}

/// An independent baseline of the filtered development copy: no parent gitdir, remotes,
/// hooks, credential config, alternates or hardlinked objects are inherited.
pub fn initialize_private_snapshot(root: &Path) -> Result<(), String> {
    use std::ffi::OsStr;
    let gitdir = root.join(".git");
    let index = gitdir.join("index");
    let objects = gitdir.join("objects");
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let env = [
        ("GIT_CONFIG_NOSYSTEM", OsStr::new("1")),
        ("GIT_CONFIG_GLOBAL", OsStr::new(null)),
        ("GIT_CONFIG_SYSTEM", OsStr::new(null)),
        ("GIT_CONFIG_COUNT", OsStr::new("0")),
        ("GIT_CONFIG_PARAMETERS", OsStr::new("")),
        ("GIT_DIR", gitdir.as_os_str()),
        ("GIT_COMMON_DIR", gitdir.as_os_str()),
        ("GIT_OBJECT_DIRECTORY", objects.as_os_str()),
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", OsStr::new("")),
        ("GIT_CONFIG", OsStr::new(null)),
        ("GIT_WORK_TREE", root.as_os_str()),
        ("GIT_INDEX_FILE", index.as_os_str()),
    ];
    for args in [
        vec!["init", "--quiet", "--template="],
        vec![
            "add",
            "--force",
            "--all",
            "--",
            ".",
            ":(exclude)node_modules",
            ":(exclude).venv",
            ":(exclude)venv",
            ":(exclude)vendor",
            ":(glob,exclude)**/node_modules/**",
            ":(glob,exclude)**/.venv/**",
            ":(glob,exclude)**/venv/**",
            ":(glob,exclude)**/vendor/**",
        ],
        vec![
            "-c",
            "user.name=Dial",
            "-c",
            "user.email=dial@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Isolated workspace baseline",
        ],
    ] {
        let (ok, out, err) =
            git_streams(root, &strings(&args), true, true, &env, &NEVER_CANCELLED)?;
        if !ok {
            return Err(combine(out, err));
        }
    }
    // Dependencies are available to checks but are never source changes in this repository.
    std::fs::create_dir_all(gitdir.join("info")).map_err(|error| error.to_string())?;
    std::fs::write(
        gitdir.join("info/exclude"),
        "node_modules/\n.venv/\nvenv/\nvendor/\n",
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// The repository's top-level directory. Patch paths and untracked paths are relative to it, so
/// `/wt` must run its diff and clean from there, not from a launch subdirectory.
fn repo_toplevel(root: &Path) -> Result<PathBuf, String> {
    run_checked(root, &["rev-parse", "--show-toplevel"], false)
        .map(|top| PathBuf::from(top.trim_end_matches(['\n', '\r'])))
}

/// Reset tracked files to HEAD and remove untracked, non-ignored files from the source checkout once
/// `/wt` has moved its changes into a worktree.
pub fn clean_source_checkout(root: &Path) -> Result<(), String> {
    let top = repo_toplevel(root)?;
    run_checked(&top, &["reset", "--hard", "HEAD"], true)?;
    run_checked(&top, &["clean", "-fd"], true)?;
    Ok(())
}

/// True when the workspace is a jj repository (possibly colocated with git).
pub(crate) fn is_jj(root: &Path) -> bool {
    root.join(".jj").is_dir()
}

fn use_jj(ws: &Workspace, args: &Value, action: &str) -> Result<bool, String> {
    let jj_action = matches!(action, "status" | "diff" | "log" | "commit");
    match opt_str(args, "vcs")? {
        Some("git") => Ok(false),
        Some("jj") if jj_action => Ok(true),
        Some("jj") => Err(format!(
            "{action} has no jj form here; use vcs \"git\" or bash"
        )),
        Some(other) => Err(format!("unknown vcs {other:?}; use \"git\" or \"jj\"")),
        None => Ok(jj_action && is_jj(ws.root())),
    }
}

fn jj_dispatch(
    ws: &Workspace,
    args: &Value,
    action: &str,
    cancel: &AtomicBool,
) -> Result<(bool, String), String> {
    let root = ws.root();
    let jj = |a: Vec<String>| {
        run(Invocation {
            program: "jj",
            args: &a,
            root,
            timeout: LOCAL_TIMEOUT,
            hooks: Hooks::Off,
            envs: &[],
            cancel,
            may_mutate: !READS.contains(&action),
        })
    };
    match action {
        "status" => {
            let mut a = strings(&["status"]);
            if let Some(path) = opt_str(args, "path")? {
                a.push(jj_path(ws, path)?);
            }
            jj(a)
        }
        "diff" => {
            if opt_bool(args, "staged")? {
                return Err("jj has no staging area; diff shows the working-copy change (omit `staged`, or pass vcs \"git\")".into());
            }
            let mut a = strings(&["diff", "--git"]);
            if let Some(base) = opt_str(args, "base")? {
                a.extend(["--from".into(), valid_ref(base)?.to_owned()]);
            }
            if let Some(path) = opt_str(args, "path")? {
                a.push(jj_path(ws, path)?);
            }
            jj(a)
        }
        "log" => {
            let n = opt_u64(args, "limit")?.unwrap_or(20).clamp(1, 50);
            let mut a = vec!["log".into(), "--limit".into(), n.to_string()];
            if let Some(path) = opt_str(args, "path")? {
                a.push(jj_path(ws, path)?);
            }
            jj(a)
        }
        "commit" => {
            let message = commit_message(args)?;
            let mut a = strings(&["commit", "-m", message]);
            if args.get("paths").is_some_and(|p| !p.is_null()) {
                a.extend(paths(ws, args)?.into_iter().map(|p| jj_file(&p)));
            }
            jj(a)
        }
        _ => unreachable!("use_jj limits actions"),
    }
}

/// A workspace path as a literal jj fileset (`file:"..."`), never a revset or option.
fn jj_path(ws: &Workspace, input: &str) -> Result<String, String> {
    Ok(jj_file(&one_path(ws, input)?))
}

/// `file:"<path>"` with the path as a jj string literal. jj's literal grammar accepts raw UTF-8
/// plus the escapes `\" \\ \t \r \n \0 \e \xHH`; Rust's `{:?}` would emit `\u{..}`, which jj rejects.
fn jj_file(path: &str) -> String {
    let mut out = String::from("file:\"");
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\0' => out.push_str("\\0"),
            '\x1b' => out.push_str("\\e"),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn commit_message(args: &Value) -> Result<&str, String> {
    let message = req_str(args, "message")?.trim();
    if message.is_empty() || message.len() > MAX_MESSAGE {
        return Err(format!("commit message must be 1..{MAX_MESSAGE} bytes"));
    }
    Ok(message)
}

/// Keep the exact porcelain output retrievable, but do not flood context with a file inventory.
fn status_output(ws: &Workspace, text: &str) -> String {
    const SAMPLE_PATHS: usize = 20;
    const SAMPLE_CHARS: usize = 240;
    let is_entry = |line: &str| {
        let bytes = line.as_bytes();
        bytes.get(2) == Some(&b' ') && bytes[..2].iter().all(|code| b" MTADRCU?!".contains(code))
    };
    let entries: Vec<_> = text.lines().filter(|line| is_entry(line)).collect();
    if entries.len() <= SAMPLE_PATHS && text.len() <= 4096 {
        return text.to_owned();
    }
    let uri = match ws
        .artifacts()
        .map(|store| store.retain_with_integrity(text))
    {
        Some(Ok((uri, integrity)))
            if matches!(
                integrity.retention,
                crate::runtime::execution::Completeness::Complete
            ) =>
        {
            uri
        }
        Some(Ok(_)) => {
            return format!(
                "{text}\n[status summary bypassed: artifact retention was incomplete; complete status returned inline]"
            );
        }
        Some(Err(error)) => {
            return format!(
                "{text}\n[status summary bypassed: complete status could not be retained: {error:#}; complete status returned inline]"
            );
        }
        None => {
            return format!(
                "{text}\n[status summary bypassed: no artifact store; complete status returned inline]"
            );
        }
    };
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for line in &entries {
        if let Some(code) = line.get(..2) {
            *counts.entry(code).or_default() += 1;
        }
    }
    let mut out = String::new();
    for header in text.lines().filter(|line| line.starts_with("## ")) {
        out.extend(header.chars().take(SAMPLE_CHARS));
        out.push('\n');
    }
    out.push_str(&format!(
        "Status: {} entries (paths; renames count once).\nXY status counts (index/worktree; ??=untracked):",
        entries.len()
    ));
    for (code, count) in counts {
        out.push_str(&format!(" {code:?}={count}"));
    }
    out.push_str(&format!(
        "\nSample paths (first {}):\n",
        entries.len().min(SAMPLE_PATHS)
    ));
    for line in entries.iter().take(SAMPLE_PATHS) {
        out.extend(line.chars().take(SAMPLE_CHARS));
        if line.chars().count() > SAMPLE_CHARS {
            out.push_str("… [path shortened]");
        }
        out.push('\n');
    }
    if text
        .lines()
        .any(|line| !line.starts_with("## ") && !is_entry(line))
    {
        out.push_str("Additional diagnostics are included in complete output.\n");
    }
    out.push_str(&format!(
        "Complete status ({} bytes): {uri}; retrieve with artifact_read.",
        text.len()
    ));
    out.push_str("\nSample only; use git status with `path` to inspect a scope.");
    out
}

fn dispatch(
    ws: &Workspace,
    args: &Value,
    bypass_hooks: bool,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    let action = req_str(args, "action")?;
    let root = ws.root();
    let git = |a: Vec<String>, t: Duration| {
        run(Invocation {
            program: "git",
            args: &a,
            root,
            timeout: t,
            hooks: Hooks::Off,
            envs: &[],
            cancel,
            may_mutate: !READS.contains(&action),
        })
    };
    // Approved repository changes honour hooks/signing unless the operator bypasses them.
    let hooked = if bypass_hooks {
        Hooks::Off
    } else {
        Hooks::Repository
    };
    let git_hooked = |a: Vec<String>, t: Duration| {
        run(Invocation {
            program: "git",
            args: &a,
            root,
            timeout: t,
            hooks: hooked,
            envs: &[],
            cancel,
            may_mutate: !READS.contains(&action),
        })
    };
    let gh = |a: Vec<String>| {
        run(Invocation {
            program: "gh",
            args: &a,
            root,
            timeout: NETWORK_TIMEOUT,
            hooks: Hooks::Off,
            envs: &[],
            cancel,
            may_mutate: !READS.contains(&action),
        })
    };
    let mut note = None;
    let jj = use_jj(ws, args, action)?;
    let (ok, text) = if jj {
        jj_dispatch(ws, args, action, cancel)?
    } else {
        match action {
            "status" => {
                let mut a = strings(&[
                    "status",
                    "--porcelain=v1",
                    "--branch",
                    "--untracked-files=all",
                ]);
                if let Some(path) = opt_str(args, "path")? {
                    a.push("--".into());
                    a.push(one_path(ws, path)?);
                }
                git(a, LOCAL_TIMEOUT)?
            }
            "diff" => {
                let mut a = strings(&["diff", "--no-color", "--no-ext-diff", "--no-textconv"]);
                if opt_bool(args, "staged")? {
                    a.push("--staged".into());
                }
                if let Some(base) = opt_str(args, "base")? {
                    a.push(valid_ref(base)?.to_owned());
                }
                if let Some(path) = opt_str(args, "path")? {
                    a.push("--".into());
                    a.push(one_path(ws, path)?);
                }
                git(a, LOCAL_TIMEOUT)?
            }
            "log" => {
                let n = opt_u64(args, "limit")?.unwrap_or(20).clamp(1, 50);
                let mut a = vec![
                    "log".into(),
                    "--no-color".into(),
                    "--decorate".into(),
                    "--date=short".into(),
                    format!("-n{n}"),
                    "--format=%h %ad %an  %s%d".into(),
                ];
                if let Some(path) = opt_str(args, "path")? {
                    a.push("--".into());
                    a.push(one_path(ws, path)?);
                }
                git(a, LOCAL_TIMEOUT)?
            }
            "show" => git(
                vec![
                    "show".into(),
                    "--no-color".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--stat".into(),
                    "--patch".into(),
                    valid_ref(req_str(args, "rev")?)?.into(),
                ],
                LOCAL_TIMEOUT,
            )?,
            "blame" => {
                let mut a = strings(&["blame", "-w", "--date=short"]);
                let start = opt_u64(args, "start")?;
                let end = opt_u64(args, "end")?;
                match (start, end) {
                    (Some(s), Some(e)) if s >= 1 && e >= s => a.push(format!("-L{s},{e}")),
                    (Some(s), None) if s >= 1 => a.push(format!("-L{s},+40")),
                    (None, Some(e)) if e >= 1 => a.push(format!("-L1,{e}")),
                    (None, None) => {}
                    _ => return Err("blame needs 1 ≤ start ≤ end".into()),
                }
                if let Some(rev) = opt_str(args, "rev")? {
                    a.push(valid_ref(rev)?.to_owned());
                }
                a.push("--".into());
                a.push(one_path(ws, req_str(args, "path")?)?);
                git(a, LOCAL_TIMEOUT)?
            }
            "branches" => git(
                strings(&["branch", "--all", "--verbose", "--no-color"]),
                LOCAL_TIMEOUT,
            )?,
            "add" => {
                let mut a = strings(&["add", "--"]);
                a.extend(paths(ws, args)?);
                git(a, LOCAL_TIMEOUT)?
            }
            "unstage" => {
                let mut a = strings(&["restore", "--staged", "--"]);
                a.extend(paths(ws, args)?);
                git(a, LOCAL_TIMEOUT)?
            }
            "commit" => {
                let message = commit_message(args)?;
                let mut a = strings(&["commit"]);
                if bypass_hooks {
                    a.extend(strings(&["--no-verify", "--no-gpg-sign"]));
                    note = Some(HOOKS_SKIPPED);
                }
                if opt_bool(args, "all")? {
                    a.push("--all".into());
                }
                a.extend(["-m".into(), message.to_owned()]);
                git_hooked(a, LOCAL_TIMEOUT)?
            }
            "branch_create" => {
                let name = valid_ref(req_str(args, "name")?)?;
                let mut a = strings(&["switch", "-c", name]);
                if let Some(from) = opt_str(args, "from")? {
                    a.push(valid_ref(from)?.to_owned());
                }
                git_hooked(a, LOCAL_TIMEOUT)?
            }
            "switch" => git_hooked(
                vec!["switch".into(), valid_ref(req_str(args, "name")?)?.into()],
                LOCAL_TIMEOUT,
            )?,
            "push" => {
                let mut a = strings(&["push"]);
                if bypass_hooks {
                    a.push("--no-verify".into());
                    note = Some(HOOKS_SKIPPED);
                }
                if let Some(remote) = opt_str(args, "remote")? {
                    a.push(valid_ref(remote)?.to_owned());
                } else {
                    a.push("origin".into());
                }
                match opt_str(args, "branch")? {
                    Some(branch) => a.push(valid_ref(branch)?.to_owned()),
                    None => a.push("HEAD".into()),
                }
                git_hooked(a, NETWORK_TIMEOUT)?
            }
            "pr_view" => gh(strings(&["pr", "view", &number(args)?]))?,
            "pr_checks" => gh(strings(&["pr", "checks", &number(args)?]))?,
            "issue_view" => gh(strings(&["issue", "view", &number(args)?, "--comments"]))?,
            "pr_create" => {
                let title = req_str(args, "title")?.trim();
                let body = req_str(args, "body")?;
                if title.is_empty() || title.len() > 256 || body.len() > 64 * 1024 {
                    return Err("pr_create needs a title (≤256 bytes) and a body (≤64 KiB)".into());
                }
                let mut a = strings(&["pr", "create", "--title", title, "--body", body]);
                if let Some(base) = opt_str(args, "base")? {
                    a.extend(["--base".into(), valid_ref(base)?.to_owned()]);
                }
                if opt_bool(args, "draft")? {
                    a.push("--draft".into());
                }
                gh(a)?
            }
            other => return Err(format!("unknown git action {other:?}")),
        }
    };
    let mut text = if ok && action == "status" && !jj {
        status_output(ws, text.trim_end())
    } else {
        clip_with_hint(
            ws,
            text.trim_end().to_owned(),
            "narrow the request with `path`, `limit` or a smaller range",
        )
    };
    if text.is_empty() {
        text = "(no output)".to_owned();
    }
    if let Some(note) = note {
        text.push('\n');
        text.push_str(note);
    }
    Ok(if ok {
        ToolOutput::ok(text)
    } else {
        ToolOutput::err(text)
    })
}

impl Tool for Git {
    fn name(&self) -> &str {
        "git"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "git",
            DESCRIPTION,
            json!({
                "action": {"type": "string", "enum": ["status", "diff", "log", "show", "blame", "branches", "add", "unstage", "commit", "branch_create", "switch", "push", "pr_create", "pr_view", "pr_checks", "issue_view"]},
                "vcs": {"type": "string", "enum": ["git", "jj"], "description": "status/diff/log/commit: force git or jj (default: jj when .jj/ exists)"},
                "start": {"type": "integer", "minimum": 1, "description": "blame: first line"},
                "end": {"type": "integer", "minimum": 1, "description": "blame: last line"},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "add/unstage: workspace paths"},
                "path": {"type": "string", "description": "status/diff/log: limit to this path; blame: file (required)"},
                "staged": {"type": "boolean", "description": "diff: staged changes"},
                "base": {"type": "string", "description": "diff: compare the working tree to this ref; pr_create: base branch"},
                "rev": {"type": "string", "description": "show: commit, tag or ref; blame: blame as of this revision"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "log: commits to show (default 20)"},
                "message": {"type": "string", "description": "commit: message"},
                "all": {"type": "boolean", "description": "commit: include every tracked change"},
                "name": {"type": "string", "description": "branch_create/switch: branch name"},
                "from": {"type": "string", "description": "branch_create: starting ref (default HEAD)"},
                "remote": {"type": "string", "description": "push: remote (default origin)"},
                "branch": {"type": "string", "description": "push: branch (default HEAD)"},
                "title": {"type": "string", "description": "pr_create: title"},
                "body": {"type": "string", "description": "pr_create: description"},
                "draft": {"type": "boolean", "description": "pr_create: open as draft"},
                "number": {"type": "integer", "minimum": 1, "description": "pr_view/pr_checks/issue_view: number"}
            }),
            &["action"],
        )
    }

    fn needs_approval(&self, args: &Value) -> bool {
        !args["action"].as_str().is_some_and(|a| READS.contains(&a))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        dispatch(ctx.workspace, args, self.bypass_hooks, &NEVER_CANCELLED)
            .unwrap_or_else(ToolOutput::err)
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        dispatch(ctx.workspace, args, self.bypass_hooks, cancel).unwrap_or_else(ToolOutput::err)
    }
}

#[cfg(test)]
mod tests;
