//! Config-tier slash commands: `/fast`, `/slow`, `/extended-context`, `/modelpreset`, `/usage`
//! and `/ssh`. Each reads or writes `service_tier`, `extended_context`, `model_presets`,
//! `ssh_hosts` or the usage reporter, and persists through the shared settings path.

use crate::agent::{Agent, plan_with_runner, related_tests_with_runner};
use crate::app::{App, Shell, Step};
use crate::config::settings_store::{self, Change, Scope};
use crate::config::types::{Config, ModelPreset, SshHostConfig, VerificationCommand};
use crate::llm::usage_report::{self, Outcome, UsageReport, resolve_used_fraction};
use anyhow::{Result, anyhow, bail};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How often the status-line usage cache refreshes from the providers.
const USAGE_REFRESH_MS: u64 = 5 * 60 * 1000;

/// Provider usage for the status line. The render path only reads the cache; a background thread
/// refreshes it at most once per [`USAGE_REFRESH_MS`], so no network call blocks drawing.
pub struct UsageStatus {
    user_dir: PathBuf,
    reports: Arc<parking_lot::Mutex<Vec<UsageReport>>>,
    next_due_ms: Arc<AtomicU64>,
    in_flight: Arc<AtomicBool>,
}

impl UsageStatus {
    pub fn new(user_dir: PathBuf) -> Self {
        Self {
            user_dir,
            reports: Arc::default(),
            next_due_ms: Arc::default(),
            in_flight: Arc::default(),
        }
    }

