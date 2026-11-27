//! Native controller reads and writes that give a client the rows the terminal prints for
//! `/hooks`, `/ssh`, `/wt list` and `dial --from-claude|--from-codex`: the configured lifecycle
//! hooks with the event names they may use, every addressable SSH host, the repository's
//! worktrees, and foreign transcripts for this project with their import. Hooks and SSH hosts
//! are edited through `set_setting`/`reset_setting` (`hooks`, `ssh_hosts.<name>`), which already
//! own validation and locked publication.
use super::coded;
use crate::cli::session_import::{self, Candidate, Source};
use crate::config::Config;
use crate::config::paths::Layout;
use crate::tools::vcs::git;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};
use std::path::Path;
use std::time::UNIX_EPOCH;

fn invalid(message: impl Into<String>) -> anyhow::Error {
    coded(ErrorCode::InvalidParams, message.into())
}

fn object(params: &Value, method: &str) -> Result<()> {
    if params.is_null() || params.is_object() {
        Ok(())
    } else {
        Err(invalid(format!("{method} params must be an object")))
    }
}

/// `list_hooks`: the saved hooks in order, each with its effective failure mode, and the
/// canonical event names a new hook may use.
pub(super) fn hooks(config: &Config) -> Result<Value> {
    let hooks: Vec<Value> = config
        .hooks
        .iter()
        .map(|hook| {
            let mut row = serde_json::to_value(hook)?;
            let canonical = crate::config::hooks::canonical(&hook.event);
            row["fails_closed"] = json!(hook.fail_closed.unwrap_or(canonical == "pre_tool_use"));
            Ok(row)
        })
        .collect::<Result<_>>()?;
    Ok(json!({"hooks": hooks, "events": crate::config::hooks::EVENTS}))
}

/// `ssh_hosts`: `~/.ssh/config` aliases merged with `[ssh_hosts]`; a configured entry wins.
/// `origin` is `dial` for a configured host (removable with `reset_setting`) or `ssh_config`.
pub(super) fn ssh_hosts(config: &Config) -> Result<Value> {
    let hosts = match crate::config::paths::home_dir() {
        Some(home) => crate::tools::integrations::ssh::resolve_hosts(&home, &config.ssh_hosts),
        None => crate::tools::integrations::ssh::resolve_hosts(Path::new(""), &config.ssh_hosts),
    };
    let rows: Vec<Value> = hosts
        .iter()
        .map(|host| {
            let configured = host.source.as_os_str().is_empty();
            json!({
                "name": host.alias,
                "host": host.hostname.as_deref().unwrap_or(&host.alias),
                "user": host.user,
                "port": host.port,
                "key": host.identity_file,
                "origin": if configured { "dial" } else { "ssh_config" },
                "source": (!configured).then(|| host.source.display().to_string()),
            })
        })
        .collect();
    Ok(json!({"hosts": rows}))
}

/// `list_worktrees`: the repository's worktrees, primary checkout first; `current` marks the one
/// this engine runs in. Outside a repository the list is empty and `repository` is false.
pub(super) fn worktrees(root: &Path) -> Result<Value> {
    let trees = match git::worktree_list(root) {
        Ok(trees) => trees,
        Err(_) if !root.join(".git").exists() => {
            return Ok(json!({"repository": false, "worktrees": []}));
        }
        Err(error) => return Err(coded(ErrorCode::Failed, error)),
    };
    let here = dunce_canonical(root);
    let rows: Vec<Value> = trees
        .iter()
        .enumerate()
        .map(|(index, tree)| {
            json!({
                "path": tree.path.display().to_string(),
                "branch": tree.branch,
                "head": tree.head,
                "primary": index == 0,
                "current": dunce_canonical(&tree.path) == here,
                "bare": tree.bare,
                "detached": tree.detached,
                "locked": tree.locked,
            })
        })
        .collect();
    Ok(json!({"repository": true, "worktrees": rows}))
}

fn dunce_canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn source(params: &Value, method: &str) -> Result<Source> {
    match params["source"].as_str() {
        Some("claude") => Ok(Source::Claude),
        Some("codex") => Ok(Source::Codex),
        _ => Err(invalid(format!(
            "{method} requires params.source claude or codex"
        ))),
    }
}

