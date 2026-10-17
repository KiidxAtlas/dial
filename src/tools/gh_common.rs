//! Shared `gh` CLI runner for the GitHub tool: non-interactive environment, bounded capture,
//! deadline and cancellation, plus gh-compatible failure messages.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::Workspace;

pub(super) const GH_TIMEOUT: Duration = Duration::from_secs(120);
/// Model-facing cut for command output; the complete text goes to an artifact.
const MAX_OUTPUT: usize = 20 * 1024;

/// Captured result of one `gh` invocation (stdout and stderr are untrimmed).
struct GhRun {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Validate `owner/name` (ASCII letters, digits, `-`, `_`, `.`; no leading `-`).
pub(super) fn valid_repo(text: &str) -> Result<&str, String> {
    let valid = match text.split_once('/') {
        Some((owner, name)) => [owner, name].iter().all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }),
        None => false,
    };
    if valid {
        Ok(text)
    } else {
        Err(format!("repo must be `owner/name`, got `{text}`"))
    }
}

/// A repository reference: `owner/name` on the default host, `host/owner/name` elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GhRepo {
    /// Non-default host (lowercased); `None` means the default host.
    pub host: Option<String>,
    /// `owner/name`.
    pub slug: String,
}

impl GhRepo {
    /// `host/owner/name` when the host is not the default, else `owner/name`.
    pub(super) fn qualified(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}/{}", self.slug),
            None => self.slug.clone(),
        }
    }

    /// `--hostname <host>` for `gh api` calls; empty on the default host.
    pub(super) fn hostname_args(&self) -> Vec<String> {
        match &self.host {
            Some(host) => strings(&["--hostname", host]),
            None => Vec::new(),
        }
    }
}

/// Drop the host when it is the default host; otherwise lowercase it.
fn normalize_host(host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    (host != default_gh_host().to_ascii_lowercase()).then_some(host)
}

/// Parse `owner/name` or `host/owner/name`.
pub(super) fn parse_repo_ref(text: &str) -> Result<GhRepo, String> {
    let parts: Vec<&str> = text.split('/').collect();
    match parts.as_slice() {
        [_, _] => Ok(GhRepo {
            host: None,
            slug: valid_repo(text)?.to_owned(),
        }),
        [host, _, _] => {
            let host_ok = !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'));
            if !host_ok {
                return Err(format!("repo host is invalid in `{text}`"));
            }
            let slug = valid_repo(&text[host.len() + 1..])?;
            Ok(GhRepo {
                host: normalize_host(host),
                slug: slug.to_owned(),
            })
        }
        _ => Err(format!(
            "repo must be `owner/name` or `host/owner/name`, got `{text}`"
        )),
    }
}

/// Parse a repository web URL such as `https://host/owner/name[.git]`.
fn repo_from_url(url: &str) -> Result<GhRepo, String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("could not read repository URL `{url}`"))?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| format!("could not read repository URL `{url}`"))?;
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.is_empty() {
        return Err(format!("could not read repository URL `{url}`"));
    }
    let mut segments = path.trim_matches('/').split('/');
    let owner = segments.next().unwrap_or("");
    let name = segments.next().unwrap_or("").trim_end_matches(".git");
    let slug = valid_repo(&format!("{owner}/{name}"))?.to_owned();
    Ok(GhRepo {
        host: normalize_host(host),
        slug,
    })
}

/// Resolve the target repository: parse an explicit `owner/name` or `host/owner/name`, otherwise
/// read the URL of the workspace checkout from `gh repo view`.
pub(super) fn resolve_repo(
    ws: &Workspace,
    repo: Option<&str>,
    cancel: &AtomicBool,
) -> Result<GhRepo, String> {
    match repo {
        Some(repo) => parse_repo_ref(repo),
        None => {
            let args = strings(&["repo", "view", "--json", "url", "-q", ".url"]);
            let url = gh(ws, &args, GH_TIMEOUT, cancel)?;
            repo_from_url(&url)
        }
    }
}

/// Run `gh` with `args` in the workspace root. Returns stdout on exit 0; a non-zero exit becomes an
/// error with the trimmed stderr (or stdout) and `gh` context. stdout is returned untrimmed.
pub(super) fn gh(
    ws: &Workspace,
    args: &[String],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let run = run_gh(ws.root(), args, timeout, cancel)?;
    if run.success {
        Ok(run.stdout)
    } else {
        Err(format_failure(args, &run.stdout, &run.stderr))
    }
}

