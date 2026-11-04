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
//! - `/weights` shows stored weight results and opens their loopback dashboard at once;
//!   `/weights improve [--with MODEL] GOAL` has a model (picked when others are configured) run
//!   the whole edit loop on the current model's weights for a goal in plain words;
//!   `/weights map|stats|activations|trace|experiment|generate|direction|edit|evaluate` runs
//!   `dial weights` as a background job, and `/weights promote NAME|revert` switches this
//!   project's model to an edited copy that passed its evaluation, or back to its base.
use super::CommandSpec;
use super::extensions::spawn_job_streaming;
use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

mod watch_launch;

pub(super) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "watch",
        description: "open live edits, errors and screenshots in a separate terminal pane",
        args_hint: "",
        subcommands: &[],
    },
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
    CommandSpec {
        name: "weights",
        description: "inspect local MLX model weights and run evaluation-gated weight edit experiments",
        args_hint: "[status|open|next|map|analyze|stats|activations|trace|experiment NAME [--budget N]|generate|direction NAME|edit NAME|combine NAME --edits a,b|evaluate EDIT --cases FILE|history|show ID|compare A B|prune|cases --session ID|improve [--with MODEL] GOAL|promote EDIT|revert]",
        subcommands: &[
            "status",
            "open",
            "map",
            "analyze",
            "stats",
            "activations",
            "trace",
            "experiment",
            "generate",
            "direction",
            "edit",
            "evaluate",
            "combine",
            "history",
            "show",
            "compare",
            "prune",
            "cases",
            "next",
            "improve",
            "promote",
            "revert",
        ],
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
        "weights" if matches!(args.split_whitespace().next(), Some("promote" | "revert")) => {
            return Some(weights_switch(shell, app, agent, args));
        }
        "weights" if args.split_whitespace().next() == Some("improve") => {
            return Some(weights_improve(shell, app, agent, args));
        }
        "weights" => weights_job(shell, app, args),
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
        "watch" => watch(shell, app, args),
        "schedule" => schedule(shell, app, args),
        "gc" => gc(shell, app, args),
        "images" => images(shell, app, args),
        "update" => update(shell, app, args),
        "relay" => relay(shell, app, args),
        "hooks" => hooks(shell, app),
        "weights" if matches!(args, "" | "status" | "open") => weights(shell, app, args),
        _ => return None,
    };
    Some(result.map(|()| Step::Done))
}

fn watch(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    if !args.trim().is_empty() {
        shell.note("Watch", "usage: /watch");
        return Ok(());
    }
    crate::cli::watch::append(
        &app.layout,
        &shell.session_id,
        crate::cli::watch::Event::Text {
            title: "Dial live feed".into(),
            body: "Edits, reviews, screenshots and turn results appear here; the other tabs show the plan, jobs, workers, side threads and peers. Language-server advisories are not verification."
                .into(),
        },
    )?;
    let argv = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "-C".to_owned(),
        app.layout.project_root.to_string_lossy().into_owned(),
        "watch".to_owned(),
        shell.session_id.clone(),
    ];
    let command = shlex::try_join(argv.iter().map(String::as_str))?;
    match watch_launch::target() {
        Ok(Some(launcher)) => {
            // Keep the manual command visible even if the split fails or the job slot is busy.
            shell.note("Watch", watch_launch::opening_message(&launcher, &command));
            spawn_job_streaming(shell, "Watch", move |cancel, _progress| {
                Ok(match watch_launch::launch(&launcher, &argv, &cancel) {
                    Ok(()) => watch_launch::opened_message(&launcher),
                    Err(error) => watch_launch::failed_message(&launcher, &error, &command),
                })
            })
        }
        Ok(None) => shell.note("Watch", watch_launch::unavailable_message(None, &command)),
        Err(error) => shell.note(
            "Watch",
            watch_launch::unavailable_message(Some(&error), &command),
        ),
    }
    Ok(())
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
    let mut words = split_args(args)?;
    // `/update 0.2.8` names a version the way the hint shows; the CLI spells it `--version`.
    if let [version] = words.as_slice()
        && !version.starts_with('-')
    {
        words = vec!["--version".to_owned(), version.clone()];
    }
    argv.extend(words);
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

/// `/weights`, `/weights status` and `/weights open`: stored results only, never a model load.
fn weights(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    if args != "status" {
        let url = crate::weights::dashboard::url(&app.config)?;
        let opened = crate::llm::oauth::util::open_browser(&url);
        shell.note(
            "Weights",
            if opened {
                format!("Dashboard: {url}")
            } else {
                format!("Dashboard: {url} (open it in a browser)")
            },
        );
    }
    if args != "open" {
        let status = crate::weights::status(&app.config);
        shell.note("Weights", crate::weights::render_status(&status));
    }
    Ok(())
}