fn candidates(source: Source, project: &Path) -> Vec<Candidate> {
    let cwd = project.display().to_string();
    match source {
        Source::Claude => session_import::list_claude(&session_import::claude_root(), &cwd),
        Source::Codex => session_import::list_codex(&session_import::codex_root(), &cwd),
    }
}

/// `import_candidates`: the Claude Code or Codex transcripts recorded for this project.
pub(super) fn import_candidates(layout: &Layout, params: &Value) -> Result<Value> {
    object(params, "import_candidates")?;
    let source = source(params, "import_candidates")?;
    let rows: Vec<Value> = candidates(source, &layout.project_root)
        .iter()
        .map(|candidate| {
            json!({
                "id": candidate.id,
                "title": candidate.title,
                "path": candidate.path.display().to_string(),
                "modified": candidate
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0),
            })
        })
        .collect();
    Ok(json!({"source": source.flag(), "candidates": rows}))
}

/// `import_session`: converts one listed transcript into a new Dial session. Only a path that
/// `import_candidates` lists for this project is accepted, so a caller cannot make the engine
/// read an arbitrary file.
pub(super) fn import_session(layout: &Layout, params: &Value) -> Result<Value> {
    object(params, "import_session")?;
    let source = source(params, "import_session")?;
    let wanted = params["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid("import_session requires params.path"))?;
    let candidate = candidates(source, &layout.project_root)
        .into_iter()
        .find(|candidate| candidate.path == Path::new(wanted))
        .ok_or_else(|| {
            coded(
                ErrorCode::NotFound,
                format!(
                    "{} transcript is not listed for this project: {wanted}",
                    source.label()
                ),
            )
        })?;
    let imported = session_import::import_file(layout, source, &candidate.path)
        .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
    Ok(json!({
        "session_id": imported.session.id().to_string(),
        "source_id": imported.source_id,
        "skipped": (!imported.skipped.is_empty()).then_some(imported.skipped),
    }))
}

/// `computer_status`: whether computer use is on in the saved configuration, the screenshot size
/// limit, and the native backend's probe (`capabilities`: backend and permission states) or why
/// it is unavailable on this machine, as `/computer status` reports them.
pub(super) fn computer_status(config: &Config) -> Value {
    use crate::computer;
    let enabled = config
        .integrations
        .iter()
        .any(|name| name == computer::NAME);
    let (capabilities, unavailable) = match computer::capabilities() {
        Ok(caps) => (caps, None),
        Err(reason) => (Value::Null, Some(reason)),
    };
    json!({
        "enabled": enabled,
        "max_width": computer::MAX_WIDTH,
        "max_height": computer::MAX_HEIGHT,
        "capabilities": capabilities,
        "unavailable": unavailable,
    })
}

/// `doctor`: optional capability installation status and memory configuration notes, matching
/// `dial doctor --json`.
pub(super) fn doctor(layout: &Layout, config: Option<&Config>, params: &Value) -> Result<Value> {
    object(params, "doctor")?;
    let all = match params.get("all") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(invalid("doctor params.all must be a boolean")),
    };
    let env = crate::setup::deps::Env::current(&layout.user_dir);
    let rows = crate::setup::deps::report(&env);
    let mut capabilities = Vec::new();
    let mut missing = 0usize;
    for row in rows {
        let installed = matches!(row.status, crate::setup::deps::Status::Ready);
        if !installed {
            missing += 1;
            if !all {
                continue;
            }
        }
        capabilities.push(json!({
            "id": row.id,
            "name": row.title,
            "installed": installed,
            "path": installed.then_some(row.detail),
        }));
    }
    let mut notes = Vec::new();
    if !all && missing > 0 {
        notes.push(format!(
            "Not installed: {missing} optional capabilit{}; `dial doctor --all` lists them with their setup commands.",
            if missing == 1 { "y" } else { "ies" }
        ));
    }
    let memory_note = match config {
        Some(cfg) => crate::setup::memory::doctor_text(cfg),
        None => "Memory: settings could not be read.".to_owned(),
    };
    notes.extend(
        memory_note
            .lines()
            .map(|line| line.trim_end().to_owned())
            .filter(|line| !line.is_empty()),
    );
    Ok(json!({
        "capabilities": capabilities,
        "notes": notes,
    }))
}

