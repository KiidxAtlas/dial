//! `/debug`: the Debug Tools selector. Without arguments it prints the action list
//! and the session, model, usage, settings, and tool facts behind it. `/debug <item>` runs one
//! action; items Dial cannot provide are reported unavailable with the reason.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

use anyhow::Result;

use crate::agent::Agent;
use crate::agent::modes::debug::DebugSnapshot;
use crate::app::{App, Shell, Step};
use crate::config::paths::Layout;
use crate::platform::fs as platform_fs;
use crate::storage::dump::{DumpKind, dump_session};
use crate::storage::session::Session;

enum Action {
    /// Prints the session artifact folder (Dial's artifact store) without opening it.
    Artifacts,
    /// Writes a full session archive via `storage::dump`.
    Dump,
    /// Prints the process and environment facts Dial can read without a hardware probe.
    System,
    /// Exports the session as plain text to a temp file.
    Transcript,
    /// Action with no Dial equivalent; the reason is shown instead of running it.
    Unavailable(&'static str),
}

struct Item {
    name: &'static str,
    label: &'static str,
    action: Action,
}

/// Debug action items, in display order.
const ITEMS: &[Item] = &[
    Item {
        name: "open-artifacts",
        label: "Open: artifact folder",
        action: Action::Artifacts,
    },
    Item {
        name: "performance",
        label: "Report: performance issue",
        action: Action::Unavailable("Dial has no CPU profiler or performance bundle"),
    },
    Item {
        name: "work",
        label: "Profile: work scheduling",
        action: Action::Unavailable("Dial has no work-scheduling profiler"),
    },
    Item {
        name: "dump",
        label: "Report: dump session",
        action: Action::Dump,
    },
    Item {
        name: "memory",
        label: "Report: memory issue",
        action: Action::Unavailable("Dial collects no memory statistics"),
    },
    Item {
        name: "logs",
        label: "View: recent logs",
        action: Action::Unavailable("Dial keeps no in-memory debug log buffer to view"),
    },
    Item {
        name: "system",
        label: "View: system info",
        action: Action::System,
    },
    Item {
        name: "terminal",
        label: "View: terminal state",
        action: Action::Unavailable("terminal geometry and protocol state live in the TUI layer"),
    },
    Item {
        name: "protocols",
        label: "Test: terminal protocols",
        action: Action::Unavailable("Dial has no terminal protocol probe"),
    },
    Item {
        name: "raw-sse",
        label: "View: raw SSE stream",
        action: Action::Unavailable("Dial keeps no raw SSE frame buffer"),
    },
    Item {
        name: "remote-debugger",
        label: "Start: JS remote debugger",
        action: Action::Unavailable("the JavaScriptCore inspector is Bun-specific"),
    },
    Item {
        name: "transcript",
        label: "Export: TUI transcript",
        action: Action::Transcript,
    },
    Item {
        name: "clear-cache",
        label: "Clear: artifact cache",
        action: Action::Unavailable("Dial keeps no separate artifact cache to clear"),
    },
];

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let name = args.trim();
    if name.is_empty() {
        shell.note("Debug", overview(&agent.debug_snapshot()));
        return Ok(Step::Done);
    }
    let Some(item) = ITEMS.iter().find(|item| item.name == name) else {
        let names: Vec<&str> = ITEMS.iter().map(|item| item.name).collect();
        shell.note(
            "Debug",
            format!(
                "unknown debug item `{name}`; choose one of: {}",
                names.join(", ")
            ),
        );
        return Ok(Step::Done);
    };
    let session = agent.session();
    let text = match &item.action {
        Action::Artifacts => artifacts_text(session),
        Action::Dump => dump_text(&app.layout, session),
        Action::System => system_text(&app.layout.project_root),
        Action::Transcript => transcript_text(session),
        Action::Unavailable(reason) => Ok(format!("unavailable: {reason}")),
    };
    match text {
        Ok(text) => shell.note(item.label, text),
        Err(error) => shell.note(item.label, format!("failed: {error:#}")),
    }
    Ok(Step::Done)
}

/// The session's artifact folder: `<project>/artifacts`, the sibling of `sessions/` that
/// `Session::fit_oversized_message` writes into.
fn artifact_dir(session: &Session) -> Option<PathBuf> {
    let path = session.path()?;
    Some(path.parent()?.parent()?.join("artifacts"))
}

fn artifacts_text(session: &Session) -> Result<String> {
    let Some(dir) = artifact_dir(session) else {
        return Ok("in-memory session: no artifact folder".to_owned());
    };
    let state = if dir.exists() {
        "exists"
    } else {
        "not created yet"
    };
    Ok(format!(
        "{} ({state}); printed only, not opened",
        dir.display()
    ))
}

