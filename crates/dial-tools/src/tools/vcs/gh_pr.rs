//! GitHub PR mutation ops: `pr_create`, `pr_checkout` and `pr_push`. `gh` runs through
//! `gh_common`. Git runs as a fixed argv with the repository-config hardening used by
//! `git.rs`; hooks stay enabled.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::gh_common::{GH_TIMEOUT, gh, gh_json, strings, valid_repo};
use crate::tools::{ToolOutput, Workspace, opt_bool, opt_str};

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Fetch and push run under a 30 minute network budget.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Worktree attempts per PR: the base name, then `-2` through `-100`.
const MAX_WORKTREE_SUFFIX: usize = 100;

const PR_VIEW_FIELDS: &str = "author,baseRefName,body,createdAt,files,headRefName,isDraft,labels,mergeStateStatus,number,reviews,reviewDecision,state,title,updatedAt,url";
const PR_CHECKOUT_FIELDS: &str = "baseRefName,headRefName,headRefOid,headRepository,headRepositoryOwner,isCrossRepository,maintainerCanModify,number,title,url";

// Keep PR-test executable/home overrides local to the thread constructing the operation. Mutating
// process PATH/HOME races unrelated Git, browser and configuration tests, even with a writer lock.
#[cfg(test)]
thread_local! {
    static TEST_ENV: std::cell::RefCell<Option<(PathBuf, PathBuf)>> = const { std::cell::RefCell::new(None) };
}

pub(in crate::tools) fn run(
    op: &str,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    match op {
        "pr_create" => pr_create(ws, args, cancel),
        "pr_checkout" => pr_checkout(ws, args, cancel),
        "pr_push" => pr_push(ws, args, cancel),
        other => Err(format!("unknown PR operation `{other}`")),
    }
}

// ───────────────────────────── runners ─────────────────────────────

/// Workspace and cancellation handles shared by one op.
struct Env<'a> {
    ws: &'a Workspace,
    cancel: &'a AtomicBool,
}

struct GitRun {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Env<'_> {
    fn root(&self) -> &Path {
        self.ws.root()
    }

    /// Run `git` with a fixed argv in the workspace root. Never uses a shell.
    fn git(&self, args: &[&str], timeout: Duration) -> Result<GitRun, String> {
        let mut command = Command::new("git");
        #[cfg(test)]
        if let Some((bin, home)) = TEST_ENV.with(|value| value.borrow().clone()) {
            let fake = bin.join("git");
            if fake.is_file() {
                command = Command::new(fake);
            }
            command.env("HOME", home);
        }
        // Repository config is untrusted: no fsmonitor, pager, external diff or ext transport.
        command
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.pager=cat",
                "-c",
                "diff.external=",
                "-c",
                "protocol.ext.allow=never",
                "--no-pager",
            ])
            .args(args)
            .current_dir(self.root())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::runtime::process::run(
            command,
            None,
            timeout,
            64 * 1024 * 1024,
            self.cancel,
            &mut |_, _| {},
        )?;
        if output.cancelled || output.timed_out || output.io_error.is_some() {
            return Err(output.failure("git", timeout));
        }
        Ok(GitRun {
            success: output.success(),
            stdout: output.stdout_text(),
            stderr: output.text(crate::runtime::process::Stream::Stderr),
        })
    }

    /// Run git and return stdout on success; otherwise an error carrying the git detail.
    fn git_checked(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let run = self.git(args, timeout)?;
        if run.success {
            return Ok(run.stdout);
        }
        let detail = if run.stderr.trim().is_empty() {
            run.stdout.trim()
        } else {
            run.stderr.trim()
        };
        Err(format!("git {} failed: {detail}", args.join(" ")))
    }

    fn git_text(&self, args: &[&str]) -> Result<String, String> {
        self.git_checked(args, GIT_TIMEOUT)
    }

    fn git_succeeds(&self, args: &[&str]) -> Result<bool, String> {
        Ok(self.git(args, GIT_TIMEOUT)?.success)
    }

    /// `git config --get <key>`: `None` when the key is unset or empty.
    fn config_get(&self, key: &str) -> Result<Option<String>, String> {
        let run = self.git(&["config", "--get", key], GIT_TIMEOUT)?;
        Ok(run
            .success
            .then(|| run.stdout.trim().to_owned())
            .filter(|value| !value.is_empty()))
    }

    fn config_set(&self, key: &str, value: &str) -> Result<(), String> {
        self.git_text(&["config", key, value]).map(|_| ())
    }

    fn remote_names(&self) -> Result<Vec<String>, String> {
        Ok(self
            .git_text(&["remote"])?
            .lines()
            .filter_map(trimmed)
            .collect())
    }

    fn gh_text(&self, args: &[String]) -> Result<String, String> {
        gh(self.ws, args, GH_TIMEOUT, self.cancel)
    }

    fn gh_json(&self, args: &[String]) -> Result<Value, String> {
        gh_json(self.ws, args, GH_TIMEOUT, self.cancel)
    }
}

