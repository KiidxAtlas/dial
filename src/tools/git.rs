//! The `git` tool: structured git and GitHub (`gh`) operations.
//!
//! Every action builds a fixed argument vector (no shell), validates refs and paths, and runs
//! with repository hooks, external diff drivers, textconv and fsmonitor switched off, because
//! repository configuration is untrusted. Reads need no approval; anything that changes the
//! repository or reaches the network does. There is deliberately no force-push, reset, clean,
//! rebase or checkout of paths: those stay with `bash` and its approval prompt.
use super::ext::{Tool, ToolContext};
use super::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use serde_json::{Value, json};
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 20 * 1024;
const MAX_MESSAGE: usize = 8 * 1024;
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

const DESCRIPTION: &str = "Git and GitHub operations without shell quoting. Read actions: `status`, `diff` \
(optional `staged`, `base` ref, `path`), `log` (`limit` ≤ 50, optional `path`), `show` (`rev`), `branches`, \
`pr_view`/`pr_checks`/`issue_view` (`number`; need the `gh` CLI). Changing actions (ask approval): `add` (`paths`), \
`unstage` (`paths`), `commit` (`message`; commits what is staged, or every tracked change with `all`), \
`branch_create` (`name`, optional `from`; switches to it), `switch` (`name`), `push` (optional `remote`, `branch`; \
never forces), `pr_create` (`title`, `body`, optional `base`, `draft`). Repository hooks do not run. For reset, rebase, \
force-push or anything else, use bash.";

pub struct Git;

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Git)]
}

const READS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
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

fn clip(text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let end = (0..=MAX_OUTPUT)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!(
        "{}\n[output cut at {MAX_OUTPUT} bytes; narrow the request with `path`, `limit` or a smaller range]",
        &text[..end]
    )
}

/// Run `program` with a bounded time and output. Returns (success, combined stdout+stderr).
fn run(
    program: &str,
    args: &[String],
    root: &Path,
    timeout: Duration,
) -> Result<(bool, String), String> {
    let mut command = Command::new(program);
    if program == "git" {
        // Repository config is untrusted: no hooks, fsmonitor, pager, external diff or textconv.
        command.args([
            "-c",
            "core.hooksPath=/dev/null",
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
    }
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("`{program}` is not installed or not on PATH"),
        _ => format!("could not run {program}: {e}"),
    })?;
    let reader = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stream) = stream {
                let _ = stream.take(4 * MAX_OUTPUT as u64).read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = reader(child.stdout.take().map(|s| Box::new(s) as _));
    let err = reader(child.stderr.take().map(|s| Box::new(s) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let text = if err.trim().is_empty() {
        out
    } else if out.trim().is_empty() {
        err
    } else {
        format!("{out}\n{err}")
    };
    Ok((status.success(), text))
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

fn dispatch(ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
    let action = req_str(args, "action")?;
    let root = ws.root();
    let git = |a: Vec<String>, t: Duration| run("git", &a, root, t);
    let gh = |a: Vec<String>| run("gh", &a, root, NETWORK_TIMEOUT);
    let (ok, text) = match action {
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
            let message = req_str(args, "message")?.trim();
            if message.is_empty() || message.len() > MAX_MESSAGE {
                return Err(format!("commit message must be 1..{MAX_MESSAGE} bytes"));
            }
            let mut a = strings(&["commit", "--no-verify", "--no-gpg-sign"]);
            if opt_bool(args, "all")? {
                a.push("--all".into());
            }
            a.extend(["-m".into(), message.to_owned()]);
            git(a, LOCAL_TIMEOUT)?
        }
        "branch_create" => {
            let name = valid_ref(req_str(args, "name")?)?;
            let mut a = strings(&["switch", "-c", name]);
            if let Some(from) = opt_str(args, "from")? {
                a.push(valid_ref(from)?.to_owned());
            }
            git(a, LOCAL_TIMEOUT)?
        }
        "switch" => git(
            vec!["switch".into(), valid_ref(req_str(args, "name")?)?.into()],
            LOCAL_TIMEOUT,
        )?,
        "push" => {
            let mut a = strings(&["push", "--no-verify"]);
            if let Some(remote) = opt_str(args, "remote")? {
                a.push(valid_ref(remote)?.to_owned());
            } else {
                a.push("origin".into());
            }
            match opt_str(args, "branch")? {
                Some(branch) => a.push(valid_ref(branch)?.to_owned()),
                None => a.push("HEAD".into()),
            }
            git(a, NETWORK_TIMEOUT)?
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
    };
    let text = clip(text.trim_end().to_owned());
    let text = if text.is_empty() {
        "(no output)".to_owned()
    } else {
        text
    };
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
                "action": {"type": "string", "enum": ["status", "diff", "log", "show", "branches", "add", "unstage", "commit", "branch_create", "switch", "push", "pr_create", "pr_view", "pr_checks", "issue_view"]},
                "paths": {"type": "array", "items": {"type": "string"}, "description": "add/unstage: workspace paths"},
                "path": {"type": "string", "description": "diff/log: limit to this path"},
                "staged": {"type": "boolean", "description": "diff: staged changes"},
                "base": {"type": "string", "description": "diff: compare the working tree to this ref; pr_create: base branch"},
                "rev": {"type": "string", "description": "show: commit, tag or ref"},
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
        dispatch(ctx.workspace, args).unwrap_or_else(ToolOutput::err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::PermissionsExt;

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
        dispatch(ws, &args).unwrap_or_else(ToolOutput::err)
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
    fn repository_hooks_never_run_and_approval_follows_the_action() {
        let (dir, ws) = repo();
        let hook = dir.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
        crate::platform::fs::set_mode(&hook, 0o755).unwrap();
        std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
        assert!(!call(&ws, json!({"action":"add","paths":["c.txt"]})).is_error);
        let commit = call(&ws, json!({"action":"commit","message":"x"}));
        assert!(!commit.is_error, "{}", commit.text);
        assert!(
            !dir.path().join("hook-ran").exists(),
            "the repository's pre-commit hook ran"
        );
        for (action, asks) in [
            ("status", false),
            ("diff", false),
            ("pr_view", false),
            ("commit", true),
            ("push", true),
            ("add", true),
            ("pr_create", true),
        ] {
            assert_eq!(
                Git.needs_approval(&json!({"action":action})),
                asks,
                "{action}"
            );
        }
    }
}
