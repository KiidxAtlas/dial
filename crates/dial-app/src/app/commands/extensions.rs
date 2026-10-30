//! Extension slash commands: `/mcp`, `/tools`, `/extensions`, `/agents`.

use crate::agent::Agent;
use crate::agent::base::agent_defs::Catalog;
use crate::app::{App, Shell, Step, Which};
use crate::config::settings_store::{Change, Scope, load_layer};
use crate::config::tools::mcp::validate_server;
use crate::config::types::{Config, McpServerConfig, McpTransport};
use crate::llm::oauth::BannerInteraction;
use crate::tools::integrations::mcp::connect_extra;
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) const COMMANDS: &[super::CommandSpec] = &[
    super::CommandSpec {
        name: "mcp",
        description: "manage MCP servers: list, add, remove, enable, disable, test, login, reload",
        args_hint: "[list|add|remove|enable|disable|test|login|reload|help]",
        subcommands: &[
            "list",
            "add",
            "remove",
            "enable",
            "disable",
            "test",
            "login",
            "reload",
            "reconnect",
            "logout",
            "resources",
            "prompts",
            "prompt",
            "notifications",
            "smithery-search",
            "smithery-install",
            "smithery-login",
            "smithery-logout",
            "help",
        ],
    },
    super::CommandSpec {
        name: "tools",
        description: "list the tools offered to the agent and those available",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "extensions",
        description: "browse and install MCP servers and skills (/status shows the configuration)",
        args_hint: "",
        subcommands: &[],
    },
];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
) -> Option<Result<Step>> {
    let title = match name {
        "tools" => "Tools",
        "extensions" => "Extensions",
        _ => return None,
    };
    let result = match name {
        "tools" => {
            shell.note(title, tools_report(agent));
            Ok(Step::Done)
        }
        _ => shell.open(app, agent, Which::Extensions),
    };
    Some(noted(shell, title, result))
}

/// `/mcp` forms that change settings or ask for a rebuild.
/// `agent` is `None` during a turn: writes land at once and the rebuild waits for the turn's end.
pub(super) fn run_settings(
    shell: &mut Shell,
    app: &App,
    agent: Option<&mut Agent<'_>>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let (title, result) = match name {
        "mcp" if !split_subcommand(args).0.starts_with("smithery-") => {
            ("MCP", mcp(shell, app, agent, args))
        }
        _ => return None,
    };
    Some(noted(shell, title, result))
}

/// Reports, listings, connection tests, sign-in and marketplace file operations. None of them
/// changes the running agent, so they run at once even during a turn.
pub(super) fn run_live(
    shell: &mut Shell,
    app: &App,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let (title, result) = match name {
        "status" => {
            shell.note("Extensions", extensions_report(&app.config));
            ("Extensions", Ok(Step::Done))
        }
        "agents" => {
            shell.note("Agents", agents_report(&app.config));
            ("Agents", Ok(Step::Done))
        }
        "mcp" => ("MCP", mcp_live(shell, app, args)?),
        _ => return None,
    };
    Some(noted(shell, title, result))
}

/// Shows a command error in the transcript under `title` instead of failing the dispatch.
fn noted(shell: &mut Shell, title: &str, result: Result<Step>) -> Result<Step> {
    result.or_else(|error| {
        shell.note(title, format!("{error:#}"));
        Ok(Step::Done)
    })
}

pub(super) fn completions(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for name in config.mcp.keys() {
        for sub in [
            "login",
            "test",
            "remove",
            "enable",
            "disable",
            "reconnect",
            "logout",
        ] {
            out.push(format!("/mcp {sub} {name}"));
        }
    }
    for sub in [
        "list",
        "help",
        "add",
        "reload",
        "resources",
        "prompts",
        "prompt",
        "notifications",
    ] {
        out.push(format!("/mcp {sub}"));
    }
    out
}

// ---- /mcp ----------------------------------------------------------------------------------

