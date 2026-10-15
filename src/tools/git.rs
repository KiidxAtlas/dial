//! The `git` tool: structured git, jj (jujutsu) and GitHub (`gh`) operations.
//!
//! Every action builds a fixed argument vector (no shell), validates refs and paths, and runs
//! with fsmonitor, external diff drivers, textconv and pagers switched off, because repository
//! configuration is untrusted. Approved commits/pushes honour the repository's hooks and signing
//! unless `git.bypass_hooks` is set, which the output then says. Reads need no approval;
//! anything that changes the repository or reaches the network does. There is deliberately no
//! force-push, reset, clean, rebase or checkout of paths: those stay with `bash`.
use super::ext::{Tool, ToolContext, ToolEvent};
use super::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

const MAX_OUTPUT: usize = 20 * 1024;
/// Hard ceiling on captured bytes; pipes are still drained past it so git never sees SIGPIPE.
const MAX_CAPTURE: usize = 64 * 1024 * 1024;
const MAX_MESSAGE: usize = 8 * 1024;
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
const HOOKS_SKIPPED: &str =
    "[repository hooks and commit signing were skipped because git.bypass_hooks = true]";

const DESCRIPTION: &str = "Git, jj (jujutsu) and GitHub operations without shell quoting. Read actions: `status`, `diff` \
(optional `staged`, `base` ref, `path`), `log` (`limit` ≤ 50, optional `path`), `show` (`rev`), `blame` (`path`, optional \
`start`/`end` lines, `rev`), `branches`, `pr_view`/`pr_checks`/`issue_view` (`number`; need the `gh` CLI). Changing actions \
(ask approval): `add` (`paths`), `unstage` (`paths`), `commit` (`message`; commits what is staged, or every tracked change \
with `all`), `branch_create` (`name`, optional `from`; switches to it), `switch` (`name`), `push` (optional `remote`, \
`branch`; never forces), `pr_create` (`title`, `body`, optional `base`, `draft`). In a jj repository (`.jj/`) status, diff, \
log and commit use jj unless `vcs` is \"git\"; jj has no staging area. Repository hooks and signing apply to commit/push \
unless the operator set git.bypass_hooks. Long output is cut with the full text kept as an artifact:// link. For reset, \
rebase, force-push or anything else, use bash.";

/// `[git]` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// Skip repository hooks (`--no-verify`, empty hooksPath) on commit/push in the agent git
    /// tool, the `/git` panel, and `dial commit`. The agent tool also skips commit signing, since it
    /// cannot answer a pinentry prompt; user-initiated commits keep the configured signing.
    pub bypass_hooks: bool,
}

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

/// Cut long output for the model; the complete text goes to an artifact when a store exists.
fn clip(text: String, ws: &Workspace) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let end = (0..=MAX_OUTPUT)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let full = match ws.artifacts().map(|store| store.retain(&text)) {
        Some(Ok(uri)) => format!("full output ({} bytes): {uri}", text.len()),
        Some(Err(error)) => format!("full output could not be retained: {error:#}"),
        None => "narrow the request with `path`, `limit` or a smaller range".to_owned(),
    };
    format!(
        "{}\n[output cut at {MAX_OUTPUT} bytes; {full}]",
        &text[..end]
    )
}

/// Hook/signing-sensitive invocation: whether repository hooks may run for this call.
#[derive(Clone, Copy, PartialEq)]
enum Hooks {
    Off,
    Repository,
}