// ───────────────────────────── arg helpers ─────────────────────────────

fn trimmed(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Optional `repo`, validated as `owner/name`.
fn opt_repo(args: &Value) -> Result<Option<String>, String> {
    match opt_str(args, "repo")?.and_then(trimmed) {
        Some(repo) => Ok(Some(valid_repo(&repo)?.to_owned())),
        None => Ok(None),
    }
}

/// Non-empty trimmed strings from a string array argument; empties are dropped.
fn list_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(trimmed)
        .collect()
}

/// `pr` accepts a single string or a string array.
fn pr_refs(args: &Value) -> Vec<String> {
    match args.get("pr") {
        Some(Value::String(pr)) => trimmed(pr).into_iter().collect(),
        Some(Value::Array(_)) => list_arg(args, "pr"),
        _ => Vec::new(),
    }
}

/// Pass `--repo` unless the PR reference is already a URL (which names its own repository).
fn append_repo_flag(args: &mut Vec<String>, repo: Option<&str>, pr_ref: &str) {
    if let Some(repo) = repo
        && !pr_ref.starts_with("https://")
    {
        args.push("--repo".to_owned());
        args.push(repo.to_owned());
    }
}

fn push_line(lines: &mut Vec<String>, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        lines.push(format!("{label}: {value}"));
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().and_then(trimmed)
}

fn nested_str(value: &Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))?
        .as_str()
        .and_then(trimmed)
}

fn required_str(value: &Value, key: &str, label: &str) -> Result<String, String> {
    str_field(value, key).ok_or_else(|| format!("GitHub CLI did not return a {label}."))
}

fn short_sha(oid: &str) -> String {
    oid.chars().take(12).collect()
}

fn format_author(author: &Value) -> Option<String> {
    str_field(author, "login")
        .map(|login| format!("@{login}"))
        .or_else(|| str_field(author, "name"))
}

fn format_labels(labels: &Value) -> Option<String> {
    let names: Vec<String> = labels
        .as_array()?
        .iter()
        .filter_map(|label| str_field(label, "name"))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

// ───────────────────────────── PR URLs ─────────────────────────────

struct PrUrl {
    host: String,
    /// `host/owner/name`, as parsed from the PR URL (the host is kept).
    repo: String,
    number: u64,
}

/// Match `https://<host>/<owner>/<name>/pull/<n>[/...]`.
fn parse_pull_request_url(url: &str) -> Option<PrUrl> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let parts: Vec<&str> = path.split('/').collect();
    if host.is_empty()
        || parts.len() < 4
        || parts[0].is_empty()
        || parts[1].is_empty()
        || parts[2] != "pull"
        || parts[3].is_empty()
        || !parts[3].bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(PrUrl {
        host: host.to_owned(),
        repo: format!("{host}/{}/{}", parts[0], parts[1]),
        number: parts[3].parse().ok()?,
    })
}