    /// Cached one-line summary (`None` hides the segment). Starts a background refresh when one is
    /// due; never waits for it.
    pub fn poll(&self) -> Option<String> {
        let now = now_ms();
        if now >= self.next_due_ms.load(Ordering::SeqCst)
            && !self.in_flight.swap(true, Ordering::SeqCst)
        {
            self.next_due_ms
                .store(now + USAGE_REFRESH_MS, Ordering::SeqCst);
            let dir = self.user_dir.clone();
            let reports = self.reports.clone();
            let in_flight = self.in_flight.clone();
            let spawned = std::thread::Builder::new()
                .name("usage-status".into())
                .spawn(move || {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let fresh: Vec<UsageReport> = usage_report::fetch_all(&dir, &cancel)
                        .into_iter()
                        .filter_map(|outcome: Outcome| outcome.result.ok())
                        .collect();
                    *reports.lock() = fresh;
                    in_flight.store(false, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.in_flight.store(false, Ordering::SeqCst);
            }
        }
        usage_report::status_summary(&self.reports.lock(), now)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "fast",
        description: "priority service tier: on, ultra (OpenAI ultrafast), off, status",
        args_hint: "[on|ultra|off|status]",
        subcommands: &["on", "ultra", "off", "status"],
    },
    super::CommandSpec {
        name: "slow",
        description: "flex service tier for lower cost: on, off, status",
        args_hint: "[on|off|status]",
        subcommands: &["on", "off", "status"],
    },
    super::CommandSpec {
        name: "extended-context",
        description: "request the extended context window: on, off, status",
        args_hint: "[on|off|status]",
        subcommands: &["on", "off", "status"],
    },
    super::CommandSpec {
        name: "modelpreset",
        description: "manage model presets: list, save, switch, delete",
        args_hint: "[list|save <name>|switch <name>|delete <name>]",
        subcommands: &["list", "save", "switch", "delete"],
    },
    super::CommandSpec {
        name: "usage",
        description: "show provider usage limits for signed-in providers",
        args_hint: "[show|reset [anthropic|openai-codex] [confirm]]",
        subcommands: &["show", "reset"],
    },
    super::CommandSpec {
        name: "ssh",
        description: "manage SSH hosts: list, add, remove, help",
        args_hint: "[list|add|remove|help]",
        subcommands: &["list", "add", "remove", "help"],
    },
    super::CommandSpec {
        name: "checks",
        description: "list the checks Dial runs after edits; add or remove project checks",
        args_hint: "[list|add <name> [--ext rs,py] [--timeout secs] -- <command>|remove <name>|help]",
        subcommands: &["list", "add", "remove", "help"],
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
        "fast" => fast(shell, app, agent, args),
        "slow" => slow(shell, app, agent, args),
        "extended-context" => extended_context(shell, app, agent, args),
        "modelpreset" => modelpreset(shell, app, agent, args),
        "usage" => usage(shell, app, args),
        "ssh" => ssh(shell, app, agent, args),
        "checks" => checks(shell, app, agent, args),
        _ => return None,
    };
    Some(result)
}

/// Provider families whose `priority` tier Dial sends (`/fast`).
const PRIORITY_PROVIDERS: &[&str] = &[
    "openai",
    "openai-codex",
    "chatgpt",
    "anthropic",
    "google",
    "gemini",
    "google-vertex",
];
/// Provider families with OpenAI's `ultrafast` tier.
const ULTRAFAST_PROVIDERS: &[&str] = &["openai", "openai-codex"];
/// Provider families with the `flex` tier (`/slow`). Anthropic's low-priority lane is not supported.
const FLEX_PROVIDERS: &[&str] = &["openai", "openai-codex", "chatgpt", "google", "gemini"];

/// A `/fast` or `/slow` outcome: the note to show and the settings writes it needs.
struct TierPlan {
    note: String,
    changes: Vec<Change>,
}

fn set_value(path: &str, value: toml::Value) -> Change {
    Change::Set {
        path: path.into(),
        value,
    }
}

fn tier_plan(note: impl Into<String>, changes: Vec<Change>) -> TierPlan {
    TierPlan {
        note: note.into(),
        changes,
    }
}

/// `/fast` owns `fast_mode` and the explicit `service_tier` fast values; it never touches `slow_mode`.
fn fast_plan(config: &Config, provider: &str, args: &str) -> Result<TierPlan> {
    let tier = config.service_tier.as_str();
    let fast_tier = matches!(tier, "priority" | "ultrafast");
    let fast_on = config.fast_mode || fast_tier;
    let status = match tier {
        "ultrafast" => "ultra",
        _ if fast_on => "on",
        _ => "off",
    };
    let target = match args.trim() {
        "status" => return Ok(tier_plan(format!("Fast mode is {status}."), vec![])),
        "" | "toggle" if fast_on => "off",
        "" | "toggle" | "on" => "on",
        "ultra" | "ultrafast" => "ultra",
        "off" => "off",
        other => bail!("Usage: /fast [on|ultra|off|status] (got {other:?})"),
    };
    match target {
        "off" => {
            let mut changes = Vec::new();
            if config.fast_mode {
                changes.push(set_value("fast_mode", toml::Value::Boolean(false)));
            }
            if fast_tier {
                changes.push(set_value(
                    "service_tier",
                    toml::Value::String("none".into()),
                ));
            }
            Ok(tier_plan("Fast mode disabled.", changes))
        }
        "on" => {
            if !PRIORITY_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    "Fast mode is unavailable for the current model.",
                    vec![],
                ));
            }
            let mut changes = vec![set_value("fast_mode", toml::Value::Boolean(true))];
            if tier != "none" {
                changes.push(set_value(
                    "service_tier",
                    toml::Value::String("none".into()),
                ));
            }
            Ok(tier_plan("Fast mode enabled.", changes))
        }
        _ => {
            if !ULTRAFAST_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    "Ultrafast is unavailable for the current model.",
                    vec![],
                ));
            }
            Ok(tier_plan(
                "Ultrafast mode enabled.",
                vec![set_value(
                    "service_tier",
                    toml::Value::String("ultrafast".into()),
                )],
            ))
        }
    }
}

