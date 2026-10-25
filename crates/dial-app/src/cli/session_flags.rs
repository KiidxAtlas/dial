//! Session-selection flags: `--fork <id|path>`, `--from-claude [PATH]`, `--from-codex [PATH]`.
//!
//! `SessionFlagArgs::selection` validates flag combinations without side
//! effects. `SessionFlagArgs::resolve` performs the session-creating action. `--export` is not
//! handled here; see the notes in the module report (`session export` already exists in `main.rs`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use clap::Args;

use crate::cli::session_import::{self, Source};
use crate::config::paths::Layout;
use crate::storage::session::Session;

#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = crate::cli::help_heading::SESSIONS)]
pub struct SessionFlagArgs {
    /// Fork a session (by id prefix or by .jsonl path) into a new session.
    #[arg(long, value_name = "ID|PATH")]
    pub fork: Option<String>,
    /// Import a Claude Code transcript as a new session. Pass the .jsonl path; without one, the
    /// candidate sessions for the current directory are listed.
    #[arg(long = "from-claude", value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub from_claude: Option<String>,
    /// Import a Codex rollout transcript as a new session. Pass the .jsonl path; without one, the
    /// candidate sessions for the current directory are listed.
    #[arg(long = "from-codex", value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub from_codex: Option<String>,
}

/// Command-line state outside these flags that the combinations depend on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context {
    /// `--continue` or `--resume` was given.
    pub continue_or_resume: bool,
    /// `--no-session` was given.
    pub no_session: bool,
    /// Running in a protocol mode (rpc/acp/...), where foreign imports are rejected.
    pub protocol_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Fork(String),
    Import {
        source: Source,
        path: Option<PathBuf>,
    },
}

/// True when a `--fork` value names a file rather than a session id.
pub fn is_path_selector(selector: &str) -> bool {
    selector.contains('/') || selector.contains('\\') || selector.ends_with(".jsonl")
}

impl SessionFlagArgs {
    /// Validates flag combinations and returns the requested action, if any.
    pub fn selection(&self, ctx: Context) -> Result<Option<Selection>> {
        if self.from_claude.is_some() && self.from_codex.is_some() {
            bail!("--from-claude and --from-codex cannot be used together");
        }
        let import = match (&self.from_claude, &self.from_codex) {
            (Some(raw), _) => Some((Source::Claude, raw)),
            (_, Some(raw)) => Some((Source::Codex, raw)),
            _ => None,
        };
        if let Some((source, raw)) = import {
            let flag = format!("--from-{}", source.flag());
            if self.fork.is_some() || ctx.continue_or_resume {
                bail!("{flag} cannot be combined with --continue, --resume, or --fork");
            }
            if ctx.protocol_mode {
                bail!("{flag} is not available in protocol modes");
            }
            if ctx.no_session {
                bail!("{flag} requires session persistence");
            }
            let path = (!raw.trim().is_empty()).then(|| PathBuf::from(raw.trim()));
            return Ok(Some(Selection::Import { source, path }));
        }
        if let Some(selector) = &self.fork {
            if ctx.continue_or_resume {
                bail!("--fork cannot be combined with --continue or --resume");
            }
            if selector.trim().is_empty() {
                bail!("--fork requires a session id or a .jsonl path");
            }
            if ctx.no_session {
                bail!("--fork requires session persistence");
            }
            return Ok(Some(Selection::Fork(selector.trim().to_string())));
        }
        Ok(None)
    }

    /// Runs the selected action and returns the session the process should continue with.
    /// `Ok(None)` means no session flag was given.
    pub fn resolve(&self, cwd: &Path, ctx: Context) -> Result<Option<Session>> {
        match self.selection(ctx)? {
            None => Ok(None),
            Some(Selection::Fork(selector)) => fork(cwd, &selector).map(Some),
            Some(Selection::Import { source, path }) => {
                import(source, cwd, path.as_deref()).map(Some)
            }
        }
    }
}

/// Forks a session selected by id prefix (within `cwd`'s project) or by `.jsonl` path into a new
/// session, which records the source as its parent.
pub fn fork(cwd: &Path, selector: &str) -> Result<Session> {
    if selector.trim().is_empty() {
        bail!("--fork requires a session id or a .jsonl path");
    }
    let layout = Layout::discover(cwd)?;
    let source_path = if is_path_selector(selector) {
        let path = PathBuf::from(selector);
        if !path.is_file() {
            bail!("Session \"{selector}\" not found.");
        }
        path
    } else {
        let sessions = Session::list(&layout, usize::MAX)?;
        let mut matches = sessions.iter().filter(|s| s.id.starts_with(selector));
        let found = matches
            .next()
            .ok_or_else(|| anyhow!("Session \"{selector}\" not found."))?;
        if matches.next().is_some() {
            bail!("session prefix {selector:?} is ambiguous; use a longer id");
        }
        found.path.clone()
    };
    Session::read(&source_path)?.fork(&layout)
}

/// Imports a Claude Code or Codex transcript as a new Dial session. With `path: None`, lists the
/// candidate sessions for `cwd` and fails, since Dial has no interactive picker here.
pub fn import(source: Source, cwd: &Path, path: Option<&Path>) -> Result<Session> {
    let Some(path) = path else {
        bail!("{}", candidates_message(source, cwd));
    };
    if !path.is_file() {
        bail!(
            "{} transcript not found: {}",
            source.label(),
            path.display()
        );
    }
    let layout = Layout::discover(cwd)?;
    let imported = session_import::import_file(&layout, source, path)?;
    let dial_id = imported.session.id().to_string();
    eprintln!(
        "Imported {} transcript {} as session {dial_id}",
        source.label(),
        imported.source_id.as_deref().unwrap_or("(no id)"),
    );
    if !imported.skipped.is_empty() {
        eprintln!("Not imported: {}", imported.skipped);
    }
    Ok(imported.session)
}

fn candidates_message(source: Source, cwd: &Path) -> String {
    let cwd = cwd.display().to_string();
    let list = match source {
        Source::Claude => session_import::list_claude(&session_import::claude_root(), &cwd),
        Source::Codex => session_import::list_codex(&session_import::codex_root(), &cwd),
    };
    if list.is_empty() {
        return format!("No {} sessions found for {cwd}", source.label());
    }
    let mut msg = format!(
        "--from-{} needs a transcript path; {} sessions for {cwd}:",
        source.flag(),
        source.label()
    );
    for c in list.iter().take(20) {
        let _ = write!(
            msg,
            "\n  {}  {}",
            c.path.display(),
            c.title.as_deref().unwrap_or(c.id.as_str())
        );
    }
    msg
}

#[cfg(test)]
mod tests;