// ───────────────────────────── pr_create ─────────────────────────────

fn pr_create(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let env = Env { ws, cancel };
    let repo = opt_repo(args)?;
    let title = opt_str(args, "title")?.and_then(trimmed);
    let body = opt_str(args, "body")?;
    let base = opt_str(args, "base")?.and_then(trimmed);
    let head = opt_str(args, "head")?.and_then(trimmed);
    let draft = opt_bool(args, "draft")?;
    let fill = opt_bool(args, "fill")?;
    if !fill && title.is_none() {
        return Err("title is required unless fill is true".to_owned());
    }
    if fill && (title.is_some() || body.is_some_and(|text| !text.is_empty())) {
        return Err("fill is mutually exclusive with title and body".to_owned());
    }

    let mut argv = strings(&["pr", "create"]);
    append_repo_flag(&mut argv, repo.as_deref(), "");
    for (flag, value) in [
        ("--title", title.as_deref()),
        ("--base", base.as_deref()),
        ("--head", head.as_deref()),
    ] {
        if let Some(value) = value {
            argv.push(flag.to_owned());
            argv.push(value.to_owned());
        }
    }
    if draft {
        argv.push("--draft".to_owned());
    }
    if fill {
        argv.push("--fill".to_owned());
    }
    for reviewer in list_arg(args, "reviewer") {
        argv.push("--reviewer".to_owned());
        argv.push(reviewer);
    }
    for assignee in list_arg(args, "assignee") {
        argv.push("--assignee".to_owned());
        argv.push(assignee);
    }
    for label in list_arg(args, "label") {
        argv.push("--label".to_owned());
        argv.push(label);
    }
    // The body file must outlive the `gh` call; dropping it removes the directory.
    let _body_dir = match (fill, body) {
        (false, Some(text)) if !text.is_empty() => {
            let dir = tempfile::Builder::new()
                .prefix("gh-pr-body-")
                .tempdir()
                .map_err(|e| format!("could not create PR body file: {e}"))?;
            let path = dir.path().join("body.md");
            fs::write(&path, text).map_err(|e| format!("could not write PR body file: {e}"))?;
            argv.push("--body-file".to_owned());
            argv.push(path.to_string_lossy().into_owned());
            Some(dir)
        }
        (false, _) => {
            argv.push("--body".to_owned());
            argv.push(String::new());
            None
        }
        (true, _) => None,
    };

    let created = env.gh_text(&argv)?;
    let url = created
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("https://"))
        .map_or_else(|| created.trim().to_owned(), str::to_owned);
    let pr = parse_pull_request_url(&url);
    let resolved_repo = repo.or_else(|| pr.as_ref().map(|pr| pr.repo.clone()));

    // Best effort: the created PR's details enrich the report; failures are ignored.
    let view = match (&resolved_repo, &pr) {
        (Some(repo_ref), Some(pr)) => {
            let number = pr.number.to_string();
            let view_args = strings(&[
                "pr",
                "view",
                number.as_str(),
                "--repo",
                repo_ref.as_str(),
                "--json",
                PR_VIEW_FIELDS,
            ]);
            env.gh_json(&view_args).ok()
        }
        _ => None,
    };
    let view = view.as_ref();
    let number = view
        .and_then(|v| v.get("number"))
        .and_then(Value::as_u64)
        .or(pr.as_ref().map(|pr| pr.number));
    let title_text = view
        .and_then(|v| str_field(v, "title"))
        .or(title)
        .unwrap_or_else(|| "Untitled".to_owned());
    let header = match number {
        Some(number) => format!("# Created Pull Request #{number}: {title_text}"),
        None => format!("# Created Pull Request: {title_text}"),
    };

    let mut lines = Vec::new();
    push_line(
        &mut lines,
        "URL",
        view.and_then(|v| str_field(v, "url"))
            .or_else(|| Some(url.clone())),
    );
    push_line(
        &mut lines,
        "State",
        view.and_then(|v| str_field(v, "state")),
    );
    push_line(
        &mut lines,
        "Draft",
        view.and_then(|v| v.get("isDraft"))
            .and_then(Value::as_bool)
            .map(|draft| draft.to_string()),
    );
    push_line(
        &mut lines,
        "Base",
        view.and_then(|v| str_field(v, "baseRefName")),
    );
    push_line(
        &mut lines,
        "Head",
        view.and_then(|v| str_field(v, "headRefName")),
    );
    push_line(
        &mut lines,
        "Author",
        view.and_then(|v| v.get("author")).and_then(format_author),
    );
    push_line(
        &mut lines,
        "Created",
        view.and_then(|v| str_field(v, "createdAt")),
    );
    push_line(
        &mut lines,
        "Labels",
        view.and_then(|v| v.get("labels")).and_then(format_labels),
    );

    let mut out = vec![header, String::new()];
    out.extend(lines);
    if let Some(body_text) = view.and_then(|v| str_field(v, "body")) {
        out.push(String::new());
        out.push("## Body".to_owned());
        out.push(body_text);
    }
    let data = json!({
        "op": "pr_create",
        "url": url,
        "sourceUrl": url,
        "number": number,
        "repo": resolved_repo,
    });
    Ok(ToolOutput {
        execution: Default::default(),
        text: out.join("\n"),
        is_error: false,
        data: Some(data),
    })
}

