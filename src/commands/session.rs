use crate::agent::Agent;
use crate::app::{App, AuthJob, Shell, Step};
use crate::llm::{chatgpt, oauth};
use crate::stats::{self, Db};
use crate::storage::{
    paths::Layout,
    session::{Session, SessionMeta},
    session_pins,
};
use crate::ui::text::one_line;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "branch",
        description: "List user messages to branch from, or rewind to one (keeps the old path as a branch; alias: rewind)",
        args_hint: "[n]",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "delete",
        description: "Delete the current session and start a new one",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "pin",
        description: "Pin or unpin a session at the top of the resume list",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "move",
        description: "Move the current session to a different directory",
        args_hint: "<dir>",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "restart",
        description: "Restart dial with the same launch flags, resuming this session",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "retry",
        description: "Retry the last failed agent turn",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "stats",
        description: "Show local usage statistics; --port starts the loopback dashboard",
        args_hint: "[range] [--port <port>]",
        subcommands: &["--port"],
    },
    super::CommandSpec {
        name: "login",
        description: "Sign in to an OAuth provider (no argument lists providers)",
        args_hint: "[provider]",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "logout",
        description: "Sign out of an OAuth provider (no argument lists signed-in providers)",
        args_hint: "[provider]",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "session",
        description: "Show the current session, or manage it (info, delete, pin)",
        args_hint: "[info|delete|pin]",
        subcommands: &["info", "delete", "pin"],
    },
    super::CommandSpec {
        name: "changelog",
        description: "Show the Dial changelog: `full`, or `last [N]` (default 1)",
        args_hint: "[full|last [N]]",
        subcommands: &["full", "last"],
    },
];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let args = args.trim();
    let result = match name {
        "branch" | "rewind" => branch(shell, app, agent, args),
        "retry" => retry(shell, app, agent),
        "session" => session(shell, app, agent, args),
        "login" => login(shell, app, args),
        "logout" => logout(shell, app, args),
        "stats" => stats_command(shell, app, args),
        "changelog" => {
            shell.note("Changelog", crate::changelog::render(args));
            Ok(Step::Done)
        }
        "delete" => delete_current(shell, app, agent),
        "pin" => pin_current(shell, app, agent),
        "move" => move_current(shell, agent, args),
        "restart" => restart(shell),
        _ => return None,
    };
    Some(result)
}

pub(super) fn completions(_config: &crate::config::types::Config) -> Vec<String> {
    oauth::flows()
        .flat_map(|flow| {
            [
                format!("/login {}", flow.id),
                format!("/logout {}", flow.id),
            ]
        })
        .collect()
}

/// Quit this session and relaunch the binary after the terminal is restored (`run_interactive`).
fn restart(shell: &mut Shell) -> Result<Step> {
    shell.restart_requested = true;
    Ok(Step::Quit)
}

/// Delete the current session file, start a fresh session, and drop its pin (`/delete`).
fn delete_current(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    let Some(old_path) = agent.session().path().map(|p| p.to_path_buf()) else {
        shell.note("Delete", "No session file to delete (in-memory session).");
        return Ok(Step::Done);
    };
    let old_id = agent.session().id().to_owned();
    let layout = agent
        .layout()
        .ok_or_else(|| anyhow::anyhow!("agent has no project layout"))?;
    let fresh = Session::create(
        layout,
        SessionMeta {
            model: agent.model().to_owned(),
        },
    )?;
    let fresh_id = fresh.id().to_owned();
    let old = agent.swap_session(fresh);
    agent.set_plan_mode(false);
    shell.stop_loop_for_session_change();
    shell.session_id = fresh_id.clone();
    old.remove_file()?;
    session_pins::unpin(&app.layout.user_dir, &old_id)?;
    shell.note(
        "Delete",
        format!(
            "Deleted session {old_id} ({}).\nStarted new session {fresh_id}.",
            old_path.display()
        ),
    );
    Ok(Step::Done)
}

/// Toggle this session's pin in the global pin list (`/pin`).
fn pin_current(shell: &mut Shell, app: &App, agent: &Agent<'_>) -> Result<Step> {
    let id = agent.session().id().to_owned();
    let text = if session_pins::toggle(&app.layout.user_dir, &id)? {
        format!("Pinned session {id}; it now sorts first in the session list.")
    } else {
        format!("Unpinned session {id}.")
    };
    shell.note("Pin", text);
    Ok(Step::Done)
}