/// `/slow` owns `slow_mode` only; it never touches `fast_mode` or `service_tier`.
fn slow_plan(config: &Config, provider: &str, args: &str) -> Result<TierPlan> {
    let slow_on = config.slow_mode;
    let arg = args.trim();
    match arg {
        "status" => Ok(tier_plan(
            if slow_on {
                "Slow mode is on (flex tier)."
            } else {
                "Slow mode is off."
            },
            vec![],
        )),
        "" | "toggle" | "on" | "off" => {
            let enable = match arg {
                "on" => true,
                "off" => false,
                _ => !slow_on,
            };
            if !enable {
                let changes = if slow_on {
                    vec![set_value("slow_mode", toml::Value::Boolean(false))]
                } else {
                    vec![]
                };
                return Ok(tier_plan("Slow mode off.", changes));
            }
            if !FLEX_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    format!(
                        "Slow mode is unavailable for {provider}: Dial implements /slow as the flex tier \
                         on OpenAI and Google models; Anthropic's subscription low-priority lane is not ported."
                    ),
                    vec![],
                ));
            }
            Ok(tier_plan(
                "Slow mode on: requests use the flex tier (lower cost, higher latency).",
                vec![set_value("slow_mode", toml::Value::Boolean(true))],
            ))
        }
        other => bail!("Usage: /slow [on|off|status] (got {other:?})"),
    }
}

fn apply_tier_plan(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    title: &str,
    plan: TierPlan,
) -> Result<Step> {
    if plan.changes.is_empty() {
        shell.note(title, plan.note);
        return Ok(Step::Done);
    }
    let step = shell.persist(app, agent, &plan.changes, Scope::Global)?;
    shell.note(title, plan.note);
    Ok(step)
}

fn fast(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let provider = app.config.selection().provider;
    let plan = fast_plan(&app.config, &provider, args)?;
    apply_tier_plan(shell, app, agent, "Fast", plan)
}

fn slow(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let provider = app.config.selection().provider;
    let plan = slow_plan(&app.config, &provider, args)?;
    apply_tier_plan(shell, app, agent, "Slow", plan)
}