// ───────────────────────────── pr_checkout ─────────────────────────────

/// Worktree and branch facts from `git worktree list --porcelain`.
struct Worktree {
    path: PathBuf,
    branch: Option<String>,
}

struct Checkout {
    number: u64,
    title: Option<String>,
    url: String,
    base: Option<String>,
    head_ref: String,
    local_branch: String,
    worktree: PathBuf,
    remote: String,
    remote_url: String,
    cross: bool,
    maintainer_can_modify: bool,
    reused: bool,
}

fn pr_checkout(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let repo = opt_repo(args)?;
    let refs = pr_refs(args);
    if refs.is_empty() {
        return Err("pr is required for pr_checkout".to_owned());
    }
    let force = opt_bool(args, "force")?;
    let env = Env { ws, cancel };
    let mut checkouts = Vec::new();
    let mut failures = Vec::new();
    for pr_ref in &refs {
        match checkout_pull_request(&env, repo.as_deref(), pr_ref, force) {
            Ok(checkout) => checkouts.push(checkout),
            Err(error) => failures.push((pr_ref.clone(), error)),
        }
    }
    render_checkouts(refs.len(), &checkouts, &failures)
}

fn checkout_pull_request(
    env: &Env,
    repo: Option<&str>,
    pr_ref: &str,
    force: bool,
) -> Result<Checkout, String> {
    if pr_ref.starts_with('-') {
        return Err(format!(
            "invalid PR identifier: {pr_ref}. Pass a PR number, URL, or branch name."
        ));
    }
    let mut view_args = strings(&["pr", "view"]);
    view_args.push(pr_ref.to_owned());
    append_repo_flag(&mut view_args, repo, pr_ref);
    view_args.push("--json".to_owned());
    view_args.push(PR_CHECKOUT_FIELDS.to_owned());
    let view = env.gh_json(&view_args)?;

    let number = view
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "GitHub CLI did not return a pull request number.".to_owned())?;
    let head_ref = required_str(&view, "headRefName", "head branch")?;
    let head_oid = required_str(&view, "headRefOid", "head commit")?;
    let url = str_field(&view, "url").unwrap_or_default();
    let title = str_field(&view, "title");
    let base = str_field(&view, "baseRefName");
    let cross = view
        .get("isCrossRepository")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let maintainer = view
        .get("maintainerCanModify")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let local = format!("pr-{number}");
    let full = format!("refs/heads/{local}");

    let trees = list_worktrees(env)?;
    let existing = trees
        .iter()
        .find(|tree| tree.branch.as_deref() == Some(full.as_str()))
        .map(|tree| tree.path.clone());
    let (remote, remote_url) = ensure_pr_remote(env, &view, cross, &url)?;
    let refspec = format!("+refs/heads/{head_ref}:refs/remotes/{remote}/{head_ref}");
    env.git_checked(
        &["fetch", remote.as_str(), refspec.as_str()],
        NETWORK_TIMEOUT,
    )?;

    let remote_ref = format!("refs/remotes/{remote}/{head_ref}");
    if let Some(path) = &existing {
        let path_text = path.to_string_lossy().into_owned();
        env.git_checked(
            &[
                "-C",
                path_text.as_str(),
                "merge",
                "--ff-only",
                remote_ref.as_str(),
            ],
            NETWORK_TIMEOUT,
        )?;
    } else {
        ensure_local_branch(env, &local, &full, &remote_ref, &head_oid, force)?;
    }

    let merge = format!("refs/heads/{head_ref}");
    let cross_text = cross.to_string();
    let maintainer_text = maintainer.to_string();
    for (key, value) in [
        ("remote", remote.as_str()),
        ("merge", merge.as_str()),
        ("pushRemote", remote.as_str()),
        ("dialPrHeadRef", head_ref.as_str()),
        ("dialPrUrl", url.as_str()),
        ("dialPrIsCrossRepository", cross_text.as_str()),
        ("dialPrMaintainerCanModify", maintainer_text.as_str()),
    ] {
        env.config_set(&format!("branch.{local}.{key}"), value)?;
    }

    let (worktree, reused) = match existing {
        Some(path) => (path, true),
        None => (add_worktree(env, &trees, number, &local)?, false),
    };
    Ok(Checkout {
        number,
        title,
        url,
        base,
        head_ref,
        local_branch: local,
        worktree,
        remote,
        remote_url,
        cross,
        maintainer_can_modify: maintainer,
        reused,
    })
}