fn mcp(shell: &mut Shell, app: &App, agent: Option<&mut Agent<'_>>, args: &str) -> Result<Step> {
    let (sub, rest) = split_subcommand(args);
    let config = shell.settings_config(app).clone();
    match sub {
        "add" => {
            let (name, server, scope) = parse_mcp_add(rest)?;
            if config.mcp.contains_key(&name) {
                bail!("MCP server {name} already exists; remove it first.");
            }
            let change = Change::Insert {
                path: format!("mcp.{name}"),
                value: toml::Value::try_from(&server)?,
            };
            let step = shell.save(app, agent, &[change], scope)?;
            shell.note(
                "MCP",
                format!("Added MCP server {name}. It takes effect at the next rebuild; the check below uses a temporary connection."),
            );
            test_server(shell, app, name, server);
            Ok(step)
        }
        "remove" => {
            let name = existing_server(&config, rest, "remove")?;
            let step = shell.save(
                app,
                agent,
                &[Change::Remove {
                    path: format!("mcp.{name}"),
                }],
                mcp_scope(&app.layout, name)?,
            )?;
            shell.note("MCP", format!("Removed MCP server {name}."));
            Ok(step)
        }
        "enable" | "disable" => {
            let name = existing_server(&config, rest, sub)?;
            let enabled = sub == "enable";
            let step = shell.save(
                app,
                agent,
                &[Change::Set {
                    path: format!("mcp.{name}.enabled"),
                    value: toml::Value::Boolean(enabled),
                }],
                mcp_scope(&app.layout, name)?,
            )?;
            shell.note(
                "MCP",
                format!(
                    "{} MCP server {name}. The change takes effect at the next rebuild.",
                    if enabled { "Enabled" } else { "Disabled" }
                ),
            );
            Ok(step)
        }
        "reload" | "reconnect" => Ok(reload_runtime(shell, app, agent)),
        other => bail!("Unknown /mcp subcommand: {other}. Run /mcp help."),
    }
}

/// The `/mcp` subcommands that never touch the running agent: listings, help, temporary test
/// connections, sign-in and sign-out. `None` leaves the subcommand to [`mcp`].
fn mcp_live(shell: &mut Shell, app: &App, args: &str) -> Option<Result<Step>> {
    let (sub, rest) = split_subcommand(args);
    let config = shell.settings_config(app).clone();
    let result = match sub {
        "" | "list" => {
            shell.note("MCP", mcp_list(&config));
            Ok(Step::Done)
        }
        "help" => {
            shell.note("MCP", MCP_HELP);
            Ok(Step::Done)
        }
        "test" => existing_server(&config, rest, "test").map(|name| {
            test_server(shell, app, name.to_owned(), config.mcp[name].clone());
            Step::Done
        }),
        "login" => existing_server(&config, rest, "login").map(|name| {
            let server = config.mcp[name].clone();
            let user_dir = config.user_dir.clone();
            let owned = name.to_owned();
            spawn_job(shell, "MCP login", move |cancel| {
                let ui = BannerInteraction { id: "mcp" };
                crate::tools::integrations::mcp::login(&user_dir, &owned, &server, &ui, cancel)
            });
            Step::Done
        }),
        "logout" | "unauth" => existing_server(&config, rest, sub).and_then(|name| {
            let removed = crate::tools::integrations::mcp::logout(&app.config.user_dir, name)?;
            shell.note(
                "MCP",
                if removed {
                    format!("Signed out of MCP server {name}. A live connection keeps its token until it reconnects.")
                } else {
                    format!("No stored sign-in for MCP server {name}.")
                },
            );
            Ok(Step::Done)
        }),
        "prompt" => mcp_prompt(shell, &config, rest),
        "resources" | "prompts" | "notifications" => {
            let specs: BTreeMap<String, McpServerConfig> = app
                .config
                .mcp
                .iter()
                .filter(|(_, server)| server.enabled)
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect();
            let config = app.config.clone();
            let kind = sub.to_owned();
            spawn_job(shell, "MCP", move |_| {
                let load = connect_extra(&specs, &config);
                Ok(mcp_listing(&kind, &load))
            });
            Ok(Step::Done)
        }
        _ => return None,
    };
    Some(result)
}

/// Connects to `server` on a temporary connection in the background and lists its tools.
fn test_server(shell: &mut Shell, app: &App, name: String, mut server: McpServerConfig) {
    let config = app.config.clone();
    server.enabled = true;
    spawn_job(shell, "MCP test", move |_| {
        let specs = BTreeMap::from([(name.clone(), server)]);
        let load = connect_extra(&specs, &config);
        let offered = load.live.offered_tools();
        let mut lines = vec![format!("{name}: {} tool(s)", offered.len())];
        lines.extend(offered.iter().map(|tool| format!("  {}", tool.name())));
        lines.extend(load.notices);
        Ok(lines.join("\n"))
    });
}