fn extended_context(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Step> {
    const TITLE: &str = "Extended context";
    let current = app.config.extended_context;
    let (enabled, reply) = match args.trim() {
        "status" => {
            shell.note(
                TITLE,
                format!(
                    "Extended context is {}.",
                    if current { "on" } else { "off" }
                ),
            );
            return Ok(Step::Done);
        }
        "" | "toggle" => {
            let enabled = !current;
            (
                enabled,
                format!(
                    "Extended context {}.",
                    if enabled { "enabled" } else { "disabled" }
                ),
            )
        }
        "on" => (true, "Extended context enabled.".to_owned()),
        "off" => (false, "Extended context disabled.".to_owned()),
        other => bail!("Usage: /extended-context [on|off|status] (got {other:?})"),
    };
    let step = shell.persist(
        app,
        agent,
        &[Change::Set {
            path: "extended_context".into(),
            value: toml::Value::Boolean(enabled),
        }],
        Scope::Global,
    )?;
    shell.note(TITLE, reply);
    Ok(step)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn modelpreset(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    const TITLE: &str = "Model preset";
    let trimmed = args.trim();
    let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (trimmed, ""),
    };
    match sub {
        "" | "list" => {
            let presets = &app.config.model_presets;
            let text = if presets.is_empty() {
                "No model presets saved. Use /modelpreset save <name> to create one.".to_owned()
            } else {
                format!(
                    "Model presets: {}",
                    presets.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            shell.note(TITLE, text);
            Ok(Step::Done)
        }
        "save" => {
            if !valid_name(rest) {
                bail!(
                    "Usage: /modelpreset save <name>; names start with a letter, then letters, digits, '-' or '_'"
                );
            }
            let preset = ModelPreset {
                roles: app.config.model_roles.clone(),
                thinking: app
                    .config
                    .active_model_profile()
                    .and_then(|profile| profile.reasoning_effort),
            };
            let change = Change::Set {
                path: format!("model_presets.{rest}"),
                value: toml::Value::try_from(&preset)?,
            };
            let step = shell.persist(app, agent, &[change], Scope::Global)?;
            shell.note(TITLE, format!("Saved model preset \"{rest}\"."));
            Ok(step)
        }
        "switch" => {
            let Some(preset) = app.config.model_presets.get(rest) else {
                shell.note(TITLE, format!("Preset not found: {rest}"));
                return Ok(Step::Done);
            };
            let mut changes = vec![Change::Set {
                path: "model_roles".into(),
                value: toml::Value::try_from(&preset.roles)?,
            }];
            if let Some(effort) = preset.thinking
                && let Some(active) = app.config.active_model_profile()
            {
                let profile_name = app
                    .config
                    .model_profiles
                    .iter()
                    .find(|(_, profile)| std::ptr::eq(*profile, active))
                    .map(|(name, _)| name.clone())
                    .ok_or_else(|| anyhow!("active model profile is not in model_profiles"))?;
                changes.push(Change::Set {
                    path: format!("model_profiles.{profile_name}.reasoning_effort"),
                    value: toml::Value::try_from(effort)?,
                });
            }
            let step = shell.persist(app, agent, &changes, Scope::Global)?;
            shell.note(TITLE, format!("Switched to model preset \"{rest}\"."));
            Ok(step)
        }
        "delete" | "remove" => {
            let project = settings_store::load_layer(&app.layout, Scope::Project)?;
            if project
                .get("model_presets")
                .and_then(|value| value.as_table())
                .is_some_and(|table| table.contains_key(rest))
            {
                bail!("Preset {rest} is defined in the project config; delete it there.");
            }
            if !app.config.model_presets.contains_key(rest) {
                shell.note(TITLE, format!("Preset not found: {rest}"));
                return Ok(Step::Done);
            }
            let step = shell.persist(
                app,
                agent,
                &[Change::Remove {
                    path: format!("model_presets.{rest}"),
                }],
                Scope::Global,
            )?;
            shell.note(TITLE, format!("Deleted model preset \"{rest}\"."));
            Ok(step)
        }
        other => bail!(
            "Unknown /modelpreset subcommand: {other}. Usage: /modelpreset [list|save <name>|switch <name>|delete <name>]"
        ),
    }
}

fn usage(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let args = args.trim();
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let user_dir = app.config.user_dir.clone();
    match sub {
        "" | "show" if rest.trim().is_empty() => {
            super::extensions::spawn_job(shell, "Usage", move |cancel: Arc<AtomicBool>| {
                Ok(usage_text(&usage_report::fetch_all(&user_dir, &cancel)))
            });
            Ok(Step::Done)
        }
        "reset" => {
            let rest = rest.trim().to_owned();
            super::extensions::spawn_job(shell, "Usage reset", move |cancel: Arc<AtomicBool>| {
                usage_report::resets::run(&user_dir, &rest, &cancel)
            });
            Ok(Step::Done)
        }
        _ => bail!("Usage: /usage [show|reset [anthropic|openai-codex] [confirm]] (got {args:?})"),
    }
}

/// Renders every signed-in provider's limits; failures stay per provider.
fn usage_text(outcomes: &[Outcome]) -> String {
    if outcomes.is_empty() {
        return "No usage data: no supported provider is signed in.".to_owned();
    }
    outcomes
        .iter()
        .map(|outcome| match &outcome.result {
            Ok(report) => report_text(report),
            Err(error) => format!("{}: error: {error}", outcome.provider),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn report_text(report: &UsageReport) -> String {
    let mut lines = vec![report.provider.clone()];
    for limit in &report.limits {
        let window = limit
            .window
            .as_ref()
            .map_or(limit.label.as_str(), |window| window.label.as_str());
        let used = resolve_used_fraction(limit)
            .map(|used| format!("{:.0}% used", used * 100.0))
            .unwrap_or_else(|| "usage unknown".to_owned());
        lines.push(format!("  {window}: {used}"));
    }
    lines.extend(report.notes.iter().map(|note| format!("  note: {note}")));
    lines.join("\n")
}

const SSH_USAGE: &str = "Usage: /ssh [list|add <name> --host <host> [--user <user>] [--port <port>] [--key <path>] [--scope project|user]|remove <name> [--scope project|user]|help]";

fn ssh(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    const TITLE: &str = "SSH";
    let trimmed = args.trim();
    let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (trimmed, ""),
    };
    match sub {
        "" | "list" => {
            let text = format!(
                "{}\n\nAdd a host with /ssh add <name> --host <host>.",
                ssh_list(&app.config)
            );
            shell.note(TITLE, text);
            Ok(Step::Done)
        }
        "help" => {
            shell.note(TITLE, SSH_USAGE);
            Ok(Step::Done)
        }
        "add" => {
            let (name, host, scope) = parse_ssh_add(rest)?;
            if app.config.ssh_hosts.contains_key(&name) {
                bail!("SSH host {name} already exists; remove it first.");
            }
            let change = Change::Set {
                path: format!("ssh_hosts.{name}"),
                value: toml::Value::try_from(&host)?,
            };
            let step = shell.persist(app, agent, &[change], scope)?;
            shell.note(TITLE, format!("Added SSH host {name}."));
            Ok(step)
        }
        "remove" => {
            let (name, scope) = parse_ssh_remove(rest)?;
            if !app.config.ssh_hosts.contains_key(&name) {
                bail!("SSH host not found: {name}");
            }
            let step = shell.persist(
                app,
                agent,
                &[Change::Remove {
                    path: format!("ssh_hosts.{name}"),
                }],
                scope,
            )?;
            shell.note(TITLE, format!("Removed SSH host {name}."));
            Ok(step)
        }
        other => bail!("Unknown /ssh subcommand: {other}. {SSH_USAGE}"),
    }
}

fn ssh_list(config: &Config) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return configured_only_list(config);
    };
    let hosts = crate::tools::integrations::ssh::resolve_hosts(&home, &config.ssh_hosts);
    let lines: Vec<String> = hosts
        .iter()
        .map(|host| {
            let user = host
                .user
                .as_deref()
                .map(|user| format!("{user}@"))
                .unwrap_or_default();
            let target = host.hostname.as_deref().unwrap_or(&host.alias);
            let port = host.port.map(|port| format!(":{port}")).unwrap_or_default();
            let origin = if host.source.as_os_str().is_empty() {
                "dial"
            } else {
                "~/.ssh/config"
            };
            format!("{}  {user}{target}{port}  ({origin})", host.alias)
        })
        .collect();
    if lines.is_empty() {
        "No SSH hosts found in ~/.ssh/config or Dial's host store.".to_owned()
    } else {
        lines.join("\n")
    }
}

const CHECKS_USAGE: &str = "Usage: /checks [list|add <name> [--ext rs,py] [--timeout secs] -- <command>|remove <name>|help]";

fn checks(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    const TITLE: &str = "Checks";
    let trimmed = args.trim();
    let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (trimmed, ""),
    };
    match sub {
        "" | "list" => {
            shell.note(TITLE, checks_list(&app.config));
            Ok(Step::Done)
        }
        "help" => {
            shell.note(TITLE, CHECKS_USAGE);
            Ok(Step::Done)
        }
        "add" => {
            let (name, check) = parse_check_add(rest)?;
            if app
                .config
                .verification
                .commands
                .iter()
                .any(|c| c.name == name)
            {
                bail!("check {name} already exists; remove it first.");
            }
            let mut commands = project_checks(app)?;
            commands.push(toml::Value::try_from(&check)?);
            let step = shell.persist(
                app,
                agent,
                &[Change::Set {
                    path: "verification.commands".into(),
                    value: toml::Value::Array(commands),
                }],
                Scope::Project,
            )?;
            shell.note(TITLE, format!("Added check {name}."));
            Ok(step)
        }
        "remove" => {
            let name = rest;
            if name.is_empty() {
                bail!("Usage: /checks remove <name>");
            }
            let mut commands = project_checks(app)?;
            let before = commands.len();
            commands.retain(|value| value.get("name").and_then(toml::Value::as_str) != Some(name));
            if commands.len() == before {
                bail!(
                    "no check named {name} in the project config; checks from the user config must be removed from that file"
                );
            }
            let step = shell.persist(
                app,
                agent,
                &[Change::Set {
                    path: "verification.commands".into(),
                    value: toml::Value::Array(commands),
                }],
                Scope::Project,
            )?;
            shell.note(TITLE, format!("Removed check {name}."));
            Ok(step)
        }
        other => bail!("Unknown /checks subcommand: {other}. {CHECKS_USAGE}"),
    }
}

/// Existing project-level `verification.commands` entries, read raw so user-level entries are not copied down.
fn project_checks(app: &App) -> Result<Vec<toml::Value>> {
    let project = settings_store::load_layer(&app.layout, Scope::Project)?;
    Ok(project
        .get("verification")
        .and_then(|value| value.get("commands"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// `<name> [--ext rs,py] [--timeout secs] -- <command>`; the command runs through the shell.
fn parse_check_add(rest: &str) -> Result<(String, VerificationCommand)> {
    const USAGE: &str = "Usage: /checks add <name> [--ext rs,py] [--timeout secs] -- <command>";
    let Some(split) = rest.find(" -- ") else {
        bail!("{USAGE}");
    };
    let (head, command) = (&rest[..split], rest[split + 4..].trim());
    if command.is_empty() {
        bail!("{USAGE}");
    }
    let mut tokens = head.split_whitespace();
    let name = tokens.next().filter(|name| !name.starts_with("--"));
    let Some(name) = name else {
        bail!("{USAGE}");
    };
    let mut file_extensions = Vec::new();
    let mut timeout_secs = 0;
    while let Some(flag) = tokens.next() {
        let value = tokens.next();
        match (flag, value) {
            ("--ext", Some(value)) => {
                file_extensions = value
                    .split(',')
                    .map(|ext| ext.trim().trim_start_matches('.').to_owned())
                    .filter(|ext| !ext.is_empty())
                    .collect();
            }
            ("--timeout", Some(value)) => {
                timeout_secs = value
                    .parse()
                    .map_err(|_| anyhow!("--timeout must be a whole number of seconds"))?;
            }
            _ => bail!("{USAGE}"),
        }
    }
    Ok((
        name.to_owned(),
        VerificationCommand {
            name: name.to_owned(),
            command: command.to_owned(),
            file_extensions,
            timeout_secs,
        },
    ))
}

/// Automatic checks for the uncommitted changes (the same planner that runs after each edit) plus
/// configured commands.
fn checks_list(config: &Config) -> String {
    let root = &config.project_root;
    let changed = changed_files(root);
    let paths: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
    let python = if config.python.is_file() {
        config.python.clone()
    } else {
        PathBuf::from("python3")
    };
    let mut automatic =
        plan_with_runner(root, &python, &paths, &config.verification.python_command);
    if config.assistance.enabled && config.assistance.selective_verification {
        automatic.extend(related_tests_with_runner(
            root,
            &python,
            &paths,
            &config.verification.python_command,
        ));
    }
    let mut lines = Vec::new();
    if !config.verification.enabled {
        lines.push("Automatic checks: off (verification is disabled in the config).".to_owned());
    }
    lines.push("Automatic checks for the current changes:".to_owned());
    if changed.is_empty() {
        lines.push("  none; there are no uncommitted changes yet.".to_owned());
    } else if automatic.is_empty() {
        lines.push("  none for these changes.".to_owned());
    } else {
        lines.extend(
            automatic
                .iter()
                .map(|check| format!("  {}: {}", check.name, check.command)),
        );
    }
    lines.push("Configured checks:".to_owned());
    if config.verification.commands.is_empty() {
        lines
            .push("  none. Add one with /checks add <name> [--ext rs,py] -- <command>.".to_owned());
    }
    for check in &config.verification.commands {
        let scope = if check.file_extensions.is_empty() {
            "any source edit".to_owned()
        } else {
            check.file_extensions.join(", ")
        };
        let timeout = if check.timeout_secs == 0 {
            "default".to_owned()
        } else {
            format!("{}s", check.timeout_secs)
        };
        lines.push(format!(
            "  {}: {} (files: {scope}; timeout: {timeout})",
            check.name, check.command
        ));
    }
    lines.join("\n")
}

/// Paths with uncommitted changes in the project, as `git status` reports them. Empty outside Git.
fn changed_files(root: &std::path::Path) -> Vec<String> {
    let Ok(stdout) = crate::tools::vcs::git::run_checked(
        root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        false,
    ) else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|path| path.rsplit(" -> ").next().unwrap_or(path).trim().to_owned())
        .filter(|path| !path.is_empty())
        .collect()
}

/// Without `HOME` there is no `~/.ssh/config` to merge; list only Dial's configured hosts.
fn configured_only_list(config: &Config) -> String {
    if config.ssh_hosts.is_empty() {
        return "No SSH hosts configured in Dial's host store.".to_owned();
    }
    config
        .ssh_hosts
        .iter()
        .map(|(name, host)| format!("{name}  {}  (dial)", describe_host(host)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_host(host: &SshHostConfig) -> String {
    let user = host
        .user
        .as_deref()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    let port = host.port.map(|port| format!(":{port}")).unwrap_or_default();
    format!("{user}{}{port}", host.host)
}

fn parse_scope(value: &str) -> Result<Scope> {
    match value {
        "project" => Ok(Scope::Project),
        "user" => Ok(Scope::Global),
        other => bail!("--scope must be project or user (got {other:?})"),
    }
}

fn parse_ssh_add(rest: &str) -> Result<(String, SshHostConfig, Scope)> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let name = tokens
        .first()
        .filter(|name| !name.starts_with("--"))
        .ok_or_else(|| anyhow!("Usage: /ssh add <name> --host <host> [...]"))?;
    if !valid_name(name) {
        bail!("SSH host names start with a letter, then letters, digits, '-' or '_'");
    }
    let mut host = SshHostConfig::default();
    let mut scope = Scope::Global;
    let mut index = 1;
    while index < tokens.len() {
        let flag = tokens[index];
        let value = tokens
            .get(index + 1)
            .copied()
            .ok_or_else(|| anyhow!("{flag} needs a value"))?;
        match flag {
            "--host" => host.host = value.to_owned(),
            "--user" => host.user = Some(value.to_owned()),
            "--port" => {
                let port: u16 = value
                    .parse()
                    .map_err(|_| anyhow!("--port must be 1..65535 (got {value:?})"))?;
                if port == 0 {
                    bail!("--port must be 1..65535 (got 0)");
                }
                host.port = Some(port);
            }
            "--key" => host.key = Some(value.to_owned()),
            "--scope" => scope = parse_scope(value)?,
            other => bail!("Unknown /ssh add option: {other}"),
        }
        index += 2;
    }
    if host.host.is_empty() {
        bail!("/ssh add needs --host <host>");
    }
    Ok((name.to_string(), host, scope))
}

fn parse_ssh_remove(rest: &str) -> Result<(String, Scope)> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let name = tokens
        .first()
        .ok_or_else(|| anyhow!("Usage: /ssh remove <name> [--scope project|user]"))?;
    let mut scope = Scope::Global;
    let mut index = 1;
    while index < tokens.len() {
        match tokens[index] {
            "--scope" => {
                let value = tokens
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("--scope needs a value"))?;
                scope = parse_scope(value)?;
                index += 2;
            }
            other => bail!("Unknown /ssh remove option: {other}"),
        }
    }
    Ok(((*name).to_owned(), scope))
}

#[cfg(test)]
mod tests;
