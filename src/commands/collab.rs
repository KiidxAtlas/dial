//! Collaboration slash commands: /collab, /join, /leave, /share, /dump, /trace.
use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::collab::registry;
use crate::collab::{
    guest::Guest,
    host::Host,
    room::{self, Room},
};
use crate::storage::share;
use anyhow::{Context, Result, bail};

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "collab",
        description: "host a live collab session; status and stop manage it",
        args_hint: "[view|status|stop|list|abort]",
        subcommands: &["view", "status", "stop", "list", "abort"],
    },
    super::CommandSpec {
        name: "join",
        description: "Join a collab session from a link",
        args_hint: "<link>",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "leave",
        description: "Leave the collab session you joined",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "share",
        description: "upload this session encrypted and print the link",
        args_hint: "[gist] [whole-journal]",
        subcommands: &["gist", "whole-journal"],
    },
    super::CommandSpec {
        name: "dump",
        description: "Write this session's transcript to a file and print its path",
        args_hint: "[all|anon]",
        subcommands: &["all", "anon"],
    },
    super::CommandSpec {
        name: "trace",
        description: "Open this session in the stats dashboard",
        args_hint: "",
        subcommands: &[],
    },
];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let result = match name {
        "collab" => collab(shell, app, agent, args),
        "join" => join(shell, app, args),
        "leave" => leave(shell),
        "share" => share_session(shell, app, agent, args),
        "dump" => dump(shell, app, agent, args),
        "trace" => trace(shell, app, agent),
        _ => return None,
    };
    Some(result)
}