/// `-c` overrides that point `core.hooksPath` at the null device, so repository hooks do not run.
/// Shared by the agent tool, the `/git` panel, and `dial commit` when `git.bypass_hooks` is set.
pub(crate) fn hooks_off_args() -> [&'static str; 2] {
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

/// Run `program` with a bounded time. Returns (success, stdout, stderr), each complete up to
/// `MAX_CAPTURE`; callers clip for display.
fn run_streams(
    program: &str,
    args: &[String],
    root: &Path,
    timeout: Duration,
    hooks: Hooks,
    envs: &[(&str, &std::ffi::OsStr)],
    cancel: &AtomicBool,
    may_mutate: bool,
) -> Result<(bool, String, String), String> {
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
    Ok((
        output.success(),
        output.stdout_text(),
        output.text(crate::runtime::process::Stream::Stderr),
    ))
}

/// Run `program` with a bounded time. Returns (success, stdout+stderr combined), complete up to
/// `MAX_CAPTURE`; callers clip for display.
fn run(
    program: &str,
    args: &[String],
    root: &Path,
    timeout: Duration,
    hooks: Hooks,
    envs: &[(&str, &std::ffi::OsStr)],
    cancel: &AtomicBool,
    may_mutate: bool,
) -> Result<(bool, String), String> {
    let (ok, out, err) = run_streams(
        program, args, root, timeout, hooks, envs, cancel, may_mutate,
    )?;
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
    run_streams(
        "git",
        args,
        root,
        LOCAL_TIMEOUT,
        hooks,
        envs,
        cancel,
        mutating,
    )
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worktree {
    pub(crate) path: PathBuf,
    /// Short branch name with `refs/heads/` stripped; `None` when detached or bare.
    pub(crate) branch: Option<String>,
    pub(crate) head: String,
    pub(crate) bare: bool,
    pub(crate) detached: bool,
    pub(crate) locked: bool,
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
pub(crate) fn worktree_list(root: &Path) -> Result<Vec<Worktree>, String> {
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
/// patch, and untracked files are copied. Returns whether anything was carried over.
pub(crate) fn worktree_add_keep_changes(
    root: &Path,
    path: &Path,
    branch: &str,
) -> Result<bool, String> {
    worktree_add(root, path, branch, "HEAD")?;
    match carry_changes(root, path, branch) {
        Ok(carried) => Ok(carried),
        Err(error) => {
            // Roll back the registered worktree and its new branch so a retry is not blocked.
            let _ = worktree_remove(root, path, true);
            let _ = run_checked(root, &["branch", "-D", branch], true);
            Err(error)
        }
    }
}

fn carry_changes(root: &Path, path: &Path, branch: &str) -> Result<bool, String> {
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
        let patch_file = std::env::temp_dir().join(format!(
            "dial-wt-{}-{}.patch",
            std::process::id(),
            hash_branch(branch)
        ));
        std::fs::write(&patch_file, patch.as_bytes()).map_err(|e| e.to_string())?;
        let applied = run_checked(
            path,
            &["apply", "--binary", &patch_file.to_string_lossy()],
            true,
        );
        let _ = std::fs::remove_file(&patch_file);
        applied?;
    }
    for name in untracked {
        let (from, to) = (top.join(name), path.join(name));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&from, &to).map_err(|e| format!("copying {name}: {e}"))?;
    }
    Ok(true)
}

fn hash_branch(branch: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    branch.hash(&mut hasher);
    hasher.finish()
}

/// Remove a registered worktree. A dirty or locked worktree is refused unless `force`.
pub(crate) fn worktree_remove(root: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut args = strings(&["worktree", "remove"]);
    if force {
        args.push("--force".to_owned());
    }
    args.push(path.to_string_lossy().into_owned());
    git_checked(root, &args, true).map(drop)
}

/// Worktree location for `branch`: `~/.dial/wt/<hash of root>/<branch with / as ->`.
pub(crate) fn worktree_path_for(root: &Path, branch: &str) -> Result<PathBuf, String> {
    let base = super::gh_pr::worktrees_root()?;
    let hash = super::gh_pr::hash_path(root);
    Ok(base.join(hash).join(branch.replace('/', "-")))
}

/// Run a git command and return stdout, or git's stderr on failure.
pub(crate) fn run_checked(root: &Path, args: &[&str], mutating: bool) -> Result<String, String> {
    git_checked(root, &strings(args), mutating)
}

/// An independent baseline of the filtered development copy: no parent gitdir, remotes,
/// hooks, credential config, alternates or hardlinked objects are inherited.
pub(crate) fn initialize_private_snapshot(root: &Path) -> Result<(), String> {
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
pub(crate) fn clean_source_checkout(root: &Path) -> Result<(), String> {
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
        run(
            "jj",
            &a,
            root,
            LOCAL_TIMEOUT,
            Hooks::Off,
            &[],
            cancel,
            !READS.contains(&action),
        )
    };
    match action {
        "status" => jj(strings(&["status"])),
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
                a.extend(paths(ws, args)?.into_iter().map(|p| format!("file:{p:?}")));
            }
            jj(a)
        }
        _ => unreachable!("use_jj limits actions"),
    }
}

/// A workspace path as a literal jj fileset (`file:"..."`), never a revset or option.
fn jj_path(ws: &Workspace, input: &str) -> Result<String, String> {
    Ok(format!("file:{:?}", one_path(ws, input)?))
}