/// Pin a stored ChatGPT OAuth account to this session (`/session pin [account]`). With no
/// selector, lists stored accounts. The choice is written to the session's provider identity, so
/// it survives resume, and the live backend is rebound at once. A rebind refusal (for example a
/// different account over native replay history) restores the previous identity.
fn pin_account(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    selector: &str,
) -> Result<Step> {
    if shell.status.phase != crate::ui::status::Phase::Idle {
        shell.note(
            "Pin",
            "Cannot pin an account while the session is streaming.",
        );
        return Ok(Step::Done);
    }
    let provider = app.config.selection().provider;
    if provider != chatgpt::PROVIDER {
        shell.note(
            "Pin",
            format!(
                "No pinnable OAuth accounts for {provider}; only ChatGPT keeps several stored accounts."
            ),
        );
        return Ok(Step::Done);
    }
    let accounts = chatgpt::accounts(&app.layout.user_dir)?;
    if selector.is_empty() {
        shell.note("Pin", account_list(&accounts));
        return Ok(Step::Done);
    }
    let account = match_account(&accounts, selector)?;
    if !account.signed_in {
        bail!(
            "{} is not signed in; sign in to it with /login before pinning",
            account.label
        );
    }
    let previous = agent.session().control_state();
    let mut state = if previous.is_object() {
        previous.clone()
    } else {
        serde_json::json!({})
    };
    let mut identity = state
        .get("provider_identity")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let mut selection = identity
        .get("selection")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    selection["account_id"] = Value::String(account.client_id.clone());
    identity["selection"] = selection;
    state["provider_identity"] = identity;
    agent.session_mut().set_control_state(state)?;
    if let Err(error) = agent.rebind_session() {
        agent.session_mut().set_control_state(previous)?;
        let _ = agent.rebind_session();
        return Err(error.context("account pin not applied; previous account kept"));
    }
    shell.note(
        "Pin",
        format!("Pinned {} to this session for {provider}.", account.label),
    );
    Ok(Step::Done)
}

fn account_list(accounts: &[chatgpt::AccountInfo]) -> String {
    if accounts.is_empty() {
        return "No stored ChatGPT accounts. Sign in first with /login.".to_owned();
    }
    let mut out = String::from("Stored ChatGPT accounts:");
    for (index, account) in accounts.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}{}{}",
            index + 1,
            account.label,
            if account.active { " (active)" } else { "" },
            if account.signed_in {
                ""
            } else {
                " (signed out)"
            },
        ));
    }
    out.push_str("\nPin one with /session pin <number|account id|active|email>.");
    out
}

/// Resolve a selector by 1-based position, `active`, account id, or a unique label substring.
fn match_account<'a>(
    accounts: &'a [chatgpt::AccountInfo],
    selector: &str,
) -> Result<&'a chatgpt::AccountInfo> {
    if let Ok(position) = selector.parse::<usize>() {
        return position
            .checked_sub(1)
            .and_then(|index| accounts.get(index))
            .with_context(|| format!("No ChatGPT account numbered {selector}"));
    }
    if selector.eq_ignore_ascii_case("active") {
        return accounts
            .iter()
            .find(|account| account.active)
            .context("No active ChatGPT account");
    }
    let needle = selector.to_lowercase();
    let found: Vec<&chatgpt::AccountInfo> = accounts
        .iter()
        .filter(|account| {
            account.client_id == selector || account.label.to_lowercase().contains(&needle)
        })
        .collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => bail!("No ChatGPT account matches '{selector}'; list them with /session pin"),
        _ => bail!("'{selector}' matches several ChatGPT accounts; use its number"),
    }
}

/// Relocate this session into another directory's project (`/move`). The journal is copied
/// with its header cwd rewritten and the original removed, then the live tool workspace is
/// re-rooted to the new directory through `Agent::reroot_workspace`.
fn move_current(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    if args.is_empty() {
        shell.note("Move", "Usage: /move <path>");
        return Ok(Step::Done);
    }
    match move_session(agent, args)? {
        MoveOutcome::Moved(root) => {
            shell.stop_loop_for_session_change();
            shell.note(
                "Move",
                format!("Moved to {}. Tools now operate there.", root.display()),
            )
        }
        MoveOutcome::Refused(reason) => shell.note("Move", reason),
    }
    Ok(Step::Done)
}

/// Whether [`move_session`] moved the session, or refused and left it where it was.
pub(super) enum MoveOutcome {
    /// The session now lives in this project root.
    Moved(PathBuf),
    /// Nothing moved; the message says why.
    Refused(String),
}