fn dump_text(layout: &Layout, session: &Session) -> Result<String> {
    let Some(path) = session.path() else {
        return Ok("in-memory session: nothing to dump".to_owned());
    };
    let out_dir = scratch_path("dial-debug-dump", session.id());
    platform_fs::create_dir_owner_only(fs::DirBuilder::new().recursive(true), &out_dir)?;
    let report = dump_session(layout, path, DumpKind::All, &out_dir)?;
    Ok(format!(
        "Archive: {}\nEntries: {}\nReview before sharing: the archive holds session content and may contain secrets.",
        report.archive.display(),
        report.entries.len(),
    ))
}

fn transcript_text(session: &Session) -> Result<String> {
    let path = scratch_path("dial-transcript", session.id()).with_extension("txt");
    session.export(&path, false)?;
    Ok(format!("Transcript: {}", path.display()))
}

fn system_text(project_root: &Path) -> Result<String> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "n/a".to_owned());
    let terminal = env::var("TERM_PROGRAM")
        .or_else(|_| env::var("TERM"))
        .unwrap_or_else(|_| "n/a".to_owned());
    Ok(format!(
        "OS: {} {}\nDial: {}\nProject root: {}\nShell: {shell}\nTerminal: {terminal}\nCPU model and memory: not collected (no hardware probe in Dial)",
        env::consts::OS,
        env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        project_root.display(),
    ))
}

/// A unique temp path for one debug artifact.
fn scratch_path(prefix: &str, session_id: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    env::temp_dir().join(format!("{prefix}-{session_id}-{nanos}"))
}

/// The `/debug` overview: the action list, then the facts behind it.
fn overview(snapshot: &DebugSnapshot) -> String {
    let mut out = String::from("Available (run with /debug <item>):\n");
    let mut missing = String::new();
    for item in ITEMS {
        match &item.action {
            Action::Unavailable(reason) => {
                let _ = writeln!(missing, "  {:<16} {:<28} {reason}", item.name, item.label);
            }
            action => {
                let status = match action {
                    Action::Artifacts => "prints the artifact folder path",
                    _ => "runs now",
                };
                let _ = writeln!(out, "  {:<16} {:<28} {status}", item.name, item.label);
            }
        }
    }
    if !missing.is_empty() {
        out.push_str("Not available in Dial:\n");
        out.push_str(&missing);
    }

    let file = snapshot
        .session_path
        .as_deref()
        .map_or("in-memory".to_owned(), |path| path.display().to_string());
    let leaf = snapshot.branch_leaf.as_deref().unwrap_or("none");
    let _ = write!(
        out,
        "\nSession\n  id: {}\n  file: {file}\n  messages: {}\n  branch leaf: {leaf}\n  version: {} ({}, {})\n",
        snapshot.session_id,
        snapshot.messages,
        snapshot.version_id,
        snapshot.version_kind,
        snapshot.version_status,
    );

    let measured = if snapshot.context_estimated {
        "estimated"
    } else {
        "measured"
    };
    let _ = write!(
        out,
        "\nModel\n  backend: {}\n  context window: {} tokens\n  context used: {} tokens ({measured})\n",
        snapshot.backend, snapshot.context_window, snapshot.context_tokens,
    );

    let cost = snapshot
        .session_cost
        .map_or("n/a".to_owned(), |cost| format!("${cost:.4}"));
    let _ = write!(
        out,
        "\nUsage\n  responses: {}\n  prompt tokens: {}\n  output tokens: {}\n  cached tokens: {}\n  cache write tokens: {}\n  reasoning tokens: {}\n  compactions: {}\n  turns completed: {}\n  tool contract errors: {}\n  session cost: {cost}\n",
        snapshot.responses,
        snapshot.prompt_tokens,
        snapshot.output_tokens,
        snapshot.cached_tokens,
        snapshot.cache_write_tokens,
        snapshot.reasoning_tokens,
        snapshot.compactions,
        snapshot.turns_completed,
        snapshot.tool_contract_errors,
    );

    let _ = write!(
        out,
        "\nSettings\n  plan mode: {}\n",
        if snapshot.plan_mode { "on" } else { "off" },
    );

    let _ = write!(
        out,
        "\nTools\n  offered: {} ({})\n  available: {}\n",
        snapshot.tools_offered.len(),
        snapshot.tools_offered.join(", "),
        snapshot.tools_available,
    );
    out
}

#[cfg(test)]
mod tests;