/// `commit_preview`: generate a commit message preview for the staged changes without creating a
/// commit (`dial commit --dry-run --json`).
pub(super) fn commit_preview(layout: &Layout, config: &Config, params: &Value) -> Result<Value> {
    object(params, "commit_preview")?;
    let model = match params.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if !m.is_empty() => Some(m.clone()),
        Some(_) => {
            return Err(invalid(
                "commit_preview params.model must be a non-empty string",
            ));
        }
    };
    let mut args = vec!["--dry-run".to_owned(), "--json".to_owned()];
    if let Some(m) = model {
        args.push("--model".to_owned());
        args.push(m);
    }
    let parsed = crate::commit::cli::parse_commit_args(&args)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| {
            coded(
                ErrorCode::InternalError,
                "commit_preview args returned help",
            )
        })?;
    let outcome = crate::commit::run::run_commit_command(
        &parsed,
        &layout.project_root,
        config,
        &crate::commit::backend::ProviderBackend,
        &mut |_: &str| true,
    )
    .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
    Ok(outcome.to_json())
}

/// `share_session`: encrypt and upload a session share link, matching `dial share --json`.
pub(super) fn share_session(layout: &Layout, config: &Config, params: &Value) -> Result<Value> {
    object(params, "share_session")?;
    let session_id = match params.get("session_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() => Some(id.as_str()),
        Some(_) => {
            return Err(invalid(
                "share_session params.session_id must be a non-empty string",
            ));
        }
    };
    let gist = match params.get("gist") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(invalid("share_session params.gist must be a boolean")),
    };
    let whole_journal = match params.get("whole_journal") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(invalid(
                "share_session params.whole_journal must be a boolean",
            ));
        }
    };
    let endpoint = match params.get("endpoint") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(invalid("share_session params.endpoint must be a string")),
    };
    let selected = if let Some(id) = session_id {
        let mut matching = crate::storage::session::Session::find_prefix(layout, id)
            .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?
            .into_iter();
        let info = matching.next().ok_or_else(|| {
            coded(
                ErrorCode::NotFound,
                format!("no session in this project starts with {id:?}"),
            )
        })?;
        if let Some(other) = matching.next() {
            return Err(invalid(format!(
                "session prefix {id:?} matches {} and {}; use a longer id",
                info.id, other.id
            )));
        }
        info
    } else {
        crate::storage::session::Session::list_newest(layout, 1)
            .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?
            .into_iter()
            .next()
            .ok_or_else(|| coded(ErrorCode::NotFound, "there are no sessions to share"))?
    };
    let scope = if whole_journal {
        crate::storage::share::Scope::WholeJournal
    } else {
        crate::storage::share::Scope::ActiveBranch
    };
    let transcript = crate::storage::share::transcript(&selected.path, scope)
        .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
    let fitted =
        crate::storage::share::fit_to_cap(&transcript, crate::storage::share::SHARE_CAP_BYTES)
            .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
    let mut notes = vec![
        "the key is only in the URL fragment; anyone with the full link can read the session."
            .to_owned(),
    ];
    for trim in &fitted.trims {
        notes.push(format!("Trimmed to fit the size cap: {trim}."));
    }
    let server = endpoint
        .filter(|s| !s.is_empty())
        .or_else(|| (!config.share.server_url.is_empty()).then(|| config.share.server_url.clone()))
        .or_else(|| {
            std::env::var("DIAL_SHARE_SERVER")
                .ok()
                .filter(|s| !s.is_empty())
        });
    let link = match server {
        Some(server) if !gist => crate::storage::share::upload(&fitted.share, &server),
        _ => crate::storage::share::upload_gist(&fitted.share),
    }
    .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
    Ok(json!({
        "url": link,
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests;
