//! Shared `glab` CLI runner for the GitLab merge-request tool: non-interactive environment,
//! bounded capture, deadline and cancellation, GitLab-specific failure text, project references
//! and the token-host guard.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use crate::runtime::process::Stream;

pub(super) const GLAB_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_CAPTURE: usize = 64 * 1024 * 1024;
const MISSING_GLAB: &str =
    "GitLab CLI (glab) is not installed or not on PATH; install glab and run `glab auth login`.";
/// Environment variables glab sends to whichever host it talks to.
const TOKEN_ENV: [&str; 3] = ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN"];
/// Environment variables naming a self-managed GitLab host the user already trusts.
const HOST_ENV: [&str; 3] = ["GITLAB_HOST", "GITLAB_URI", "GL_HOST"];

/// A validated GitLab project: `group[/subgroup…]/project`, optionally on an explicit host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Project {
    host: Option<String>,
    path: String,
}

impl Project {
    /// Host named by the reference, lowercased; `None` means glab's configured default host.
    pub(super) fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The `-R` value: a full URL when the host is explicit (it disambiguates self-managed hosts),
    /// otherwise the bare path.
    pub(super) fn glab_arg(&self) -> String {
        match &self.host {
            Some(host) => format!("https://{host}/{}", self.path),
            None => self.path.clone(),
        }
    }
}

/// Parse `group/project` or `host/group/project`. Segments are plain ASCII names; `..`, `.` and
/// leading `-` are refused so a reference can never read as a flag or escape the project.
pub(super) fn parse_project(text: &str) -> Result<Project, String> {
    let text = text.trim();
    let invalid = || format!("repo must be `group/project` or `host/group/project`, got `{text}`");
    let mut parts: Vec<&str> = text.split('/').collect();
    let host = if parts.len() >= 3 && looks_like_host(parts[0]) {
        let host = parts.remove(0);
        if !valid_host(host) {
            return Err(invalid());
        }
        Some(host.to_ascii_lowercase())
    } else {
        None
    };
    if parts.len() < 2 || !parts.iter().all(|part| valid_segment(part)) {
        return Err(invalid());
    }
    Ok(Project {
        host,
        path: parts.join("/"),
    })
}

/// A first segment with a dot, a port or `localhost` is a host; anything else is a namespace.
fn looks_like_host(segment: &str) -> bool {
    segment.contains('.') || segment.contains(':') || segment.eq_ignore_ascii_case("localhost")
}

/// `host[:port]` with ASCII letters, digits, `.` and `-`; no userinfo or path.
fn valid_host(host: &str) -> bool {
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let port_ok =
        port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    name_ok && port_ok
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.starts_with('-')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Refuse to send a GitLab token to an explicitly named host the user has not trusted. Trusted
/// hosts are gitlab.com and the hosts named by `GITLAB_HOST`, `GITLAB_URI` or `GL_HOST`. Without a
/// token, or without an explicit host, glab only talks to the user's own configured host.
pub(super) fn check_token_host(
    project: &Project,
    token_set: bool,
    configured_hosts: &[String],
) -> Result<(), String> {
    let Some(host) = project.host() else {
        return Ok(());
    };
    if !token_set || host == "gitlab.com" || configured_hosts.iter().any(|name| name == host) {
        return Ok(());
    }
    Err(format!(
        "Refusing to send GitLab token environment variables to {host}: it is not gitlab.com or a host named by GITLAB_HOST. Run `glab auth login --hostname {host}` or set GITLAB_HOST to trust it."
    ))
}

/// Environment inputs to [`check_token_host`], read from the process environment.
pub(super) fn check_token_host_env(project: &Project) -> Result<(), String> {
    let token_set = TOKEN_ENV
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
    let configured: Vec<String> = HOST_ENV
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter_map(|value| host_of(&value))
        .collect();
    check_token_host(project, token_set, &configured)
}

/// The host of a `GITLAB_HOST`-style value: an optional scheme, then the host up to the first `/`.
fn host_of(value: &str) -> Option<String> {
    let value = value.trim();
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let host = rest.split('/').next()?.trim();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Run `glab` with `args` in `root`. Returns stdout on exit 0; a non-zero exit becomes an error
/// with GitLab-specific guidance or glab's own message. stdout is returned untrimmed.
pub(super) fn glab_in(root: &Path, args: &[String], cancel: &AtomicBool) -> Result<String, String> {
    let program = program()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("GLAB_NO_PROMPT", "1")
        .env("GLAB_CHECK_UPDATE", "false")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_EDITOR", "true")
        .env("NO_COLOR", "1")
        .env("LC_MESSAGES", "C");
    let output = crate::runtime::process::run(
        command,
        None,
        GLAB_TIMEOUT,
        MAX_CAPTURE,
        cancel,
        &mut |_, _| {},
    )?;
    let label = format!("glab {}", args.join(" "));
    if output.cancelled || output.timed_out || output.io_error.is_some() {
        return Err(output.failure(&label, GLAB_TIMEOUT));
    }
    let stdout = output.stdout_text();
    if output.success() {
        return Ok(stdout);
    }
    let stderr = output.text(Stream::Stderr);
    Err(format_failure(args, &stdout, &stderr))
}

/// Run `glab` and parse stdout as JSON. Fails on non-zero exit, empty output or invalid JSON.
pub(super) fn glab_json(
    root: &Path,
    args: &[String],
    cancel: &AtomicBool,
) -> Result<Value, String> {
    let stdout = glab_in(root, args, cancel)?;
    if stdout.trim().is_empty() {
        return Err("GitLab CLI returned empty output.".to_owned());
    }
    serde_json::from_str(&stdout).map_err(|_| "GitLab CLI returned invalid JSON output.".to_owned())
}

/// Maps well-known glab failure messages to actionable text, else glab's own message.
fn format_failure(args: &[String], stdout: &str, stderr: &str) -> String {
    let raw = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let message = raw.trim();
    if message.contains("glab auth login")
        || message.contains("401 Unauthorized")
        || message.contains("has not been authenticated")
    {
        return "GitLab CLI is not authenticated. Run `glab auth login`.".to_owned();
    }
    if message.contains("not a git repository")
        || message.contains("no git remotes")
        || message.contains("could not determine base repository")
    {
        return "GitLab repository context is unavailable. Pass `repo` explicitly or run the tool inside a GitLab checkout.".to_owned();
    }
    if !message.is_empty() {
        return message.to_owned();
    }
    format!("GitLab CLI command failed: glab {}", args.join(" "))
}

/// Program `glab_in` spawns: the test override when installed, else `glab` from `PATH`.
fn program() -> Result<OsString, String> {
    #[cfg(test)]
    {
        if let Some(program) = super::test_support::glab_override() {
            return Ok(program);
        }
    }
    resolve_program(std::env::var_os("PATH").as_deref(), "glab").map(PathBuf::into_os_string)
}

/// The first file named `name` in the `path` search list. A missing program is reported with the
/// install and login steps rather than a bare spawn error.
pub(super) fn resolve_program(path: Option<&OsStr>, name: &str) -> Result<PathBuf, String> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| MISSING_GLAB.to_owned())
}
