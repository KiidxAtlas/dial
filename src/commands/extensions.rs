//! Extension slash commands: `/mcp`, `/tools`, `/extensions`, `/agents`, `/hub`, `/plugins`,
//! `/marketplace`, `/reload-plugins`, `/security`.

use crate::agent::Agent;
use crate::agent::agent_defs::Catalog;
use crate::app::{App, Shell, Step, Which};
use crate::config::types::{Config, McpServerConfig, McpTransport};
use crate::llm::oauth::BannerInteraction;
use crate::storage::settings_store::{Change, Scope, load_layer};
use crate::tools::mcp::{connect_extra, validate_server};
use crate::tools::plugin::installer::{InstalledPlugin, PluginStore};
use crate::tools::plugin::marketplace::{
    MarketplaceEntry, MarketplaceInfo, Marketplaces, SkillListing,
};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
        description: "show runtime extension and plugin configuration (alias: /status)",
        args_hint: "",
        subcommands: &[],
    },
    super::CommandSpec {
        name: "plugins",
        description: "list, enable, or disable installed plugins (alias: /plugin)",
        args_hint: "[list|enable|disable] [name]",
        subcommands: &["list", "enable", "disable", "help"],
    },
    super::CommandSpec {
        name: "marketplace",
        description: "list plugins and skills from marketplaces; add, remove, enable, disable, install",
        args_hint: "[list|add|remove|enable|disable|update|discover|install|uninstall|installed|upgrade|help]",
        subcommands: &[
            "add",
            "remove",
            "enable",
            "disable",
            "update",
            "list",
            "discover",
            "install",
            "uninstall",
            "installed",
            "upgrade",
            "help",
        ],
    },
    super::CommandSpec {
        name: "reload-plugins",
        description: "reload resources and restart the runtime to load plugin changes",
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
    let title = match name {
        "mcp" => "MCP",
        "tools" => "Tools",
        "extensions" | "status" => "Extensions",
        "agents" => "Agents",
        "plugins" | "plugin" => "Plugins",
        "marketplace" => "Marketplace",
        "reload-plugins" => "Plugins",
        _ => return None,
    };
    let result = match name {
        "mcp" => mcp(shell, app, agent, args),
        "tools" => {
            shell.note(title, tools_report(agent));
            Ok(Step::Done)
        }
        "extensions" => shell.open(app, agent, Which::Extensions),
        "status" => {
            shell.note(title, extensions_report(&app.config));
            Ok(Step::Done)
        }
        "agents" => {
            shell.note(title, agents_report(&app.config));
            Ok(Step::Done)
        }
        "plugins" | "plugin" => plugins(shell, app, args),
        "marketplace" => marketplace(shell, app, args),
        "reload-plugins" => {
            shell.note(title, "Reloading plugins.");
            Ok(Step::Rebuild(Box::new(app.config.clone())))
        }
        _ => return None,
    };
    Some(match result {
        Ok(step) => Ok(step),
        Err(error) => {
            shell.note(title, format!("{error:#}"));
            Ok(Step::Done)
        }
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
        "notifications",
    ] {
        out.push(format!("/mcp {sub}"));
    }
    for plugin in plugin_store(config).list().unwrap_or_default() {
        for sub in ["enable", "disable"] {
            out.push(format!("/plugins {sub} {}", plugin.name));
        }
    }
    out
}

// ---- /mcp ----------------------------------------------------------------------------------

fn mcp(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let (sub, rest) = split_subcommand(args);
    match sub {
        "" | "list" => {
            shell.note("MCP", mcp_list(&app.config));
            Ok(Step::Done)
        }
        "help" => {
            shell.note("MCP", MCP_HELP);
            Ok(Step::Done)
        }
        "add" => {
            let (name, server, scope) = parse_mcp_add(rest)?;
            if app.config.mcp.contains_key(&name) {
                bail!("MCP server {name} already exists; remove it first.");
            }
            let change = Change::Set {
                path: format!("mcp.{name}"),
                value: toml::Value::try_from(&server)?,
            };
            let step = shell.persist(app, agent, &[change], scope)?;
            shell.note(
                "MCP",
                format!("Added MCP server {name}. It takes effect at the next rebuild; the check below uses a temporary connection."),
            );
            let root = app.config.project_root.clone();
            let owned = name;
            spawn_job(shell, "MCP test", move |_| {
                let specs = BTreeMap::from([(owned.clone(), server)]);
                let load = connect_extra(&specs, &root);
                let mut lines = vec![format!("{owned}: {} tool(s)", load.tools.len())];
                lines.extend(load.tools.iter().map(|tool| format!("  {}", tool.name())));
                lines.extend(load.notices);
                Ok(lines.join("\n"))
            });
            Ok(step)
        }
        "remove" => {
            let name = existing_server(&app.config, rest, "remove")?;
            let step = shell.persist(
                app,
                agent,
                &[Change::Remove {
                    path: format!("mcp.{name}"),
                }],
                server_scope(app, name)?,
            )?;
            shell.note("MCP", format!("Removed MCP server {name}."));
            Ok(step)
        }
        "enable" | "disable" => {
            let name = existing_server(&app.config, rest, sub)?;
            let enabled = sub == "enable";
            let step = shell.persist(
                app,
                agent,
                &[Change::Set {
                    path: format!("mcp.{name}.enabled"),
                    value: toml::Value::Boolean(enabled),
                }],
                server_scope(app, name)?,
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
        "test" => {
            let name = existing_server(&app.config, rest, "test")?;
            let server = app.config.mcp[name].clone();
            let root = app.config.project_root.clone();
            let owned = name.to_owned();
            spawn_job(shell, "MCP test", move |_| {
                let specs = BTreeMap::from([(owned.clone(), server)]);
                let load = connect_extra(&specs, &root);
                let mut lines = vec![format!("{owned}: {} tool(s)", load.tools.len())];
                lines.extend(load.tools.iter().map(|tool| format!("  {}", tool.name())));
                lines.extend(load.notices);
                Ok(lines.join("\n"))
            });
            Ok(Step::Done)
        }
        "login" => {
            let name = existing_server(&app.config, rest, "login")?;
            let server = app.config.mcp[name].clone();
            let user_dir = app.config.user_dir.clone();
            let owned = name.to_owned();
            spawn_job(shell, "MCP login", move |cancel| {
                let ui = BannerInteraction { id: "mcp" };
                crate::tools::mcp::login(&user_dir, &owned, &server, &ui, cancel)
            });
            Ok(Step::Done)
        }
        "reload" | "reconnect" => Ok(reload_runtime(shell, app, agent)),
        "logout" | "unauth" => {
            let name = existing_server(&app.config, rest, sub)?;
            let removed = crate::tools::mcp::logout(&app.config.user_dir, name)?;
            shell.note(
                "MCP",
                if removed {
                    format!("Signed out of MCP server {name}. A live connection keeps its token until it reconnects.")
                } else {
                    format!("No stored sign-in for MCP server {name}.")
                },
            );
            Ok(Step::Done)
        }
        "resources" | "prompts" | "notifications" => {
            let specs: BTreeMap<String, McpServerConfig> = app
                .config
                .mcp
                .iter()
                .filter(|(_, server)| server.enabled)
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect();
            let root = app.config.project_root.clone();
            let kind = sub.to_owned();
            spawn_job(shell, "MCP", move |_| {
                let load = connect_extra(&specs, &root);
                Ok(mcp_listing(&kind, &load))
            });
            Ok(Step::Done)
        }
        other => bail!("Unknown /mcp subcommand: {other}. Run /mcp help."),
    }
}

/// `/mcp resources|prompts|notifications`: one section per connected server, from its live
/// session. A failing server reports its own error and does not hide the others.
fn mcp_listing(kind: &str, load: &crate::tools::mcp::McpLoad) -> String {
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

const MCP_HELP: &str = "/mcp list\n/mcp add <name> --url <url> [--transport http|sse] [--bearer-env VAR]\n/mcp add <name> [--scope project|user] -- <command> [args...]\n/mcp remove <name>\n/mcp enable <name>\n/mcp disable <name>\n/mcp test <name>\n/mcp login <name>\n/mcp logout <name>\n/mcp resources\n/mcp prompts\n/mcp notifications\n/mcp reload";

fn mcp_list(config: &Config) -> String {
    if config.mcp.is_empty() {
        return "No MCP servers configured. Add one with /mcp add <name> --url <url> or /mcp add <name> -- <command>.".into();
    }
    config
        .mcp
        .iter()
        .map(|(name, server)| {
            let target = match &server.url {
                Some(url) => url.clone(),
                None => std::iter::once(server.program.display().to_string())
                    .chain(server.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
            };
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
fn server_scope(app: &App, name: &str) -> Result<Scope> {
    let project = load_layer(&app.layout, Scope::Project)?;
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
    let ext = &config.extensions;
    let mut lines = vec![
        format!("Runtime plugins: {}", on_off(ext.enabled)),
        format!("Discover ~/.dial/extensions: {}", on_off(ext.discover)),
        format!("Request timeout: {}s (0 disables)", ext.timeout_secs),
    ];
    if ext.plugins.is_empty() {
        lines.push("Configured plugins: none".into());
    } else {
        lines.push("Configured plugins:".into());
        for (name, plugin) in &ext.plugins {
            lines.push(format!(
                "  {name}  {}  {}",
                on_off(plugin.enabled),
                plugin.program.display()
            ));
        }
    }
    lines.join("\n")
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

// ---- /plugins, /marketplace, /reload-plugins -------------------------------------------------

fn plugins(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let (sub, rest) = split_subcommand(args);
    let rest = user_scope_only(rest)?;
    let store = plugin_store(&app.config);
    match sub {
        "" | "list" => {
            shell.note("Plugins", plugin_list(&store.list()?));
            Ok(Step::Done)
        }
        "enable" | "disable" => {
            let name = one_arg(&rest, sub)?;
            let enabled = sub == "enable";
            store.set_enabled(name, enabled)?;
            shell.note(
                "Plugins",
                format!(
                    "{} plugin {name}.",
                    if enabled { "Enabled" } else { "Disabled" }
                ),
            );
            Ok(Step::Rebuild(Box::new(app.config.clone())))
        }
        "help" => {
            shell.note(
                "Plugins",
                "/plugins list\n/plugins enable <name>\n/plugins disable <name>",
            );
            Ok(Step::Done)
        }
        other => bail!("Unknown /plugins subcommand: {other}. Use list, enable, or disable."),
    }
}

fn marketplace(shell: &mut Shell, app: &App, args: &str) -> Result<Step> {
    let (sub, rest) = split_subcommand(args);
    let plugins_root = plugins_root(&app.config);
    let markets_root = marketplaces_root(&app.config);
    match sub {
        "" | "list" => {
            let markets = Marketplaces::new(&markets_root);
            let mut text = marketplace_list(&markets.list()?);
            text.push_str("\n\n");
            text.push_str(&offerings(&markets.entries(None)?, &markets.skills(None)?));
            text.push_str("\n\nInstall and manage these in /extensions (Plugins, Skills and Marketplaces tabs).");
            shell.note("Marketplace", text);
            Ok(Step::Done)
        }
        "add" => {
            let (name, index) = two_args(rest, "add")?;
            Marketplaces::new(&markets_root).add(name, index)?;
            shell.note("Marketplace", format!("Added marketplace {name}."));
            Ok(Step::Done)
        }
        "remove" => {
            let name = one_arg(rest, "remove")?;
            Marketplaces::new(&markets_root).remove(name)?;
            shell.note("Marketplace", format!("Removed marketplace {name}."));
            Ok(Step::Done)
        }
        "enable" | "disable" => {
            let name = one_arg(rest, sub)?;
            let enabled = sub == "enable";
            Marketplaces::new(&markets_root).set_enabled(name, enabled)?;
            shell.note(
                "Marketplace",
                format!(
                    "{} marketplace {name}.",
                    if enabled { "Enabled" } else { "Disabled" }
                ),
            );
            Ok(Step::Done)
        }
        "update" => {
            let name = optional(rest);
            spawn_job(shell, "Marketplace", move |_| {
                Marketplaces::new(&markets_root).refresh(name.as_deref())?;
                Ok(match name {
                    Some(name) => format!("Refreshed marketplace {name}."),
                    None => "Refreshed all marketplaces.".to_owned(),
                })
            });
            Ok(Step::Done)
        }
        "discover" => {
            let filter = optional(rest);
            let markets = Marketplaces::new(&markets_root);
            let text = offerings(
                &markets.entries(filter.as_deref())?,
                &markets.skills(filter.as_deref())?,
            );
            shell.note("Marketplace", text);
            Ok(Step::Done)
        }
        "install" => {
            let (force, reference) = parse_install(rest)?;
            spawn_job(shell, "Marketplace", move |_| {
                let store = PluginStore::new(&plugins_root);
                // A forced reinstall moves the old tree aside instead of deleting it first, so a
                // failed install restores the working plugin.
                let mut backup = None;
                if force {
                    let bare = reference
                        .split('@')
                        .next()
                        .unwrap_or(reference.as_str())
                        .to_owned();
                    if let Ok(existing) = store.get(&bare) {
                        let aside =
                            plugins_root.join(format!(".replaced-{bare}-{}", std::process::id()));
                        std::fs::rename(&existing.path, &aside).map_err(|error| {
                            anyhow!("could not move plugin {bare} aside: {error}")
                        })?;
                        backup = Some((aside, existing.path));
                    }
                }
                let plugin = match Marketplaces::new(&markets_root).install(&reference, &store) {
                    Ok(plugin) => {
                        if let Some((aside, _)) = &backup {
                            let _ = std::fs::remove_dir_all(aside);
                        }
                        plugin
                    }
                    Err(error) => {
                        if let Some((aside, original)) = &backup {
                            std::fs::rename(aside, original).map_err(|restore| {
                                anyhow!(
                                    "{error:#}; also could not restore the previous plugin from {}: {restore}",
                                    aside.display()
                                )
                            })?;
                        }
                        return Err(error);
                    }
                };
                Ok(format!(
                    "Installed {} {}. Run /reload-plugins to load it.",
                    plugin.name,
                    plugin.version.as_deref().unwrap_or("")
                ))
            });
            Ok(Step::Done)
        }
        "uninstall" => {
            let name = one_arg(rest, "uninstall")?;
            PluginStore::new(&plugins_root).remove(name)?;
            shell.note(
                "Marketplace",
                format!("Uninstalled {name}. Run /reload-plugins to apply."),
            );
            Ok(Step::Done)
        }
        "installed" => {
            shell.note(
                "Marketplace",
                plugin_list(&PluginStore::new(&plugins_root).list()?),
            );
            Ok(Step::Done)
        }
        "upgrade" => {
            let name = optional(rest);
            spawn_job(shell, "Marketplace", move |_| {
                let store = PluginStore::new(&plugins_root);
                let updates = Marketplaces::new(&markets_root).upgrade(&store, name.as_deref())?;
                Ok(if updates.is_empty() {
                    "No plugins to upgrade.".to_owned()
                } else {
                    updates
                        .iter()
                        .map(|u| {
                            format!(
                                "{}: {} -> {}{}",
                                u.name,
                                u.from.as_deref().unwrap_or("-"),
                                u.to.as_deref().unwrap_or("-"),
                                if u.changed { "" } else { " (unchanged)" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            });
            Ok(Step::Done)
        }
        "help" => {
            shell.note(
                "Marketplace",
                "/marketplace list\n/marketplace add <name> <index-url-or-path>\n/marketplace remove <name>\n/marketplace enable <name>\n/marketplace disable <name>\n/marketplace update [name]\n/marketplace discover [name]\n/marketplace install [--force] <plugin@marketplace>\n/marketplace uninstall <plugin>\n/marketplace installed\n/marketplace upgrade [plugin]\nThe built-in anthropic-skills marketplace is on by default. Browse and install in /extensions.",
            );
            Ok(Step::Done)
        }
        other => bail!("Unknown /marketplace subcommand: {other}. Run /marketplace help."),
    }
}

fn plugin_list(plugins: &[InstalledPlugin]) -> String {
    if plugins.is_empty() {
        return "No plugins installed.".into();
    }
    plugins
        .iter()
        .map(|p| {
            format!(
                "{}  {}  {}  {}",
                p.name,
                p.version.as_deref().unwrap_or("-"),
                if p.enabled { "enabled" } else { "disabled" },
                p.source
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn marketplace_list(markets: &[MarketplaceInfo]) -> String {
    if markets.is_empty() {
        return "No marketplaces configured. Use /marketplace add <name> <index>.".into();
    }
    markets
        .iter()
        .map(|m| {
            let refreshed = m
                .refreshed_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "never".into());
            let index = if m.builtin {
                "built into Dial".to_owned()
            } else {
                m.index.clone()
            };
            format!(
                "{}  {}  {index}  refreshed: {refreshed}",
                m.name,
                if m.enabled { "enabled" } else { "disabled" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plugin entries and skills offered by marketplaces, one per line.
fn offerings(entries: &[MarketplaceEntry], skills: &[SkillListing]) -> String {
    if entries.is_empty() && skills.is_empty() {
        return "No marketplace plugins or skills found.".into();
    }
    let mut lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            format!(
                "plugin  {}@{}  {}  {}",
                entry.name,
                entry.marketplace,
                entry.version.as_deref().unwrap_or("-"),
                entry
                    .description
                    .as_deref()
                    .unwrap_or(entry.source.as_str())
            )
        })
        .collect();
    lines.extend(skills.iter().map(|skill| {
        format!(
            "skill   {}@{}  {}",
            skill.name, skill.marketplace, skill.description
        )
    }));
    lines.join("\n")
}

fn parse_install(rest: &str) -> Result<(bool, String)> {
    let mut force = false;
    let mut reference = None;
    for token in rest.split_whitespace() {
        if token == "--force" {
            force = true;
        } else if reference.is_none() {
            reference = Some(token.to_owned());
        } else {
            bail!("unexpected argument {token}");
        }
    }
    let reference = reference
        .ok_or_else(|| anyhow!("usage: /marketplace install [--force] <plugin@marketplace>"))?;
    Ok((force, reference))
}

fn user_scope_only(rest: &str) -> Result<String> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut kept = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "--scope" {
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| anyhow!("--scope needs a value"))?;
            if *value != "user" {
                bail!(
                    "Plugins are installed in the user directory only; --scope {value} is not supported."
                );
            }
            i += 2;
        } else {
            kept.push(tokens[i]);
            i += 1;
        }
    }
    Ok(kept.join(" "))
}

fn plugin_store(config: &Config) -> PluginStore {
    PluginStore::new(plugins_root(config))
}

fn plugins_root(config: &Config) -> PathBuf {
    config.user_dir.join("plugins")
}

fn marketplaces_root(config: &Config) -> PathBuf {
    config.user_dir.join("marketplaces")
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

fn reload_runtime(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Step {
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

fn two_args<'a>(rest: &'a str, sub: &str) -> Result<(&'a str, &'a str)> {
    let mut parts = rest.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None) => Ok((a, b)),
        _ => bail!("usage: {sub} <name> <index>"),
    }
}

fn optional(rest: &str) -> Option<String> {
    let trimmed = rest.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
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

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_subcommand_and_rest() {
        assert_eq!(
            split_subcommand("  add  docs --url x "),
            ("add", "docs --url x")
        );
        assert_eq!(split_subcommand("list"), ("list", ""));
        assert_eq!(split_subcommand(""), ("", ""));
    }

    #[test]
    fn validates_server_names() {
        assert!(valid_server_name("docs-1_a"));
        assert!(!valid_server_name(""));
        assert!(!valid_server_name("a.b"));
        assert!(!valid_server_name("a b"));
        assert!(!valid_server_name(&"x".repeat(65)));
    }

    #[test]
    fn parses_transport_names() {
        assert!(parse_transport("sse").is_ok());
        assert!(parse_transport("stdio").is_ok());
        assert!(parse_transport("ws").is_err());
    }

    #[test]
    fn parses_mcp_add_scope() {
        let (_, _, scope) = parse_mcp_add("docs -- npx srv").unwrap();
        assert_eq!(scope, Scope::Project);
        let (_, _, scope) = parse_mcp_add("docs --scope user -- npx srv").unwrap();
        assert_eq!(scope, Scope::Global);
        let err = parse_mcp_add("docs --scope workspace -- npx srv").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid --scope value. Use project or user."
        );
    }
}
