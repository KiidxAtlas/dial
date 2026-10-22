//! In-session access to features that were reachable only from the command line.
//!
//! - Launch-scoped modes: `/advisor`, `/external-thinking` and `/plan-yolo` toggle the same
//!   per-process switches as `--advisor`, `--external-thinking` and `--plan-yolo[-into]`.
//! - Maintenance subcommands: `/gc`, `/anonymize`, `/images`, `/update`, `/benchmark` and
//!   `/schedule run` run the matching `dial <subcommand>` as a background job. Its output streams
//!   into the transcript instead of the terminal the TUI owns, and Esc-style cancellation kills it.
//! - `/schedule list|add|remove|log` edits the project's schedules directly.
//! - `/relay start|stop|status` runs a local collaboration relay for `/collab` in the background.
//! - `/hooks` lists the configured lifecycle hooks.
use super::CommandSpec;
use super::extensions::spawn_job_streaming;
use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "advisor",
        description: "coach reviews each tool round and the final reply (on, completion, off, status)",
        args_hint: "on|completion|off|status",
        subcommands: &["on", "completion", "off", "status"],
    },
    CommandSpec {
        name: "external-thinking",
        description: "private scratchpad reasoning hidden from the transcript (on, off, status)",
        args_hint: "on|off|status",
        subcommands: &["on", "off", "status"],
    },
    CommandSpec {
        name: "plan-yolo",
        description: "plan mode whose next plan is auto-approved and handed to MODEL (default @small)",
        args_hint: "[MODEL|off|status]",
        subcommands: &["off", "status"],
    },
    CommandSpec {
        name: "schedule",
        description: "recurring or file-triggered prompt runs for this project",
        args_hint: "list|add|remove|log|run",
        subcommands: &["list", "add", "remove", "log", "run"],
    },
    CommandSpec {
        name: "gc",
        description: "sweep stale blobs, archives and WAL state (dry run unless `apply`)",
        args_hint: "[apply] [--blobs|--archive|--wal|--stale]",
        subcommands: &["apply"],
    },
    CommandSpec {
        name: "anonymize",
        description: "write an anonymized copy of a session (default: this one)",
        args_hint: "[SESSION] [-o PATH]",
        subcommands: &[],
    },
    CommandSpec {
        name: "images",
        description: "image cache: status, doctor, probe, purge [--apply] [--all]",
        args_hint: "[status|doctor|probe|purge]",
        subcommands: &["status", "doctor", "probe", "purge"],
    },
    CommandSpec {
        name: "update",
        description: "verify and install the latest signed release (then /restart)",
        args_hint: "[VERSION]",
        subcommands: &[],
    },
    CommandSpec {
        name: "benchmark",
        description: "run a coding benchmark suite in the background (same flags as `dial benchmark`)",
        args_hint: "[flags]",
        subcommands: &[],
    },
    CommandSpec {
        name: "relay",
        description: "local collaboration relay for /collab (start [ADDR], stop, status)",
        args_hint: "start [ADDR]|stop|status",
        subcommands: &["start", "stop", "status"],
    },
    CommandSpec {
        name: "hooks",
        description: "list configured lifecycle hooks",
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
    let args = args.trim();
    let result = match name {
        "advisor" => advisor(shell, agent, args),
        "external-thinking" => external_thinking(shell, agent, args),
        "plan-yolo" => plan_yolo(shell, agent, args),
        "anonymize" => anonymize(shell, app, agent, args),
        "benchmark" => benchmark(shell, app, args),
        _ => return None,
    };
    Some(result.map(|()| Step::Done))
}

/// The schedule editor, the hook listing, the relay and the `dial` subcommands that run as their
/// own background process. None uses the agent, so they run at once even during a turn.
/// `/benchmark` stays deferred: it drives models and would compete with the turn for compute.
pub(super) fn run_live(
    shell: &mut Shell,
    app: &App,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let result = match name {
        "schedule" => schedule(shell, app, args),
        "gc" => gc(shell, app, args),
        "images" => images(shell, app, args),
        "update" => update(shell, app, args),
        "relay" => relay(shell, app, args),
        "hooks" => hooks(shell, app),
        _ => return None,
    };
    Some(result.map(|()| Step::Done))
}

/// `on`, `off` or a status request; an empty argument is a status request.
fn toggle(args: &str, usage: &str) -> Result<Option<bool>> {
    match args {
        "on" | "enable" => Ok(Some(true)),
        "off" | "disable" => Ok(Some(false)),
        "" | "status" => Ok(None),
        _ => bail!("usage: {usage}"),
    }
}

