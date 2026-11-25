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

#[cfg(test)]
mod tests;