pub(super) fn completions(_config: &crate::config::types::Config) -> Vec<String> {
    [
        "/join",
        "/collab view",
        "/collab status",
        "/collab stop",
        "/share gist",
        "/share whole-journal",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

fn collab(shell: &mut Shell, app: &App, agent: &Agent<'_>, args: &str) -> Result<Step> {
    match args.trim() {
        "" | "start" => start(shell, app, agent, false),
        "view" => start(shell, app, agent, true),
        "status" => status(shell),
        "list" => list(shell, app),
        "stop" => stop(shell),
        "abort" => guest_abort(shell),
        _ => {
            shell.note("Collab", "Usage: /collab [view|status|stop|list|abort]");
            Ok(Step::Done)
        }
    }
}

fn start(shell: &mut Shell, app: &App, agent: &Agent<'_>, read_only: bool) -> Result<Step> {
    // The room lock covers only this check; the room is opened, published and stored on the job thread.
    let busy = room::with(|slot| match slot {
        Some(Room::Guest(_)) => Some("Already in a collab session as a guest (/leave first)"),
        Some(Room::Host(_)) => Some("Stop hosting first (/collab stop)"),
        None => None,
    });
    if let Some(message) = busy {
        shell.note("Collab", message);
        return Ok(Step::Done);
    }
    let Some(layout) = agent.layout() else {
        shell.note("Collab", "agent has no project layout");
        return Ok(Step::Done);
    };
    let config = &app.config.collab;
    let relay = config.relay_url.clone();
    let allow_steering = config.allow_guest_steering;
    let messages = agent.session().archived_messages();
    let user_dir = layout.user_dir.clone();
    let cwd = layout.project_root.display().to_string();
    let session_id = agent.session().id().to_owned();
    let model = agent.model().to_owned();
    super::extensions::spawn_job(shell, "Collab", move |_cancel| {
        let mut host = Host::start(&relay, allow_steering, !read_only, messages)?;
        if let Err(error) = host.publish(&user_dir, &cwd, &session_id, &model) {
            host.stop();
            return Err(error);
        }
        let link = host.link().to_owned();
        // Re-check under the lock: a guest or host may have claimed the room while this opened.
        let claimed = room::with(|slot| {
            if slot.is_some() {
                return Err(host);
            }
            *slot = Some(Room::Host(host));
            Ok(())
        });
        if let Err(host) = claimed {
            host.stop();
            bail!("Another collab session started while hosting; this room was closed");
        }
        Ok(if read_only {
            format!(
                "Read-only collab session active. Anyone with the link can read the session but cannot prompt the agent:\n{link}"
            )
        } else {
            format!(
                "Collab session started!\nAnyone with the link can read the session and prompt the agent. Read-only link: /collab view\n{link}"
            )
        })
    });
    Ok(Step::Done)
}

/// List live collab hosts on this machine from the local directory: metadata only, never links
/// (`/collab list`).
fn list(shell: &mut Shell, app: &App) -> Result<Step> {
    let hosts = registry::list_live(&app.layout.user_dir)?;
    if hosts.is_empty() {
        shell.note("Collab", "No live collab hosts on this machine.");
        return Ok(Step::Done);
    }
    let mut lines = vec![format!("{} live collab host(s):", hosts.len())];
    for host in hosts {
        lines.push(format!(
            "- {} · session {} · {} · model {} · {} · guests {}",
            host.id,
            host.session_id,
            host.cwd,
            host.model,
            if host.read_only {
                "read-only"
            } else {
                "read-write"
            },
            host.guests,
        ));
    }
    shell.note("Collab", lines.join("\n"));
    Ok(Step::Done)
}

fn status(shell: &mut Shell) -> Result<Step> {
    let notes = room::pump();
    let text = room::with(|slot| match slot {
        None => "Not in a collab session".to_owned(),
        Some(Room::Host(host)) if host.is_read_only() => format!(
            "Read-only collab session active ({} guest{} connected)",
            host.guest_count(),
            plural(host.guest_count())
        ),
        Some(Room::Host(host)) => format!(
            "Collab session active ({} guest{} connected)",
            host.guest_count(),
            plural(host.guest_count())
        ),
        Some(Room::Guest(guest)) if guest.ended() => {
            "The collab session has ended (/leave to exit)".to_owned()
        }
        Some(Room::Guest(guest)) if guest.is_read_only() => format!(
            "In a collab session as a read-only guest ({} entries replicated; prompts stay on the host, /leave to exit)",
            guest.replicated_entries()
        ),
        Some(Room::Guest(guest)) => format!(
            "In a collab session as a guest ({} entries replicated; prompts go to the host, /collab abort stops its turn, /leave to exit)",
            guest.replicated_entries()
        ),
    });
    let mut lines = vec![text];
    lines.extend(notes);
    shell.note("Collab", lines.join("\n"));
    Ok(Step::Done)
}

fn stop(shell: &mut Shell) -> Result<Step> {
    let outcome = room::with(|slot| match slot.take() {
        Some(Room::Host(host)) => {
            host.stop();
            Ok("Collab stopped")
        }
        Some(other) => {
            *slot = Some(other);
            Err("Not hosting a collab session (use /leave to exit a guest session)")
        }
        None => Err("Not in a collab session"),
    });
    match outcome {
        Ok(text) | Err(text) => shell.note("Collab", text),
    }
    Ok(Step::Done)
}

fn join(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let link = args.trim();
    if link.is_empty() {
        shell.note("Collab", "Usage: /join <link>");
        return Ok(Step::Done);
    }
    let name = std::env::var("USER")
        .ok()
        .filter(|user| !user.trim().is_empty())
        .unwrap_or_else(|| "guest".to_owned());
    // The room lock covers only this check; the connection itself runs on the job thread.
    let busy = room::with(|slot| match slot {
        Some(Room::Guest(_)) => Some("Already in a collab session as a guest (/leave first)"),
        Some(Room::Host(_)) => Some("Stop hosting first (/collab stop)"),
        None => None,
    });
    if let Some(message) = busy {
        shell.note("Collab", message);
        return Ok(Step::Done);
    }
    let relay = app.config.collab.relay_url.clone();
    let link = link.to_owned();
    super::extensions::spawn_job(shell, "Collab", move |_cancel| {
        let guest = Guest::join(&relay, &link, &name)?;
        // Re-check under the lock: a host may have claimed the room while the connection opened.
        room::with(|slot| -> Result<()> {
            if slot.is_some() {
                guest.leave();
                bail!(
                    "Another collab session started while joining; the joined session was closed"
                );
            }
            *slot = Some(Room::Guest(guest));
            Ok(())
        })?;
        Ok("Joining collab session; waiting for the host to admit you".to_owned())
    });
    Ok(Step::Done)
}

fn leave(shell: &mut Shell) -> Result<Step> {
    let outcome = room::with(|slot| match slot.take() {
        Some(Room::Guest(guest)) => {
            guest.leave();
            Ok("Left the collab session")
        }
        Some(other) => {
            *slot = Some(other);
            Err("This session is hosted here; use /collab stop")
        }
        None => Err("Not in a collab session"),
    });
    match outcome {
        Ok(text) | Err(text) => shell.note("Collab", text),
    }
    Ok(Step::Done)
}

/// Ask the host to abort its running turn (`/collab abort`, guests only).
fn guest_abort(shell: &mut Shell) -> Result<Step> {
    let text = match room::send_guest_abort() {
        None => "Not joined to a collab session as a guest".to_owned(),
        Some(Ok(())) => "Abort sent to the host".to_owned(),
        Some(Err(message)) => message.to_owned(),
    };
    shell.note("Collab", text);
    Ok(Step::Done)
}

fn share_session(shell: &mut Shell, app: &App, agent: &Agent<'_>, args: &str) -> Result<Step> {
    let mut gist = false;
    let mut scope = share::Scope::ActiveBranch;
    for word in args.split_whitespace() {
        match word {
            "gist" => gist = true,
            // Diagnostic: the whole journal, including abandoned branches and internal records.
            "whole-journal" => scope = share::Scope::WholeJournal,
            _ => {
                shell.note("Share", "Usage: /share [gist] [whole-journal]");
                return Ok(Step::Done);
            }
        }
    }
    // Snapshot the inputs so reading, encrypting and uploading run off the UI thread.
    let path = agent.session().path().map(|path| path.to_path_buf());
    let endpoint = app.config.share.server_url.clone();
    super::extensions::spawn_job(shell, "Share", move |_cancel| {
        let result = (|| -> Result<String> {
            let path = path.context("No session file yet — send a message first.")?;
            // Same payload as `dial share`: the active branch unless the diagnostic scope is named.
            let transcript = share::transcript(&path, scope)?;
            let encrypted = share::encrypt_transcript(&transcript)?;
            let link = if gist {
                share::upload_gist(&encrypted)?
            } else {
                anyhow::ensure!(
                    !endpoint.is_empty(),
                    "set [share].server_url, or use /share gist"
                );
                share::upload(&encrypted, &endpoint)?
            };
            Ok(format!(
                "Share URL: {link}\nNote: the key is only in the URL fragment; anyone with the full link can read the session."
            ))
        })();
        result.map_err(|error| anyhow::anyhow!("Failed to share session: {error:#}"))
    });
    Ok(Step::Done)
}

fn dump(shell: &mut Shell, app: &App, agent: &Agent<'_>, args: &str) -> Result<Step> {
    let kind = match args.trim() {
        "" => None,
        "all" => Some(crate::storage::dump::DumpKind::All),
        "anon" => Some(crate::storage::dump::DumpKind::Anon),
        _ => {
            shell.note("Dump", "Usage: /dump [all|anon]");
            return Ok(Step::Done);
        }
    };
    let session = agent.session();
    if session.message_count() == 0 {
        shell.note("Dump", "No messages to dump yet.");
        return Ok(Step::Done);
    }
    if let Some(kind) = kind {
        let Some(path) = session.path() else {
            shell.note("Dump", "No session file to dump (in-memory session).");
            return Ok(Step::Done);
        };
        match crate::storage::dump::dump_session(&app.layout, path, kind, &std::env::temp_dir()) {
            Ok(report) => shell.note("Dump", report.to_string()),
            Err(error) => shell.note("Dump", format!("Failed to write archive: {error:#}")),
        }
        return Ok(Step::Done);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("dial-dump-{}-{stamp}.md", session.id()));
    match session.export(&path, false) {
        Ok(()) => shell.note(
            "Dump",
            format!(
                "Transcript written to {}\nThis file persists on disk and may contain raw context and secrets; treat it accordingly.",
                path.display()
            ),
        ),
        Err(error) => shell.note("Dump", format!("Failed to write transcript: {error:#}")),
    }
    Ok(Step::Done)
}

fn trace(shell: &mut Shell, app: &App, agent: &Agent<'_>) -> Result<Step> {
    let Some(path) = agent.session().path() else {
        shell.note("Trace", "No session file to trace (in-memory session).");
        return Ok(Step::Done);
    };
    match crate::stats::trace_url(&app.layout.user_dir, path) {
        Ok(url) => {
            if crate::llm::oauth::util::open_browser(&url) {
                shell.note(
                    "Trace",
                    format!("Opened this session's trace in the browser:\n{url}"),
                );
            } else {
                shell.note(
                    "Trace",
                    format!("Open this session's trace in the stats dashboard:\n{url}"),
                );
            }
        }
        Err(error) => shell.note("Trace", format!("Trace unavailable: {error:#}")),
    }
    Ok(Step::Done)
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