/// Relocate this session into the directory `args` names: the journal is copied with its header
/// cwd rewritten and the original removed, then the live tool workspace is re-rooted through
/// `Agent::reroot_workspace`. Refusals come back as [`MoveOutcome::Refused`], never as success.
pub(super) fn move_session(agent: &mut Agent<'_>, args: &str) -> Result<MoveOutcome> {
    let current = agent
        .layout()
        .ok_or_else(|| anyhow::anyhow!("agent has no project layout"))?
        .clone();
    let requested = PathBuf::from(args);
    let resolved = if requested.is_absolute() {
        requested
    } else {
        current.project_root.join(requested)
    };
    let target_dir = match resolved.canonicalize() {
        Ok(dir) if dir.is_dir() => dir,
        _ => {
            return Ok(MoveOutcome::Refused(format!(
                "Directory does not exist: {}",
                resolved.display()
            )));
        }
    };
    if agent.session().path().is_none() {
        return Ok(MoveOutcome::Refused(
            "No session file to move (in-memory session).".to_owned(),
        ));
    }
    let target = Layout::with_user_dir(&target_dir, current.user_dir.clone())?;
    if target.sessions_dir() == current.sessions_dir() {
        return Ok(MoveOutcome::Refused(format!(
            "The session already belongs to {}.",
            current.project_root.display()
        )));
    }
    let moved = agent.session().relocate_to(&target)?;
    let old = agent.swap_session(moved);
    agent.reroot_workspace(&target)?;
    old.remove_file()?;
    Ok(MoveOutcome::Moved(target.project_root.clone()))
}

/// Text of a session message: plain string content or joined `text` parts.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn preview(text: &str) -> String {
    one_line(text).chars().take(80).collect()
}

fn branch(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let turns = agent.session().branchable_user_messages();
    if args.is_empty() {
        let body = if turns.is_empty() {
            "No user messages to branch from.".to_owned()
        } else {
            let mut lines: Vec<String> = turns
                .iter()
                .rev()
                .enumerate()
                .map(|(index, (_, text))| format!("{}. {}", index + 1, preview(text)))
                .collect();
            lines.push("Usage: /branch <n> rewinds to message n (1 = most recent).".to_owned());
            lines.join("\n")
        };
        shell.note("Branch", body);
        return Ok(Step::Done);
    }
    let n: usize = match args.parse() {
        Ok(n) if (1..=turns.len()).contains(&n) => n,
        _ => {
            shell.note(
                "Branch",
                format!("Expected a message number from 1 to {}.", turns.len()),
            );
            return Ok(Step::Done);
        }
    };
    let target = preview(&turns[turns.len() - n].1);
    agent.cancel_background_compaction();
    match super::branch::rewind(app, agent, n) {
        Ok(rewound) => {
            shell.show_session(agent.session());
            let mut body = format!(
                "rewound {n} turn(s), dropped {} message(s); old path kept as a branch\nat: {target}",
                rewound.dropped
            );
            if let Some(note) = rewound.note {
                body.push('\n');
                body.push_str(&note);
            }
            shell.note("Branch", body);
        }
        Err(error) => shell.note("Branch", format!("rewind: {error:#}")),
    }
    Ok(Step::Done)
}

fn retry(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    let messages = agent.session().messages();
    let Some(last) = messages.last() else {
        shell.note("Retry", "Nothing to retry: the session is empty.");
        return Ok(Step::Done);
    };
    if last["role"].as_str() != Some("user") {
        shell.note(
            "Retry",
            "No failed turn: the last message is not an unanswered prompt.",
        );
        return Ok(Step::Done);
    }
    let text = message_text(last);
    agent.cancel_background_compaction();
    let rewound = match super::branch::rewind(app, agent, 1) {
        Ok(rewound) => rewound,
        Err(error) => {
            shell.note("Retry", format!("rewind: {error:#}"));
            return Ok(Step::Done);
        }
    };
    if let Some(note) = rewound.note {
        shell.note("Retry", note);
    }
    shell.show_session(agent.session());
    shell.run_turn(app, agent, &text)
}