/// Label of the `/mcp prompt` background job. Its result is put in the composer by
/// `Shell::report_job_result`.
pub(crate) const MCP_PROMPT_LABEL: &str = "MCP prompt";

/// `/mcp prompt <server> <name> [key=value…]`: fetches a prompt from a configured server on a
/// background job, so the UI stays responsive. The messages land in the composer for review when
/// the job finishes (see [`composed_draft`]); a server that fails to start fails the job instead.
fn mcp_prompt(shell: &mut Shell, config: &Config, rest: &str) -> Result<Step> {
    let request = parse_prompt_request(rest)?;
    let Some(server_config) = config.mcp.get(&request.server) else {
        bail!("No MCP server named {}.", request.server);
    };
    let specs = BTreeMap::from([(
        request.server.clone(),
        McpServerConfig {
            enabled: true,
            ..server_config.clone()
        },
    )]);
    let config = config.clone();
    spawn_job(shell, MCP_PROMPT_LABEL, move |cancel| {
        let load = connect_extra(&specs, &config);
        let Some(session) = load
            .sessions
            .iter()
            .find(|session| session.name() == request.server.as_str())
        else {
            bail!(
                "MCP server {} did not connect: {}",
                request.server,
                load.notices.join("; ")
            );
        };
        let result = session.get_prompt(
            &request.name,
            serde_json::Value::Object(request.arguments),
            &|| cancel.load(Ordering::Relaxed),
        )?;
        prompt_text(&result)
    });
    Ok(Step::Done)
}

/// A parsed `/mcp prompt` request.
struct PromptRequest {
    server: String,
    name: String,
    arguments: serde_json::Map<String, serde_json::Value>,
}

/// Parses `<server> <name> [key=value…]`. Words are split the way a shell splits them, so a value
/// with spaces is quoted: `key="a b"` passes `a b`.
fn parse_prompt_request(rest: &str) -> Result<PromptRequest> {
    const USAGE: &str = "Usage: /mcp prompt <server> <name> [key=value…]";
    let words = shlex::split(rest)
        .ok_or_else(|| anyhow!("Unclosed quote in /mcp prompt arguments. {USAGE}"))?;
    let mut words = words.into_iter();
    let (Some(server), Some(name)) = (words.next(), words.next()) else {
        bail!(USAGE);
    };
    let mut arguments = serde_json::Map::new();
    for word in words {
        let Some((key, value)) = word.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            bail!("Prompt argument `{word}` must be key=value. {USAGE}");
        };
        if arguments
            .insert(key.to_owned(), serde_json::Value::String(value.to_owned()))
            .is_some()
        {
            bail!("Prompt argument `{key}` is given more than once. {USAGE}");
        }
    }
    Ok(PromptRequest {
        server,
        name,
        arguments,
    })
}

/// The composer text once a prompt lands: appended below any draft already typed.
pub(crate) fn composed_draft(draft: &str, text: &str) -> String {
    if draft.trim().is_empty() {
        text.to_owned()
    } else {
        format!("{draft}\n\n{text}")
    }
}

/// Plain text for a `prompts/get` result: each message as `role: text`. Non-text content is
/// named rather than dumped, and the whole text is sanitized before it reaches the composer.
fn prompt_text(result: &serde_json::Value) -> Result<String> {
    let lines: Vec<String> = result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|message| {
            let role = message
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("message");
            let content = message
                .get("content")
                .map_or_else(String::new, prompt_block_text);
            format!("{role}: {content}")
        })
        .collect();
    if lines.is_empty() {
        bail!("The prompt returned no messages.");
    }
    Ok(crate::ui::text::sanitize(&lines.join("\n")))
}

fn prompt_block_text(block: &serde_json::Value) -> String {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("resource") => block
            .pointer("/resource/text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("[resource without text omitted]")
            .to_owned(),
        Some(kind) => format!("[{kind} content omitted]"),
        None => "[unknown content omitted]".to_owned(),
    }
}