fn advisor(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<()> {
    const TITLE: &str = "Advisor";
    let wanted = if args == "completion" {
        agent.set_advisor_completion_only();
        None
    } else {
        match toggle(args, "/advisor on|completion|off|status") {
            Ok(wanted) => wanted,
            Err(error) => {
                shell.note(TITLE, error.to_string());
                return Ok(());
            }
        }
    };
    if let Some(on) = wanted {
        agent.set_advisor(on);
    }
    let model = agent.advisor_model();
    let state = match (agent.advisor_enabled(), agent.advisor_completion_only()) {
        (false, _) => "off",
        (true, true) => "on (completion only)",
        (true, false) => "on",
    };
    let scope = if agent.advisor_completion_only() {
        "reviews tool evidence before completion"
    } else {
        "reviews each tool round (notes reach the next request; an unresolved blocker is re-raised once before completion) and reviews tool evidence before completion"
    };
    let text = match (agent.advisor_enabled(), model) {
        (true, Some(model)) => format!(
            "{state}: {model} {scope} and can request one repair pass, then reviews the result. Notes are unverified. Normal approvals and limits apply. Remote coaching requires escalation.allow_remote=true. For this process only."
        ),
        (true, None) => format!(
            "{state}, but no `coach` model role is assigned, so reviews are skipped. Assign one in /models → Roles."
        ),
        (false, Some(model)) => format!(
            "{state}. /advisor on reviews turns with {model}; /advisor completion reviews only before completion."
        ),
        (false, None) => {
            format!("{state}. Assign a `coach` model role in /models → Roles, then /advisor on.")
        }
    };
    shell.note(TITLE, text);
    Ok(())
}

fn external_thinking(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<()> {
    const TITLE: &str = "External thinking";
    let wanted = match toggle(args, "/external-thinking on|off|status") {
        Ok(wanted) => wanted,
        Err(error) => {
            shell.note(TITLE, error.to_string());
            return Ok(());
        }
    };
    if let Some(on) = wanted {
        agent.set_external_thinking(on);
    }
    shell.note(
        TITLE,
        if agent.external_thinking_enabled() {
            "on: provider reasoning is turned off and the model reasons in a <scratchpad> that is stripped before replies are shown or stored. Takes effect on the next request; for this process only."
        } else {
            "off: the model's own reasoning settings apply."
        },
    );
    Ok(())
}

fn plan_yolo(shell: &mut Shell, agent: &mut Agent<'_>, args: &str) -> Result<()> {
    const TITLE: &str = "Plan-yolo";
    match args {
        "status" => shell.note(
            TITLE,
            match agent.plan_yolo_target() {
                Some(target) => format!(
                    "armed: the next plan is approved without asking and handed to {target}."
                ),
                None => "not armed. /plan-yolo [MODEL] arms it (default: the small role, or the main model when none is assigned).".to_owned(),
            },
        ),
        "off" => {
            let was = agent.plan_yolo_disarm()?;
            shell.note(
                TITLE,
                if was {
                    "disarmed: the next plan asks for approval again. Plan mode is unchanged; /plan toggles it."
                } else {
                    "was not armed."
                },
            );
        }
        spec => {
            // Bare `/plan-yolo` prefers the `small` role and falls back to the main model.
            let fallback = spec.is_empty() && agent.prewalk_resolve("@small").is_err();
            let spec = match (spec.is_empty(), fallback) {
                (true, true) => "@main",
                (true, false) => "@small",
                (false, _) => spec,
            };
            match agent.plan_yolo_arm(spec) {
                Ok(target) => shell.note(
                    TITLE,
                    format!(
                        "armed: plan mode is on, and the next plan submitted is approved without asking and handed to {target}.{}",
                        if fallback {
                            "\nNo small role is assigned, so execution stays on the main model. Assign one in /models → Roles."
                        } else {
                            ""
                        }
                    ),
                ),
                Err(error) => shell.note(TITLE, format!("{error:#}")),
            }
        }
    }
    Ok(())
}

fn schedule(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    use crate::schedule::{self, Store};
    const TITLE: &str = "Schedule";
    const USAGE: &str = "/schedule list\n/schedule add --every 1h|--watch GLOB [--allow TOOL]... [--yes] [--id NAME] PROMPT\n/schedule remove ID\n/schedule log ID\n/schedule run   (runs due entries once, in the background)";
    let store = Store::new(&app.layout)?;
    let (action, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(action, rest)| (action, rest.trim()));
    match action {
        "" | "list" => {
            let state = store.last_runs();
            let entries = store.load()?;
            let text = if entries.is_empty() {
                format!("No schedules yet.\n{USAGE}")
            } else {
                entries
                    .iter()
                    .map(|entry| schedule::describe(entry, state.get(&entry.id).copied()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            shell.note(TITLE, text);
        }
        "add" => {
            let entry = parse_schedule_add(rest)?;
            if entry.prompt.is_empty() || (entry.every_secs.is_none() && entry.watch.is_empty()) {
                shell.note(TITLE, USAGE);
                return Ok(());
            }
            let access = entry.access_description();
            let id = store.add(entry)?;
            shell.note(
                TITLE,
                format!(
                    "Added {id} ({access}). Runs happen while `dial schedule run` (or /schedule run) is active."
                ),
            );
        }
        "remove" if !rest.is_empty() => {
            store.remove(rest)?;
            shell.note(TITLE, format!("Removed {rest}."));
        }
        "log" if !rest.is_empty() => shell.note(
            TITLE,
            std::fs::read_to_string(store.log_path(rest))
                .unwrap_or_else(|_| format!("No runs logged for {rest}.")),
        ),
        "run" => spawn_dial(
            shell,
            app,
            "Schedule",
            vec!["schedule".into(), "run".into(), "--once".into()],
            None,
        )?,
        _ => shell.note(TITLE, USAGE),
    }
    Ok(())
}

fn parse_schedule_add(args: &str) -> Result<crate::schedule::Entry> {
    let words = shlex::split(args).context("unbalanced quotes in /schedule add")?;
    let mut every = None;
    let mut watch = Vec::new();
    let mut allow = Vec::new();
    let mut id = None;
    let mut yes = false;
    let mut prompt = Vec::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let mut value = |flag: &str| {
            words
                .next()
                .with_context(|| format!("{flag} needs a value"))
        };
        match word.as_str() {
            "--every" => every = Some(value("--every")?),
            "--watch" => watch.push(value("--watch")?),
            "--allow" => allow.push(value("--allow")?),
            "--id" => id = Some(value("--id")?),
            "--yes" | "-y" => yes = true,
            "--" => {
                prompt.extend(words.by_ref());
                break;
            }
            _ => {
                prompt.push(word);
                prompt.extend(words.by_ref());
            }
        }
    }
    crate::schedule::Entry::new(id, prompt.join(" "), every.as_deref(), watch, allow, yes)
}

fn split_args(args: &str) -> Result<Vec<String>> {
    shlex::split(args).context("unbalanced quotes in the arguments")
}

fn gc(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    let mut argv = vec!["gc".to_owned()];
    for word in split_args(args)? {
        argv.push(if word == "apply" {
            "--apply".into()
        } else {
            word
        });
    }
    spawn_dial(shell, app, "GC", argv, None)
}

fn anonymize(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<()> {
    let mut words = split_args(args)?;
    if words.first().is_none_or(|word| word.starts_with('-')) {
        // The file path resolves even for sessions the listing skips (no titled user turn yet).
        let current = agent.session().path().map_or_else(
            || agent.session().id().to_owned(),
            |path| path.to_string_lossy().into_owned(),
        );
        words.insert(0, current);
    }
    let mut argv = vec!["anonymize".to_owned()];
    argv.extend(words);
    spawn_dial(shell, app, "Anonymize", argv, None)
}

fn images(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    let mut argv = vec!["images".to_owned()];
    argv.extend(split_args(args)?);
    spawn_dial(shell, app, "Images", argv, None)
}

fn update(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    let mut argv = vec!["update".to_owned()];
    argv.extend(split_args(args)?);
    spawn_dial(
        shell,
        app,
        "Update",
        argv,
        Some("Run /restart to start the updated binary in this session."),
    )
}

fn benchmark(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    let mut argv = vec!["benchmark".to_owned()];
    argv.extend(split_args(args)?);
    spawn_dial(shell, app, "Benchmark", argv, None)
}

/// Run `dial ARGS` from the project directory as a background job. Output lines stream into the
/// transcript under `label`; the job's cancellation kills the child process.
fn spawn_dial(
    shell: &mut Shell,
    app: &App,
    label: &'static str,
    argv: Vec<String>,
    after_success: Option<&'static str>,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating the dial executable")?;
    let project = app.layout.project_root.clone();
    let shown = format!("dial {}", argv.join(" "));
    spawn_job_streaming(shell, label, move |cancel: Arc<AtomicBool>, progress| {
        progress(&format!("$ {shown}"));
        let mut child = Command::new(&exe)
            .args(&argv)
            .current_dir(&project)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {shown}"))?;
        let (tx, rx) = mpsc::channel::<String>();
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|stream| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
        drop(tx);
        let status = loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                bail!("cancelled");
            }
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(line) => progress(&line),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break child.wait()?,
            }
            if let Some(status) = child.try_wait()? {
                for line in rx.try_iter() {
                    progress(&line);
                }
                break status;
            }
        };
        for reader in readers {
            let _ = reader.join();
        }
        for line in rx.try_iter() {
            progress(&line);
        }
        if status.success() {
            Ok(after_success.map_or_else(|| "Finished.".to_owned(), str::to_owned))
        } else {
            bail!("{shown} exited with {status}")
        }
    });
    Ok(())
}

/// The relay child started by `/relay start`, kept until `/relay stop` or exit.
static RELAY: Mutex<Option<(Child, String)>> = Mutex::new(None);

fn relay(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    const TITLE: &str = "Relay";
    let mut slot = RELAY
        .lock()
        .map_err(|_| anyhow::anyhow!("relay state is poisoned"))?;
    // Forget a relay that has exited on its own.
    if let Some((child, _)) = slot.as_mut()
        && child.try_wait()?.is_some()
    {
        *slot = None;
    }
    let (action, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(action, rest)| (action, rest.trim()));
    match action {
        "start" => {
            if let Some((_, bind)) = slot.as_ref() {
                shell.note(TITLE, format!("Already running on {bind}."));
                return Ok(());
            }
            let bind = if rest.is_empty() {
                "127.0.0.1:7331"
            } else {
                rest
            };
            bind.parse::<std::net::SocketAddr>()
                .with_context(|| format!("{bind:?} is not an address like 127.0.0.1:7331"))?;
            let child = Command::new(std::env::current_exe()?)
                .args(["relay", "--bind", bind])
                .current_dir(&app.layout.project_root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting the relay")?;
            *slot = Some((child, bind.to_owned()));
            let configured = &app.config.collab.relay_url;
            shell.note(
                TITLE,
                format!(
                    "Listening on {bind}. /collab uses {configured}; set collab.relay_url to ws://{bind} in /settings to host through it. Stops with /relay stop or when dial exits."
                ),
            );
        }
        "stop" => match slot.take() {
            Some((mut child, bind)) => {
                let _ = child.kill();
                let _ = child.wait();
                shell.note(TITLE, format!("Stopped the relay on {bind}."));
            }
            None => shell.note(TITLE, "No relay started from this session."),
        },
        "" | "status" => shell.note(
            TITLE,
            match slot.as_ref() {
                Some((_, bind)) => format!("Running on {bind}."),
                None => "Not running. /relay start [ADDR] starts one (default 127.0.0.1:7331)."
                    .to_owned(),
            },
        ),
        _ => shell.note(TITLE, "usage: /relay start [ADDR] | stop | status"),
    }
    Ok(())
}

/// Stops a relay started with `/relay start`; called when the TUI exits.
pub(crate) fn stop_relay() {
    if let Ok(mut slot) = RELAY.lock()
        && let Some((mut child, _)) = slot.take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn hooks(shell: &mut Shell, app: &App) -> Result<()> {
    let hooks = &app.config.hooks;
    let text = if hooks.is_empty() {
        "No lifecycle hooks are configured. Add `[[hooks]]` entries (event, matcher, command) in the user config; see README → Hooks.".to_owned()
    } else {
        hooks
            .iter()
            .map(|hook| {
                format!(
                    "{}{}: {} (timeout {}, {})",
                    hook.event,
                    hook.matcher
                        .as_deref()
                        .map(|m| format!(" [{m}]"))
                        .unwrap_or_default(),
                    hook.command.join(" "),
                    if hook.timeout_secs == 0 {
                        "none".to_owned()
                    } else {
                        format!("{}s", hook.timeout_secs)
                    },
                    match hook.fail_closed {
                        Some(true) => "fails closed",
                        Some(false) => "fails open",
                        None if hook.event == "pre_tool_use" => "fails closed",
                        None => "fails open",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    shell.note("Hooks", text);
    Ok(())
}

#[cfg(test)]
mod schedule_tests;