fn session(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let (sub, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(sub, rest)| (sub, rest.trim()));
    match sub {
        "" | "info" => {
            let session = agent.session();
            let path = session.path().map_or_else(
                || "not saved (in-memory session)".to_owned(),
                |path| {
                    let path = path.display().to_string();
                    match std::env::var("HOME").ok().filter(|home| !home.is_empty()) {
                        Some(home) => match path.strip_prefix(home.trim_end_matches('/')) {
                            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
                            _ => path,
                        },
                        None => path,
                    }
                },
            );
            let title = session.title();
            let title = if title.trim().is_empty() {
                "untitled (name it with /rename <title>)".to_owned()
            } else {
                title
            };
            shell.note(
                "Session",
                format!(
                    "id: {} · title: {title}\nfile: {path}\n/rename names it · /tree browses branches · /session delete removes it",
                    session.id(),
                ),
            );
        }
        "delete" => return delete_current(shell, app, agent),
        "pin" => return pin_account(shell, app, agent, rest),
        other => shell.note(
            "Session",
            format!("Unknown subcommand '{other}'. Usage: /session [info|delete|pin]"),
        ),
    }
    Ok(Step::Done)
}

fn login(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let user_dir = app.layout.user_dir.clone();
    if args.is_empty() {
        let lines: Vec<String> = oauth::flows()
            .map(|flow| {
                let state = if oauth::signed_in(&user_dir, flow.id) {
                    "signed in"
                } else {
                    "signed out"
                };
                format!("{}  {}  {state}", flow.id, flow.name)
            })
            .collect();
        shell.note(
            "Login",
            format!("Usage: /login <provider>\n{}", lines.join("\n")),
        );
        return Ok(Step::Done);
    }
    let Some(target) = oauth::flow(args) else {
        shell.note(
            "Login",
            format!("Unknown provider '{args}'. Run /login to list providers."),
        );
        return Ok(Step::Done);
    };
    if shell.auth_job.is_some() {
        shell.note("Login", "An authentication action is already running");
        return Ok(Step::Done);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (send, receiver) = std::sync::mpsc::channel();
    let id = target.id;
    std::thread::spawn(move || {
        let ui = oauth::BannerInteraction { id };
        let _ = send.send(oauth::login(&user_dir, id, &ui, worker_cancel));
    });
    shell.auth_job = Some(AuthJob {
        label: target.name,
        receiver,
        cancel,
        lines: None,
    });
    shell.note(
        target.name,
        format!(
            "Waiting for {} sign-in in your browser; sign-in continues if you dismiss this message",
            target.name
        ),
    );
    Ok(Step::Done)
}

fn logout(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let user_dir = app.layout.user_dir.clone();
    if args.is_empty() {
        let lines: Vec<String> = oauth::flows()
            .filter(|flow| oauth::signed_in(&user_dir, flow.id))
            .map(|flow| format!("{}  {}", flow.id, flow.name))
            .collect();
        let body = if lines.is_empty() {
            "No signed-in providers.".to_owned()
        } else {
            format!("Usage: /logout <provider>\n{}", lines.join("\n"))
        };
        shell.note("Logout", body);
        return Ok(Step::Done);
    }
    let Some(target) = oauth::flow(args) else {
        shell.note(
            "Logout",
            format!("Unknown provider '{args}'. Run /logout to list signed-in providers."),
        );
        return Ok(Step::Done);
    };
    let text = match oauth::sign_out(&user_dir, target.id) {
        Ok(message) => message,
        Err(error) => format!("error: {error:#}"),
    };
    shell.note(target.name, text);
    Ok(Step::Done)
}

fn stats_command(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let mut port = None;
    let mut range = Vec::new();
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--port" => {
                let value = tokens.next().context("--port needs a value")?;
                port = Some(
                    value
                        .parse::<u16>()
                        .context("--port must be a port number")?,
                );
            }
            "--host" => bail!("--host is not supported: the stats dashboard binds 127.0.0.1 only"),
            other => range.push(other),
        }
    }
    let user_dir = app.layout.user_dir.clone();
    if let Some(port) = port {
        let listener = stats::server::bind_loopback(port)?;
        let address = listener.local_addr()?;
        let serve_dir = user_dir;
        std::thread::spawn(move || {
            let _ = stats::server::serve_listener(listener, serve_dir);
        });
        shell.note("Stats", format!("Dashboard: http://{address}"));
        return Ok(Step::Done);
    }
    let since = stats::parse_range(&range.join(" "))?;
    let mut db = Db::open_default(&user_dir)?;
    db.sync_sessions(&user_dir)?;
    let report = db.report(since, stats::now_ms())?;
    shell.note("Stats", stats::render_text(&report));
    Ok(Step::Done)
}