/// `/mcp resources|prompts|notifications`: one section per connected server, from its live
/// session. A failing server reports its own error and does not hide the others.
fn mcp_listing(kind: &str, load: &crate::tools::integrations::mcp::McpLoad) -> String {
    if load.sessions.is_empty() {
        return "No MCP servers are connected.".to_owned();
    }
    let mut lines = Vec::new();
    for session in &load.sessions {
        lines.push(format!("{}:", session.name()));
        match kind {
            "resources" => match session.resources() {
                Ok(listing) => {
                    let resources = json_items(&listing, "resources");
                    let templates = json_items(&listing, "templates");
                    if resources.is_empty() && templates.is_empty() {
                        lines.push("  (none)".to_owned());
                    }
                    for item in resources {
                        lines.push(format!(
                            "  {}  {}",
                            json_text(&item, "uri"),
                            json_text(&item, "name")
                        ));
                    }
                    for item in templates {
                        lines.push(format!("  {}  (template)", json_text(&item, "uriTemplate")));
                    }
                }
                Err(error) => lines.push(format!("  error: {error:#}")),
            },
            "prompts" => match session.prompts() {
                Ok(listing) => {
                    let prompts = json_items(&listing, "prompts");
                    if prompts.is_empty() {
                        lines.push("  (none)".to_owned());
                    }
                    for item in prompts {
                        lines.push(format!(
                            "  {}  {}",
                            json_text(&item, "name"),
                            json_text(&item, "description")
                        ));
                    }
                }
                Err(error) => lines.push(format!("  error: {error:#}")),
            },
            _ => {
                let caps = session.capabilities();
                let flag = |path: &str| {
                    caps.pointer(path).and_then(serde_json::Value::as_bool) == Some(true)
                };
                let mut any = false;
                for (label, supported) in [
                    ("tools/list_changed", flag("/tools/listChanged")),
                    ("resources/list_changed", flag("/resources/listChanged")),
                    ("prompts/list_changed", flag("/prompts/listChanged")),
                    ("resources/subscribe", flag("/resources/subscribe")),
                ] {
                    any |= supported;
                    lines.push(format!("  {} {label}", if supported { "✓" } else { "✗" }));
                }
                if !any {
                    lines.push("  no notification capabilities declared".to_owned());
                }
            }
        }
    }
    lines.extend(load.notices.iter().cloned());
    lines.join("\n")
}

fn json_items(value: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn json_text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

const MCP_HELP: &str = "/mcp list\n/mcp add <name> --url <url> [--transport http|sse] [--bearer-env VAR]\n/mcp add <name> [--scope project|user] -- <command> [args...]\n/mcp remove <name>\n/mcp enable <name>\n/mcp disable <name>\n/mcp test <name>\n/mcp login <name>\n/mcp logout <name>\n/mcp resources\n/mcp prompts\n/mcp prompt <server> <name> [key=value…]\n/mcp notifications\n/mcp reload";

fn mcp_list(config: &Config) -> String {
    if config.mcp.is_empty() {
        return "No MCP servers configured. Add one with /mcp add <name> --url <url> or /mcp add <name> -- <command>.".into();
    }
    config
        .mcp
        .iter()
        .map(|(name, server)| {
            let target = super::extensions_browser::server_command(server);
            format!(
                "{name}  {}  {target}",
                if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_mcp_add(rest: &str) -> Result<(String, McpServerConfig, Scope)> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut name = None;
    let mut server = McpServerConfig::default();
    let mut scope = Scope::Project;
    let mut command = None;
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if token == "--" {
            command = Some(
                tokens[i + 1..]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>(),
            );
            break;
        }
        if let Some(flag) = token.strip_prefix("--") {
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| anyhow!("--{flag} needs a value"))?;
            match flag {
                "url" => server.url = Some((*value).to_owned()),
                "transport" => server.transport = Some(parse_transport(value)?),
                "bearer-env" => server.bearer_env = Some((*value).to_owned()),
                "scope" => {
                    scope = match *value {
                        "project" => Scope::Project,
                        "user" | "global" => Scope::Global,
                        _ => bail!("Invalid --scope value. Use project or user."),
                    };
                }
                other => bail!("unknown option --{other}"),
            }
            i += 2;
            continue;
        }
        if name.is_some() {
            bail!("unexpected argument {token}");
        }
        name = Some(token.to_owned());
        i += 1;
    }
    let name =
        name.ok_or_else(|| anyhow!("usage: /mcp add <name> (--url <url> | -- <command>)"))?;
    if !valid_server_name(&name) {
        bail!("MCP server names may only use letters, digits, '-' and '_' (max 64): {name}");
    }
    if let Some(parts) = command {
        let (program, args) = parts
            .split_first()
            .ok_or_else(|| anyhow!("-- must be followed by a command"))?;
        server.program = PathBuf::from(program);
        server.args = args.to_vec();
    }
    validate_server(&name, &server)?;
    Ok((name, server, scope))
}

fn parse_transport(value: &str) -> Result<McpTransport> {
    Ok(match value {
        "stdio" => McpTransport::Stdio,
        "http" => McpTransport::Http,
        "sse" => McpTransport::Sse,
        other => bail!("unknown transport {other}; use stdio, http or sse"),
    })
}

/// Layer that owns `name`: project when the project config defines it, otherwise user.
pub(super) fn mcp_scope(layout: &crate::config::paths::Layout, name: &str) -> Result<Scope> {
    let project = load_layer(layout, Scope::Project)?;
    let defined = project
        .get("mcp")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(name));
    Ok(if defined {
        Scope::Project
    } else {
        Scope::Global
    })
}

fn existing_server<'a>(config: &Config, rest: &'a str, sub: &str) -> Result<&'a str> {
    let name = one_arg(rest, sub)?;
    if !config.mcp.contains_key(name) {
        bail!("No MCP server named {name}.");
    }
    Ok(name)
}