/// Reuse, reset (`force`), or create `pr-<n>` at the fetched PR head.
fn ensure_local_branch(
    env: &Env,
    local: &str,
    full: &str,
    remote_ref: &str,
    head_oid: &str,
    force: bool,
) -> Result<(), String> {
    if !env.git_succeeds(&["show-ref", "--verify", "--quiet", full])? {
        return env.git_text(&["branch", local, remote_ref]).map(|_| ());
    }
    let current = env.git_text(&["rev-parse", "--verify", full])?;
    let current = current.trim();
    if current == head_oid {
        return Ok(());
    }
    if !force {
        return Err(format!(
            "local branch {local} already exists at {} ; pass force=true to reset it",
            short_sha(current)
        ));
    }
    env.git_text(&["branch", "-f", local, remote_ref])
        .map(|_| ())
}

/// Pick the remote that points at the PR head: `origin` for same-repo PRs, otherwise a fork
/// remote (reusing one whose URL already matches).
fn ensure_pr_remote(
    env: &Env,
    view: &Value,
    cross: bool,
    url: &str,
) -> Result<(String, String), String> {
    let remotes = env.remote_names()?;
    if !cross {
        if !remotes.iter().any(|name| name == "origin") {
            return Err("origin remote is unavailable for this repository.".to_owned());
        }
        let origin_url = env.config_get("remote.origin.url")?.unwrap_or_default();
        return Ok(("origin".to_owned(), origin_url));
    }

    let (Some(owner), Some(name)) = (
        nested_str(view, &["headRepositoryOwner", "login"]),
        nested_str(view, &["headRepository", "name"]),
    ) else {
        return Err(
            "Could not determine a clone URL for the pull request head repository.".to_owned(),
        );
    };
    let head_repo = format!("{owner}/{name}");
    let spec = match parse_pull_request_url(url) {
        Some(pr) => format!("{}/{head_repo}", pr.host),
        None => head_repo.clone(),
    };
    let info = env.gh_json(&strings(&[
        "repo",
        "view",
        spec.as_str(),
        "--json",
        "nameWithOwner,sshUrl,url",
    ]))?;
    let https = str_field(&info, "url");
    let ssh = str_field(&info, "sshUrl");
    let origin_url = env.config_get("remote.origin.url")?.unwrap_or_default();
    let (preferred, fallback) =
        if origin_url.starts_with("https://") || origin_url.starts_with("http://") {
            (https, ssh)
        } else {
            (ssh, https)
        };
    let clone_url = preferred
        .or(fallback)
        .ok_or_else(|| format!("Could not determine a clone URL for {head_repo}."))?;

    for name in &remotes {
        if env.config_get(&format!("remote.{name}.url"))?.as_deref() == Some(clone_url.as_str()) {
            return Ok((name.clone(), clone_url));
        }
    }
    let base = sanitize_remote_name(&owner)
        .map_or_else(|| "fork".to_owned(), |slug| format!("fork-{slug}"));
    let mut name = base.clone();
    let mut suffix = 2;
    while remotes.contains(&name) {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    env.git_text(&["remote", "add", name.as_str(), clone_url.as_str()])?;
    Ok((name, clone_url))
}

/// Keep ASCII letters, digits, `-`, `_` and `.`; other characters become `-`.
fn sanitize_remote_name(owner: &str) -> Option<String> {
    let slug: String = owner
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|c| c == '-' || c == '.');
    (!slug.is_empty()).then(|| slug.to_owned())
}