fn commit_message(args: &Value) -> Result<&str, String> {
    let message = req_str(args, "message")?.trim();
    if message.is_empty() || message.len() > MAX_MESSAGE {
        return Err(format!("commit message must be 1..{MAX_MESSAGE} bytes"));
    }
    Ok(message)
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
        run(
            "git",
            &a,
            root,
            t,
            Hooks::Off,
            &[],
            cancel,
            !READS.contains(&action),
        )
    };
    // Approved repository changes honour hooks/signing unless the operator bypasses them.
    let hooked = if bypass_hooks {
        Hooks::Off
    } else {
        Hooks::Repository
    };
    let git_hooked = |a: Vec<String>, t: Duration| {
        run(
            "git",
            &a,
            root,
            t,
            hooked,
            &[],
            cancel,
            !READS.contains(&action),
        )
    };
    let gh = |a: Vec<String>| {
        run(
            "gh",
            &a,
            root,
            NETWORK_TIMEOUT,
            Hooks::Off,
            &[],
            cancel,
            !READS.contains(&action),
        )
    };
    let mut note = None;
    let (ok, text) = if use_jj(ws, args, action)? {
        jj_dispatch(ws, args, action, cancel)?
    } else {
        match action {
            "status" => git(strings(&["status", "--short", "--branch"]), LOCAL_TIMEOUT)?,
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
    let mut text = clip(text.trim_end().to_owned(), ws);
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
                "path": {"type": "string", "description": "diff/log: limit to this path; blame: file (required)"},
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
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "{args:?}"
            )
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.name", "t"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        sh(&["add", "a.txt"]);
        sh(&["commit", "-q", "-m", "init", "--no-verify"]);
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    fn call(ws: &Workspace, args: Value) -> ToolOutput {
        dispatch(ws, &args, false, &NEVER_CANCELLED).unwrap_or_else(ToolOutput::err)
    }

    #[test]
    fn read_and_write_flow() {
        let (dir, ws) = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();
        assert!(call(&ws, json!({"action":"status"})).text.contains("a.txt"));
        assert!(
            call(&ws, json!({"action":"diff","path":"a.txt"}))
                .text
                .contains("+two")
        );
        assert!(!call(&ws, json!({"action":"add","paths":["a.txt","b.txt"]})).is_error);
        assert!(
            call(&ws, json!({"action":"diff","staged":true}))
                .text
                .contains("+new")
        );
        assert!(!call(&ws, json!({"action":"unstage","paths":["b.txt"]})).is_error);
        let commit = call(&ws, json!({"action":"commit","message":"second"}));
        assert!(!commit.is_error, "{}", commit.text);
        assert!(
            call(&ws, json!({"action":"log","limit":5}))
                .text
                .contains("second")
        );
        assert!(
            call(&ws, json!({"action":"show","rev":"HEAD"}))
                .text
                .contains("second")
        );
        assert!(
            call(&ws, json!({"action":"status"})).text.contains("b.txt"),
            "unstaged file stays out of the commit"
        );
        assert!(!call(&ws, json!({"action":"branch_create","name":"feature/x"})).is_error);
        assert!(
            call(&ws, json!({"action":"branches"}))
                .text
                .contains("feature/x")
        );
        assert!(!call(&ws, json!({"action":"switch","name":"main"})).is_error);
    }

    #[test]
    fn hostile_arguments_are_refused() {
        let (_dir, ws) = repo();
        for rev in ["--output=/tmp/x", "-p", "a..b", "x y", ""] {
            assert!(
                call(&ws, json!({"action":"show","rev":rev})).is_error,
                "{rev}"
            );
        }
        assert!(call(&ws, json!({"action":"add","paths":["../outside"]})).is_error);
        assert!(
            call(&ws, json!({"action":"add","paths":["--all"]})).is_error,
            "an option-looking path is treated as a path, which does not exist"
        );
        assert!(call(&ws, json!({"action":"push","remote":"--upload-pack=x"})).is_error);
        assert!(call(&ws, json!({"action":"reset"})).is_error);
        assert!(call(&ws, json!({"action":"commit","message":"  "})).is_error);
    }

    #[test]
    fn hooks_run_by_default_and_bypass_is_reported() {
        let (dir, ws) = repo();
        let hook = dir.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
        crate::platform::fs::set_mode(&hook, 0o755).unwrap();
        std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
        assert!(!call(&ws, json!({"action":"add","paths":["c.txt"]})).is_error);
        let blocked = call(&ws, json!({"action":"commit","message":"x"}));
        assert!(
            blocked.is_error,
            "the failing pre-commit hook must block: {}",
            blocked.text
        );
        assert!(
            dir.path().join("hook-ran").exists(),
            "repository hook should run by default"
        );
        std::fs::remove_file(dir.path().join("hook-ran")).unwrap();
        let bypassed = dispatch(
            &ws,
            &json!({"action":"commit","message":"x"}),
            true,
            &NEVER_CANCELLED,
        )
        .unwrap();
        assert!(!bypassed.is_error, "{}", bypassed.text);
        assert!(
            bypassed.text.contains("git.bypass_hooks"),
            "{}",
            bypassed.text
        );
        assert!(
            !dir.path().join("hook-ran").exists(),
            "bypass must skip the hook"
        );
        // Reads never run hooks.
        assert!(!call(&ws, json!({"action":"status"})).is_error);
        assert!(!dir.path().join("hook-ran").exists());
        for (action, asks) in [
            ("status", false),
            ("diff", false),
            ("blame", false),
            ("pr_view", false),
            ("commit", true),
            ("push", true),
            ("add", true),
            ("pr_create", true),
        ] {
            assert_eq!(
                Git::default().needs_approval(&json!({"action":action})),
                asks,
                "{action}"
            );
        }
    }

    #[test]
    fn large_output_drains_fully_and_spills_to_an_artifact() {
        let (dir, ws) = repo();
        let state = tempfile::tempdir().unwrap();
        let ws =
            ws.with_artifacts(crate::tools::artifact::ArtifactStore::new(state.path()).unwrap());
        // Far beyond the pipe buffer and the old 4×cap read limit that caused SIGPIPE failures.
        let big: String = (0..60_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        assert!(!call(&ws, json!({"action":"add","paths":["big.txt"]})).is_error);
        let commit = dispatch(
            &ws,
            &json!({"action":"commit","message":"big"}),
            true,
            &NEVER_CANCELLED,
        )
        .unwrap();
        assert!(!commit.is_error, "{}", commit.text);
        let show = call(&ws, json!({"action":"show","rev":"HEAD"}));
        assert!(
            !show.is_error,
            "large output must not fail: {}",
            &show.text[show.text.len().saturating_sub(300)..]
        );
        assert!(show.text.len() < MAX_OUTPUT + 1024);
        let uris = crate::tools::artifact::uris(&show.text);
        assert_eq!(
            uris.len(),
            1,
            "{}",
            &show.text[show.text.len().saturating_sub(300)..]
        );
        let probe: Value = serde_json::from_str(
            &ws.artifacts()
                .unwrap()
                .retrieve_bytes(uris[0], 0, 1)
                .unwrap(),
        )
        .unwrap();
        let total = probe["total_bytes"].as_u64().unwrap();
        assert!(
            total > big.len() as u64 + 60_000,
            "artifact must hold the complete output, got {total} bytes"
        );
    }

    #[test]
    fn blame_reports_line_authors() {
        let (_dir, ws) = repo();
        let blame = call(
            &ws,
            json!({"action":"blame","path":"a.txt","start":1,"end":1}),
        );
        assert!(!blame.is_error, "{}", blame.text);
        assert!(blame.text.contains("one"), "{}", blame.text);
        assert!(
            call(
                &ws,
                json!({"action":"blame","path":"a.txt","start":3,"end":1})
            )
            .is_error
        );
    }

    #[test]
    fn parses_porcelain_worktree_blocks() {
        let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /r-wt\nHEAD bbb\ndetached\nlocked reason\n\nworktree /bare\nbare\n";
        let trees = parse_worktrees(text);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0].path, PathBuf::from("/r"));
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert_eq!(trees[0].head, "aaa");
        assert!(trees[1].detached && trees[1].locked);
        assert_eq!(trees[1].branch, None);
        assert!(trees[2].bare);
    }

    #[test]
    fn worktree_add_list_remove_round_trip() {
        let (_dir, ws) = repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("wt");
        worktree_add(ws.root(), &wt, "feat/x", "HEAD").unwrap();
        let trees = worktree_list(ws.root()).unwrap();
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert!(trees.iter().any(|t| t.branch.as_deref() == Some("feat/x")));
        worktree_remove(ws.root(), &wt, false).unwrap();
        assert_eq!(worktree_list(ws.root()).unwrap().len(), 1);
        assert!(!wt.exists());
    }

    #[test]
    fn worktree_remove_refuses_dirty_without_force() {
        let (_dir, ws) = repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("dirty");
        worktree_add(ws.root(), &wt, "dirty", "HEAD").unwrap();
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        assert!(worktree_remove(ws.root(), &wt, false).is_err());
        worktree_remove(ws.root(), &wt, true).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn worktree_add_keep_changes_carries_tracked_and_untracked_edits() {
        let (_dir, ws) = repo();
        std::fs::write(ws.root().join("a.txt"), "edited\n").unwrap();
        std::fs::write(ws.root().join("new.txt"), "fresh\n").unwrap();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("kept");
        assert!(worktree_add_keep_changes(ws.root(), &wt, "kept").unwrap());
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).unwrap(),
            "edited\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("new.txt")).unwrap(),
            "fresh\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.root().join("a.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn clean_source_checkout_reverts_tracked_edits_and_removes_untracked_files() {
        let (_dir, ws) = repo();
        std::fs::write(ws.root().join("a.txt"), "edited\n").unwrap();
        std::fs::write(ws.root().join("new.txt"), "fresh\n").unwrap();
        clean_source_checkout(ws.root()).unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.root().join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(!ws.root().join("new.txt").exists());
    }
}