// ---- /tools, /extensions, /agents, /hub ------------------------------------------------------

fn tools_report(agent: &Agent<'_>) -> String {
    let offered = agent.tool_names();
    let available = agent.available_tool_names();
    let hidden: Vec<&String> = available
        .iter()
        .filter(|name| !offered.contains(*name))
        .collect();
    let mut lines = vec![format!(
        "Offered ({}): {}",
        offered.len(),
        join_or_none(offered.as_slice())
    )];
    lines.push(format!(
        "Available but not offered ({}): {}",
        hidden.len(),
        join_or_none(hidden.as_slice())
    ));
    lines.join("\n")
}

fn extensions_report(config: &Config) -> String {
    format!("Configured MCP servers: {}", config.mcp.len())
}

fn agents_report(config: &Config) -> String {
    let catalog = Catalog::load(config);
    let mut lines = Vec::new();
    for def in catalog.iter() {
        let model = if def.model.is_empty() {
            "inherit".to_owned()
        } else {
            def.model.join(", ")
        };
        lines.push(format!(
            "{}  {}  model: {model}{}",
            def.name,
            def.description,
            if def.mutates { "  (mutates files)" } else { "" }
        ));
    }
    if lines.is_empty() {
        "No agent definitions are available.".into()
    } else {
        lines.join("\n")
    }
}

// ---- shared helpers --------------------------------------------------------------------------

/// Run `work` on a worker thread and deliver its result through the shell's background job
/// slot, which the main loop polls and reports under `label`.
pub(super) fn spawn_job(
    shell: &mut Shell,
    label: &'static str,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<String> + Send + 'static,
) {
    spawn_job_streaming(shell, label, move |cancel, _progress| work(cancel));
}

/// Like [`spawn_job`], but `work` also gets a progress sink. Each line it reports is noted
/// under `label` as the main loop drains it, so long installs show live steps.
pub(super) fn spawn_job_streaming(
    shell: &mut Shell,
    label: &'static str,
    work: impl FnOnce(Arc<AtomicBool>, &mut dyn FnMut(&str)) -> Result<String> + Send + 'static,
) {
    if !crate::app::start_streaming_job(&mut shell.auth_job, label, work) {
        shell.note(
            label,
            "Another background operation is still running; try again when it finishes.",
        );
        return;
    }
    shell.note_pending(label);
}

fn reload_runtime(shell: &mut Shell, app: &App, agent: Option<&mut Agent<'_>>) -> Step {
    let Some(agent) = agent else {
        // During a turn: anything changed is held for the turn's end (`adopt_after_turn`).
        shell.refresh_resources(app, true, true);
        shell.note("MCP", "Reconnects when the current turn finishes.");
        return Step::Done;
    };
    shell.refresh_resources(app, true, false);
    if let Some(config) = shell.pending_config.take() {
        return shell.config_changed(app, agent, config);
    }
    if std::mem::take(&mut shell.pending_resource_rebuild) {
        return Step::Rebuild(Box::new(app.config.clone()));
    }
    Step::Done
}

fn split_subcommand(args: &str) -> (&str, &str) {
    match args.trim().split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (args.trim(), ""),
    }
}

fn one_arg<'a>(rest: &'a str, sub: &str) -> Result<&'a str> {
    let mut parts = rest.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(name), None) => Ok(name),
        _ => bail!("usage: {sub} <name>"),
    }
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn join_or_none<S: AsRef<str>>(items: &[S]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests;