/// `/weights map|stats|activations|trace|experiment|generate|direction|edit|evaluate ...`:
/// `dial weights` for this session's model as a background job. It joins this session's
/// foreground compute instead of competing with it.
fn weights_job(shell: &mut Shell, app: &App, args: &str) -> Result<()> {
    let words = split_args(args)?;
    let op = words.first().map(String::as_str).unwrap_or_default();
    if !matches!(
        op,
        "experiment"
            | "evaluate"
            | "combine"
            | "history"
            | "show"
            | "compare"
            | "prune"
            | "cases"
            | "next"
    ) {
        crate::weights::Operation::parse(op).context(
            "usage: /weights [status|open|map|stats|activations|trace|experiment NAME [--budget N]|generate --experiment E --cases FILE|direction NAME --experiment E --positive FILE --negative FILE|edit NAME --experiment E --direction D --layers 10,11|evaluate EDIT --cases FILE|promote EDIT|revert]",
        )?;
    }
    crate::weights::target(&app.config)?;
    // `target` accepted only Dial-owned MLX. Without `weights.model` the main model is that
    // target, so the child is pinned to it: the saved configuration may name a hosted provider
    // and endpoint for other sessions. With `weights.model` the child resolves the same setting.
    let mut argv = Vec::new();
    if app.config.weights.model.is_empty() {
        argv.extend(["--provider", "mlx", "--model", app.config.model.as_str()].map(str::to_owned));
    }
    argv.push("weights".to_owned());
    argv.extend(words);
    spawn_dial(
        shell,
        app,
        "Weights",
        argv,
        Some("Finished. /weights open shows the result in the dashboard."),
    )
}