/// Run `gh` and parse stdout as JSON. Fails on non-zero exit, empty output or invalid JSON.
pub(super) fn gh_json(
    ws: &Workspace,
    args: &[String],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Value, String> {
    let run = run_gh(ws.root(), args, timeout, cancel)?;
    if !run.success {
        return Err(format_json_failure(args, &run.stdout, &run.stderr));
    }
    if run.stdout.trim().is_empty() {
        return Err("GitHub CLI returned empty output.".to_owned());
    }
    serde_json::from_str(&run.stdout)
        .map_err(|_| "GitHub CLI returned invalid JSON output.".to_owned())
}

/// Cut long output for the model; the complete text goes to an artifact when a store exists.
pub(super) fn clip(ws: &Workspace, text: String) -> String {
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
        None => "narrow the request with `repo`, `path`, `limit` or a smaller range".to_owned(),
    };
    format!(
        "{}\n[output cut at {MAX_OUTPUT} bytes; {full}]",
        &text[..end]
    )
}

/// Convert `&str` literals to the owned argument vector `gh` expects.
pub(super) fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Host used for browser links and API calls: `GH_HOST` when set, else `github.com`.
pub(super) fn default_gh_host() -> String {
    std::env::var("GH_HOST")
        .ok()
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "github.com".to_owned())
}

/// Spawn `gh` directly (no shell) in `root`, bounded by `timeout` and the cancel flag.
fn run_gh(
    root: &std::path::Path,
    args: &[String],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<GhRun, String> {
    let label = args.join(" ");
    let mut command = Command::new(gh_program());
    command
        .args(args)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_EDITOR", "true")
        .env("NO_COLOR", "1")
        .env("LC_MESSAGES", "C");
    let output = crate::runtime::process::run(
        command,
        None,
        timeout,
        64 * 1024 * 1024,
        cancel,
        &mut |_, _| {},
    )?;
    if output.cancelled || output.timed_out || output.io_error.is_some() {
        return Err(output.failure(&format!("gh {label}"), timeout));
    }
    Ok(GhRun {
        success: output.success(),
        stdout: output.stdout_text(),
        stderr: output.text(crate::runtime::process::Stream::Stderr),
    })
}

/// Maps well-known gh failure messages to actionable text, else the raw message.
fn format_failure(args: &[String], stdout: &str, stderr: &str) -> String {
    let raw = if stderr.is_empty() { stdout } else { stderr };
    let message = raw.trim();
    if message.contains("gh auth login") || message.contains("not logged into any GitHub hosts") {
        return "GitHub CLI is not authenticated. Run `gh auth login`.".to_owned();
    }
    if message.contains("not a git repository")
        || message.contains("no git remotes found")
        || message.contains("unable to determine current repository")
    {
        return "GitHub repository context is unavailable. Pass `repo` explicitly or run the tool inside a GitHub checkout.".to_owned();
    }
    if !message.is_empty() {
        return message.to_owned();
    }
    format!("GitHub CLI command failed: gh {}", args.join(" "))
}

/// Failure text for gh JSON calls: appends GitHub API error details when the generic message adds nothing.
fn format_json_failure(args: &[String], stdout: &str, stderr: &str) -> String {
    let raw = (if stderr.is_empty() { stdout } else { stderr }).trim();
    let fallback = format_failure(args, stdout, stderr);
    if fallback != raw {
        return fallback;
    }
    let details: Vec<String> = api_error_messages(stdout)
        .into_iter()
        .filter(|message| !fallback.contains(message.as_str()))
        .collect();
    if details.is_empty() {
        return fallback;
    }
    let lines: Vec<String> = details
        .iter()
        .map(|message| format!("- {message}"))
        .collect();
    format!("{fallback}\nGitHub details:\n{}", lines.join("\n"))
}

/// Unique messages from a GitHub API error body (`message` plus each `errors[]` entry).
fn api_error_messages(stdout: &str) -> Vec<String> {
    let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(stdout) else {
        return Vec::new();
    };
    let mut messages: Vec<String> = Vec::new();
    let mut push = |message: Option<String>| {
        if let Some(message) = message
            && !messages.contains(&message)
        {
            messages.push(message);
        }
    };
    push(payload.get("message").and_then(describe_api_error));
    if let Some(Value::Array(errors)) = payload.get("errors") {
        for error in errors {
            push(describe_api_error(error));
        }
    }
    messages
}

/// Describes a GitHub API error: a string, a `message`, or `resource.field: code`.
fn describe_api_error(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return non_empty(text.trim());
    }
    let object = value.as_object()?;
    if let Some(message) = object.get("message").and_then(Value::as_str) {
        return non_empty(message.trim());
    }
    let part = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned()
    };
    let target = [part("resource"), part("field")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    let code = part("code");
    if !target.is_empty() && !code.is_empty() {
        return Some(format!("{target}: {code}"));
    }
    non_empty(&if target.is_empty() { code } else { target })
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// Program `run_gh` spawns: `gh` from PATH in every build; tests may route it to a fake.
fn gh_program() -> std::ffi::OsString {
    #[cfg(test)]
    let program = test_support::gh_override().unwrap_or_else(|| "gh".into());
    #[cfg(not(test))]
    let program = std::ffi::OsString::from("gh");
    program
}

/// Test support shared by the GitHub tool modules: a fake `gh` that records argv.
#[cfg(test)]
pub(super) mod test_support;

#[cfg(test)]
mod tests;