fn list_worktrees(env: &Env) -> Result<Vec<Worktree>, String> {
    env.git_text(&["worktree", "prune"])?;
    let out = env.git_text(&["worktree", "list", "--porcelain"])?;
    let mut trees = Vec::new();
    for block in out.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_owned());
            }
        }
        if let Some(path) = path {
            trees.push(Worktree { path, branch });
        }
    }
    Ok(trees)
}

/// The primary checkout: the parent of the shared `.git` directory.
fn primary_root(env: &Env) -> Result<PathBuf, String> {
    let common = env.git_text(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common.trim());
    Ok(common.parent().map(Path::to_path_buf).unwrap_or(common))
}

/// `~/.dial/wt`, the worktree root.
pub(crate) fn worktrees_root() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some((_, home)) = TEST_ENV.with(|value| value.borrow().clone()) {
        return Ok(home.join(".dial").join("wt"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; cannot place the PR worktree".to_owned())?;
    Ok(PathBuf::from(home).join(".dial").join("wt"))
}

/// First 7 hex digits of SHA-256 over the path.
pub(crate) fn hash_path(path: &Path) -> String {
    let mut hex = crate::hex(&Sha256::digest(path.to_string_lossy().as_bytes())[..4]);
    hex.truncate(7);
    hex
}

/// Base path, then `-2` through `-100`, skipping existing paths and registered worktrees.
fn available_worktree_path(
    base: &Path,
    segment: &str,
    registered: &[Worktree],
) -> Result<PathBuf, String> {
    for attempt in 1..=MAX_WORKTREE_SUFFIX {
        let name = if attempt == 1 {
            segment.to_owned()
        } else {
            format!("{segment}-{attempt}")
        };
        let candidate = base.join(name);
        if !candidate.exists() && !registered.iter().any(|tree| tree.path == candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no available worktree path under {} for {segment}",
        base.display()
    ))
}

fn add_worktree(
    env: &Env,
    trees: &[Worktree],
    number: u64,
    local: &str,
) -> Result<PathBuf, String> {
    let primary = primary_root(env)?;
    let segment = format!("{number}-{}", hash_path(&primary));
    let base = worktrees_root()?;
    fs::create_dir_all(&base).map_err(|e| format!("could not create {}: {e}", base.display()))?;
    let path = available_worktree_path(&base, &segment, trees)?;
    let path_text = path.to_string_lossy().into_owned();
    env.git_text(&["worktree", "add", path_text.as_str(), local])?;
    fs::canonicalize(&path).map_err(|e| format!("could not resolve {}: {e}", path.display()))
}

fn checkout_lines(checkout: &Checkout) -> Vec<String> {
    let mut lines = Vec::new();
    push_line(&mut lines, "Title", checkout.title.clone());
    push_line(&mut lines, "URL", Some(checkout.url.clone()));
    push_line(&mut lines, "Base", checkout.base.clone());
    push_line(&mut lines, "Head", Some(checkout.head_ref.clone()));
    push_line(
        &mut lines,
        "Local branch",
        Some(checkout.local_branch.clone()),
    );
    push_line(
        &mut lines,
        "Worktree",
        Some(checkout.worktree.display().to_string()),
    );
    push_line(&mut lines, "Remote", Some(checkout.remote.clone()));
    push_line(&mut lines, "Remote URL", Some(checkout.remote_url.clone()));
    push_line(
        &mut lines,
        "Cross repository",
        Some(checkout.cross.to_string()),
    );
    push_line(
        &mut lines,
        "Maintainer can modify",
        Some(checkout.maintainer_can_modify.to_string()),
    );
    lines
}

fn outcome_sentence(checkout: &Checkout) -> &'static str {
    if checkout.reused {
        "Reused the existing PR worktree."
    } else {
        "Created a dedicated worktree for this PR and configured the local branch to push back to the PR head branch."
    }
}

fn failure_lines(failures: &[(String, String)]) -> Vec<String> {
    failures
        .iter()
        .map(|(pr, error)| format!("- {pr}: {error}"))
        .collect()
}

fn render_checkouts(
    total: usize,
    checkouts: &[Checkout],
    failures: &[(String, String)],
) -> Result<ToolOutput, String> {
    if checkouts.is_empty() {
        return Err(match failures {
            [(_, error)] => error.clone(),
            _ => format!(
                "All {total} pull request checkouts failed.\n{}",
                failure_lines(failures).join("\n")
            ),
        });
    }

    let mut out = Vec::new();
    let single = total == 1;
    if single {
        let checkout = &checkouts[0];
        out.push(if checkout.reused {
            format!("# Pull Request #{} Worktree", checkout.number)
        } else {
            format!("# Checked Out Pull Request #{}", checkout.number)
        });
        out.push(String::new());
        out.extend(checkout_lines(checkout));
        out.push(String::new());
        out.push(outcome_sentence(checkout).to_owned());
    } else {
        if failures.is_empty() {
            let reused = checkouts.iter().filter(|c| c.reused).count();
            out.push(format!(
                "# {total} Pull Request Worktrees ({} checked out, {reused} reused)",
                checkouts.len() - reused
            ));
        } else {
            out.push(format!(
                "# {}/{total} Pull Request Worktrees checked out ({} failed)",
                checkouts.len(),
                failures.len()
            ));
        }
        for checkout in checkouts {
            out.push(String::new());
            out.push(format!("## Pull Request #{}", checkout.number));
            out.extend(checkout_lines(checkout));
            out.push(outcome_sentence(checkout).to_owned());
        }
        if !failures.is_empty() {
            out.push(String::new());
            out.push("## Failed".to_owned());
            out.extend(failure_lines(failures));
        }
    }

    let checkouts_json: Vec<Value> = checkouts
        .iter()
        .map(|c| {
            json!({
                "number": c.number,
                "url": c.url,
                "localBranch": c.local_branch,
                "worktree": c.worktree.display().to_string(),
                "reused": c.reused,
                "remote": c.remote,
                "remoteUrl": c.remote_url,
            })
        })
        .collect();
    let failed_json: Vec<Value> = failures
        .iter()
        .map(|(pr, error)| json!({ "pr": pr, "error": error }))
        .collect();
    let mut data = json!({
        "op": "pr_checkout",
        "checkouts": checkouts_json,
        "failed": failed_json,
    });
    if single {
        data["sourceUrl"] = json!(checkouts[0].url);
    }
    Ok(ToolOutput {
        execution: Default::default(),
        text: out.join("\n"),
        is_error: false,
        data: Some(data),
    })
}

// ───────────────────────────── pr_push ─────────────────────────────

fn current_branch(env: &Env) -> Result<Option<String>, String> {
    let run = env.git(&["symbolic-ref", "--quiet", "--short", "HEAD"], GIT_TIMEOUT)?;
    Ok(run
        .success
        .then(|| run.stdout.trim().to_owned())
        .filter(|branch| !branch.is_empty()))
}

/// Branch names reaching git must not look like options. Unlike `git check-ref-format`, this is a
/// conservative allow-list.
fn valid_branch(name: &str) -> Result<(), String> {
    let valid = !name.starts_with('-')
        && !name.contains("..")
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-' | b'@' | b'+')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("invalid branch name: {name}"))
    }
}