/// `/weights improve [--with MODEL] GOAL`: hand the user's goal to a model with the weight-editing
/// playbook, so it runs the `model_weights` loop itself. The weights edited are those of the model
/// in force now (or `weights.model`); the model that drives the loop may be another one, chosen
/// with `--with` (`@main` keeps the current one) or from a picker when other chat models are
/// configured. The driver is session-only and is put back when the request ends. Every operation
/// still asks for approval and promotion stays with `/weights promote`.
fn weights_improve(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Step> {
    let rest = args
        .trim()
        .strip_prefix("improve")
        .unwrap_or_default()
        .trim();
    let (driver, goal) = match rest.strip_prefix("--with") {
        Some(rest) => {
            let rest = rest.trim_start();
            let (selector, goal) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            (Some(selector), goal.trim())
        }
        None => (None, rest),
    };
    if goal.is_empty() || driver == Some("") {
        bail!(
            "usage: /weights improve [--with MODEL] GOAL, for example /weights improve stop endless repetition in long lists"
        );
    }
    let config = agent.effective_config();
    let target = crate::weights::target(config)?;
    if !config
        .integrations
        .iter()
        .any(|name| name == "model_weights")
    {
        bail!(
            "the model_weights tool is off. Add \"model_weights\" to `integrations` in /settings (Tools), then run this again"
        );
    }
    let prompt = crate::weights::improvement_prompt(goal);
    let driver = match driver {
        Some(selector) => crate::agent::modes::switch::resolve(config, selector)?,
        None => {
            let choices = driver_choices(config, goal);
            if choices.items.len() > 1 {
                return shell.open(
                    app,
                    agent,
                    crate::app::overlay::Which::Choice(Box::new(choices)),
                );
            }
            return shell.run_turn(app, agent, &prompt);
        }
    };
    let current = config.selection();
    if driver.provider == current.provider && driver.model == current.model {
        return shell.run_turn(app, agent, &prompt);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    let prior = agent.drive_weights_with(&driver, &target)?;
    shell.note(
        "Weights",
        format!(
            "{} drives this request; it edits {}. The session model returns afterwards.",
            driver.display_name(),
            target.display()
        ),
    );
    let step = shell.run_turn(app, agent, &prompt);
    if let Err(error) = agent.restore_model_override(prior) {
        shell.note(
            "Weights",
            format!("could not restore the session model: {error:#}"),
        );
    }
    step
}

/// Roles that never answer chat, so they cannot drive the weight-editing loop.
const NON_CHAT_ROLES: &[&str] = &[
    "embedding",
    "grounding",
    "image",
    "image_embedding",
    "memory",
];

/// The picker for `/weights improve GOAL`: the current model first, then every other configured
/// chat model (roles and favorites). Each row runs `/weights improve --with MODEL GOAL`.
fn driver_choices(config: &crate::config::types::Config, goal: &str) -> ChoiceSpec {
    let current = config.selection();
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(format!("{}/{}", current.provider, current.model));
    let mut items = vec![ChoiceItem {
        value: format!("--with @main {goal}"),
        label: format!("{} (current model)", current.display_name()),
        detail: "The model whose weights are edited also runs the loop.".into(),
        current: true,
    }];
    let roles = config
        .model_roles
        .iter()
        .filter(|(role, _)| !NON_CHAT_ROLES.contains(&role.as_str()))
        .map(|(role, selection)| (format!("role {role}"), selection));
    let favorites = config
        .model_favorites
        .iter()
        .map(|selection| ("favorite".to_owned(), selection));
    for (source, selection) in roles.chain(favorites) {
        let key = format!("{}/{}", selection.provider, selection.model);
        if selection.model.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        items.push(ChoiceItem {
            value: format!("--with {key} {goal}"),
            label: selection.display_name(),
            detail: format!("{key} · {source}: drives the loop for this request only"),
            current: false,
        });
    }
    ChoiceSpec {
        title: "Weights · choose the model that runs the edit loop".into(),
        command: "weights".into(),
        verb: "improve",
        items,
        prompt: "Filter models:".into(),
        summary: "The edited weights stay those of the current model (or weights.model).".into(),
        current_tag: Some("edited".into()),
        ..Default::default()
    }
}

/// `/weights promote NAME` and `/weights revert`: save this project's main model as the edited
/// copy (only after its evaluation passed) or its base, and adopt it in this session.
fn weights_switch(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let words = split_args(args)?;
    let switch = match words.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["promote", name] => crate::weights::promotion(&app.config, name)?,
        ["revert"] => crate::weights::reversion(&app.config)?,
        _ => bail!("usage: /weights promote EDIT or /weights revert"),
    };
    app.save_settings(
        crate::config::settings_store::Scope::Project,
        &switch.changes,
        switch.role,
    )?;
    let fresh = app.reload()?;
    shell.note("Weights", switch.message);
    Ok(shell.config_changed(app, agent, fresh))
}

/// Run `dial ARGS` from the project directory as a background job (see [`spawn_process`]).
fn spawn_dial(
    shell: &mut Shell,
    app: &App,
    label: &'static str,
    argv: Vec<String>,
    after_success: Option<&'static str>,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating the dial executable")?;
    let shown = format!("dial {}", argv.join(" "));
    let message = after_success.map_or_else(|| "Finished.".to_owned(), str::to_owned);
    spawn_process(
        shell,
        label,
        exe.into_os_string(),
        argv.into_iter().map(OsString::from).collect(),
        app.layout.project_root.clone(),
        shown,
        move || Ok(message),
    );
    Ok(())
}

/// Run `program ARGV` in `cwd` as a background job. Output lines stream into the transcript under
/// `label`, after the `$ shown` command line; the job's cancellation kills the child process.
/// After a successful exit, `finish` produces the job's result message.
pub(super) fn spawn_process(
    shell: &mut Shell,
    label: &'static str,
    program: OsString,
    argv: Vec<OsString>,
    cwd: PathBuf,
    shown: String,
    finish: impl FnOnce() -> Result<String> + Send + 'static,
) {
    spawn_job_streaming(shell, label, move |cancel: Arc<AtomicBool>, progress| {
        progress(&format!("$ {shown}"));
        let mut child = Command::new(&program)
            .args(&argv)
            .current_dir(&cwd)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    anyhow::anyhow!(
                        "starting {shown}: {} was not found on PATH",
                        program.to_string_lossy()
                    )
                } else {
                    anyhow::Error::new(error).context(format!("starting {shown}"))
                }
            })?;
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
            finish()
        } else {
            bail!("{shown} exited with {status}")
        }
    });
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
            let mut child = crate::platform::process::spawn(
                Command::new(std::env::current_exe()?)
                    .args(["relay", "--bind", bind])
                    .current_dir(&app.layout.project_root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped()),
            )
            .context("starting the relay")?;
            if let Err(error) = await_relay_ready(&mut child) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
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

/// Readiness comes from the child after bind, not a racy probe of a possibly unrelated listener.
fn await_relay_ready(child: &mut Child) -> Result<()> {
    let stderr = child
        .stderr
        .take()
        .context("relay readiness pipe is missing")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("dial-relay-ready".into())
        .spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut announced = false;
            let mut diagnostic = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !announced && line.starts_with("Dial collaboration relay listening on ") {
                    announced = true;
                    let _ = tx.send(Ok(()));
                } else if !announced && diagnostic.len() < 4096 {
                    diagnostic.push_str(&line);
                    diagnostic.push('\n');
                }
            }
            if !announced {
                let _ = tx.send(Err(diagnostic));
            }
        })
        .context("starting relay readiness reader")?;
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => bail!("relay failed before listening: {}", message.trim()),
        Err(error) => bail!("relay did not confirm readiness: {error}"),
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

#[cfg(test)]
mod tests;