fn pr_push(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
    let env = Env { ws, cancel };
    let explicit = opt_str(args, "branch")?.and_then(trimmed);
    let force_with_lease = opt_bool(args, "forceWithLease")?;
    let current = current_branch(&env)?;
    let branch = explicit
        .or_else(|| current.clone())
        .ok_or_else(|| "Current git branch is unavailable. Pass `branch` explicitly.".to_owned())?;
    valid_branch(&branch)?;

    let full = format!("refs/heads/{branch}");
    if !env.git_succeeds(&["show-ref", "--verify", "--quiet", full.as_str()])? {
        return Err(format!("local branch {branch} does not exist"));
    }
    let head_ref = env
        .config_get(&format!("branch.{branch}.dialPrHeadRef"))?
        .ok_or_else(|| {
            format!(
                "branch {branch} has no PR push metadata; check it out via op: pr_checkout first"
            )
        })?;
    let push_remote = env.config_get(&format!("branch.{branch}.pushRemote"))?;
    let remote = push_remote
        .or(env.config_get(&format!("branch.{branch}.remote"))?)
        .ok_or_else(|| format!("branch {branch} has no configured push remote"))?;
    let remote_url = env
        .config_get(&format!("remote.{remote}.url"))?
        .unwrap_or_default();
    let pr_url = env.config_get(&format!("branch.{branch}.dialPrUrl"))?;

    let source = if current.as_deref() == Some(branch.as_str()) {
        "HEAD".to_owned()
    } else {
        full
    };
    let refspec = format!("{source}:refs/heads/{head_ref}");
    let mut push_args = vec!["push", "--no-follow-tags"];
    if force_with_lease {
        push_args.push("--force-with-lease");
    }
    push_args.push(remote.as_str());
    push_args.push(refspec.as_str());
    env.git_checked(&push_args, NETWORK_TIMEOUT)?;

    let mut lines = Vec::new();
    push_line(&mut lines, "Local branch", Some(branch.clone()));
    push_line(&mut lines, "Remote", Some(remote.clone()));
    push_line(&mut lines, "Remote branch", Some(head_ref.clone()));
    push_line(&mut lines, "Remote URL", Some(remote_url.clone()));
    push_line(&mut lines, "PR", pr_url.clone());
    push_line(
        &mut lines,
        "Force with lease",
        Some(force_with_lease.to_string()),
    );
    let mut out = vec!["# Pushed Pull Request Branch".to_owned(), String::new()];
    out.extend(lines);
    out.push(String::new());
    out.push(format!("Pushed {branch} to {remote}:{head_ref}."));
    let data = json!({
        "op": "pr_push",
        "localBranch": branch,
        "remote": remote,
        "remoteBranch": head_ref,
        "remoteUrl": remote_url,
        "prUrl": pr_url,
        "forceWithLease": force_with_lease,
        "sourceUrl": pr_url,
    });
    Ok(ToolOutput {
        execution: Default::default(),
        text: out.join("\n"),
        is_error: false,
        data: Some(data),
    })
}

#[cfg(test)]
mod tests;
