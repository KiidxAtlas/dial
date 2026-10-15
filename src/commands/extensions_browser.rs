//! Data and install work behind `/extensions`. The overlay in `ui::overlay::extensions` only shows
//! rows and emits intents; this module builds the rows, plans installs and runs them on a worker
//! thread. Config changes come back as [`Change`]s and are saved on the main thread by [`poll`],
//! and only after the install and its health check succeed.
use crate::ui::overlay::extensions::ExtensionsOverlay;
use crate::{
    agent::Agent,
    app::{App, Shell},
    config::{Config, types::McpServerConfig},
    storage::{
        paths::Layout,
        settings_store::{Change, Scope},
        skills::packages,
    },
    tools::{
        mcp::{self, smithery},
        plugin::{installer::PluginStore, marketplace::Marketplaces},
    },
    ui::overlay::extensions::model::{
        CatalogUpdate, Field, Form, Intent, JobProgress, Origin, Row, SharedCatalog,
        SharedProgress, State, Tab,
    },
};
use anyhow::{Result, anyhow};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, channel},
    thread,
};

/// A built-in MCP server that needs no API key.
pub(crate) struct Curated {
    pub id: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    /// Runtime that must be on PATH; also the program that runs the server.
    pub runtime: &'static str,
    pub args: &'static [&'static str],
    /// Whether the project root is appended as the last argument (filesystem server).
    pub takes_root: bool,
    /// Tools the server exposes, for the details pane.
    pub tools: &'static str,
}

pub(crate) const CURATED: &[Curated] = &[
    Curated {
        id: "memory",
        title: "Memory",
        summary: "persistent knowledge graph across sessions",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-memory"],
        takes_root: false,
        tools: "create_entities, create_relations, search_nodes, read_graph",
    },
    Curated {
        id: "filesystem",
        title: "Filesystem",
        summary: "read and write files under the project root",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem"],
        takes_root: true,
        tools: "read_file, write_file, list_directory, search_files",
    },
    Curated {
        id: "sequential-thinking",
        title: "Sequential thinking",
        summary: "structured step-by-step reasoning tool",
        runtime: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        takes_root: false,
        tools: "sequentialthinking",
    },
    Curated {
        id: "fetch",
        title: "Fetch",
        summary: "download web pages as markdown",
        runtime: "uvx",
        args: &["mcp-server-fetch"],
        takes_root: false,
        tools: "fetch",
    },
    Curated {
        id: "time",
        title: "Time",
        summary: "current time and timezone conversion",
        runtime: "uvx",
        args: &["mcp-server-time"],
        takes_root: false,
        tools: "get_current_time, convert_time",
    },
];

fn curated(id: &str) -> Result<&'static Curated> {
    CURATED
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| anyhow!("`{id}` is not in the curated MCP list"))
}

/// Rows for the three tabs, built from disk and config.
#[derive(Default)]
pub(crate) struct Snapshot {
    pub mcp: Vec<Row>,
    pub plugins: Vec<Row>,
    pub skills: Vec<Row>,
}

pub(crate) fn snapshot(layout: &Layout, config: &Config) -> Snapshot {
    Snapshot {
        mcp: mcp_rows(config),
        plugins: plugin_rows(config),
        skills: skill_rows(layout),
    }
}

/// A fresh overlay for `/extensions`: rows from disk and config, and the running install's progress
/// when one is in flight.
pub(crate) fn overlay(
    layout: &Layout,
    config: &Config,
    running: Option<&InstallJob>,
) -> ExtensionsOverlay {
    let mut panel = ExtensionsOverlay::new();
    refresh(&mut panel, layout, config);
    if let Some(job) = running {
        panel.attach(job.progress.clone());
    }
    panel
}

/// Replace the rows with what is on disk and in config now, and restart the catalog listings.
pub(crate) fn refresh(panel: &mut ExtensionsOverlay, layout: &Layout, config: &Config) {
    let mut rows = snapshot(layout, config);
    // Smithery search results live only in the overlay until the next search.
    let found = panel
        .browser
        .rows(Tab::Mcp)
        .iter()
        .filter(|row| row.origin == Origin::Remote)
        .cloned();
    rows.mcp.extend(found);
    panel.set_snapshot(rows.mcp, rows.plugins, rows.skills);
    for tab in [Tab::Mcp, Tab::Plugins] {
        let listing = SharedCatalog::default();
        let config = config.clone();
        let shared = listing.clone();
        thread::spawn(move || {
            let update = catalog_update(&smithery::registry_url(), &config, tab, "", 1);
            if let Ok(mut slot) = shared.lock() {
                *slot = Some(update);
            }
        });
        panel.attach_catalog(tab, listing);
    }
}

fn skill_rows(layout: &Layout) -> Vec<Row> {
    packages::list_packages(layout)
        .unwrap_or_default()
        .into_iter()
        .map(|package| {
            let source = package.source.spec();
            let scope = package.scope.as_str();
            Row {
                id: package.name.clone(),
                title: package.name.clone(),
                summary: source.clone(),
                tags: vec![scope.into(), "package".into()],
                origin: Origin::Installed,
                state: State::Enabled,
                details: vec![
                    format!("source: {source}"),
                    format!("commit: {}", package.commit),
                    format!("scope: {scope}"),
                    format!("skills: {}", package.skills.join(", ")),
                ],
            }
        })
        .collect()
}

fn mcp_rows(config: &Config) -> Vec<Row> {
    let mut rows: Vec<Row> = config
        .mcp
        .iter()
        .map(|(name, server)| Row {
            id: name.clone(),
            title: name.clone(),
            summary: server_summary(server),
            tags: vec!["installed".into()],
            origin: Origin::Installed,
            state: if server.enabled {
                State::Enabled
            } else {
                State::Disabled
            },
            details: vec![format!("command: {}", server_command(server))],
        })
        .collect();
    for item in CURATED.iter().filter(|c| !config.mcp.contains_key(c.id)) {
        rows.push(Row {
            id: item.id.into(),
            title: item.title.into(),
            summary: item.summary.into(),
            tags: vec!["curated".into(), "no key".into()],
            origin: Origin::Curated,
            state: State::Available,
            details: vec![
                format!("runs: {} {}", item.runtime, item.args.join(" ")),
                format!("needs: {} on PATH", item.runtime),
                format!("installs to: {}", mcp_cache_dir(&config.user_dir).display()),
                format!("tools: {}", item.tools),
            ],
        });
    }
    rows
}

fn server_summary(server: &McpServerConfig) -> String {
    match &server.url {
        Some(url) => url.clone(),
        None => server.program.display().to_string(),
    }
}

fn server_command(server: &McpServerConfig) -> String {
    match &server.url {
        Some(url) => url.clone(),
        None => std::iter::once(server.program.display().to_string())
            .chain(server.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn plugin_rows(config: &Config) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Ok(installed) = PluginStore::new(plugins_root(config)).list() {
        for plugin in installed {
            rows.push(Row {
                id: plugin.name.clone(),
                title: plugin.name.clone(),
                summary: plugin.version.clone().unwrap_or_else(|| "installed".into()),
                tags: vec!["installed".into()],
                origin: Origin::Installed,
                state: if plugin.enabled {
                    State::Enabled
                } else {
                    State::Disabled
                },
                details: vec![
                    format!("source: {}", plugin.source),
                    format!("location: {}", plugin.path.display()),
                ],
            });
        }
    }
    // Marketplace entries come from the cached index; refresh runs through `/marketplace refresh`.
    if let Ok(entries) = Marketplaces::new(marketplaces_root(config)).entries(None) {
        for entry in entries {
            let id = format!("{}@{}", entry.name, entry.marketplace);
            let installed = rows
                .iter()
                .any(|r| r.id == id || (r.origin == Origin::Installed && r.id == entry.name));
            if installed {
                continue;
            }
            rows.push(Row {
                id,
                title: entry.name.clone(),
                summary: entry.description.clone().unwrap_or_default(),
                tags: vec![entry.marketplace.clone()],
                origin: Origin::Remote,
                state: State::Available,
                details: vec![
                    format!("marketplace: {}", entry.marketplace),
                    format!("source: {}", entry.source),
                    format!("installs to: {}", plugins_root(config).display()),
                ],
            });
        }
    }
    rows
}

fn plugins_root(config: &Config) -> PathBuf {
    config.user_dir.join("plugins")
}

fn marketplaces_root(config: &Config) -> PathBuf {
    config.user_dir.join("marketplaces")
}

fn mcp_cache_dir(user_dir: &Path) -> PathBuf {
    user_dir.join("mcp")
}

/// What an install will do, shown before the user confirms.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub command: String,
    pub location: PathBuf,
    /// Why the install cannot run yet (missing runtime, already configured).
    pub blocked: Option<String>,
}

pub(crate) fn plan_curated(config: &Config, id: &str) -> Result<Plan> {
    let item = curated(id)?;
    let (_, server) = curated_server(config, id)?;
    let command = std::iter::once(item.runtime.to_string())
        .chain(server.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let blocked = if config.mcp.contains_key(item.id) {
        Some(format!("`{id}` is already configured"))
    } else if which(item.runtime).is_none() {
        Some(format!(
            "{} is not on PATH. Install it, then try again.",
            item.runtime
        ))
    } else {
        None
    };
    Ok(Plan {
        command,
        location: mcp_cache_dir(&config.user_dir),
        blocked,
    })
}

/// The server entry that will be health-checked and then saved. Package caches are redirected under
/// Dial's user directory so nothing lands in a global cache the user did not pick.
pub(crate) fn curated_server(config: &Config, id: &str) -> Result<(String, McpServerConfig)> {
    let item = curated(id)?;
    let mut args: Vec<String> = item.args.iter().map(|s| s.to_string()).collect();
    if item.takes_root {
        args.push(config.project_root.display().to_string());
    }
    let cache = mcp_cache_dir(&config.user_dir);
    let env = BTreeMap::from([
        (
            "npm_config_cache".to_string(),
            cache.join("npm").display().to_string(),
        ),
        (
            "UV_CACHE_DIR".to_string(),
            cache.join("uv").display().to_string(),
        ),
    ]);
    let server = McpServerConfig {
        program: PathBuf::from(item.runtime),
        args,
        env,
        enabled: true,
        ..Default::default()
    };
    Ok((item.id.to_string(), server))
}

/// Finds an executable on PATH without running it.
pub(crate) fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Result of an intent. `changes` are saved by [`poll`] only when `ok` is true.
pub(crate) struct Outcome {
    pub ok: bool,
    pub message: String,
    pub changes: Vec<Change>,
}

/// One running intent: progress for the overlay plus the worker's final outcome.
pub(crate) struct InstallJob {
    outcome: Receiver<Outcome>,
    progress: SharedProgress,
    scope: Scope,
}

/// An intent the overlay asked for. The main loop starts it, because starting needs `App`.
pub(crate) struct Pending {
    intent: Intent,
    progress: SharedProgress,
    scope: Scope,
}

/// Queue an intent for the main loop. Returns the progress handle the overlay should follow.
/// A second request while one is queued is refused on the spot, not dropped.
pub(crate) fn queue(slot: &mut Option<Pending>, intent: Intent, scope: Scope) -> SharedProgress {
    let progress = SharedProgress::default();
    if slot.is_some() {
        fail(
            &progress,
            "another extension change is still starting".into(),
        );
    } else {
        *slot = Some(Pending {
            intent,
            progress: progress.clone(),
            scope,
        });
    }
    progress
}

fn fail(progress: &SharedProgress, message: String) {
    if let Ok(mut shown) = progress.lock() {
        shown.label = None;
        shown.result = Some((false, message));
    }
}

/// Start a queued intent on a worker thread. Refused when another intent is still running.
fn start(job: &mut Option<InstallJob>, pending: Pending, app: &App) {
    let Pending {
        intent,
        progress,
        scope,
    } = pending;
    if job.is_some() {
        fail(
            &progress,
            "another extension change is still running".into(),
        );
        return;
    }
    let config = match app.reload() {
        Ok(config) => config,
        Err(error) => return fail(&progress, format!("could not read settings: {error:#}")),
    };
    if let Ok(mut shown) = progress.lock() {
        *shown = JobProgress {
            label: Some(intent.describe()),
            ..Default::default()
        };
    }
    let layout = app.layout().clone();
    let (tx, rx) = channel();
    let worker = progress.clone();
    thread::spawn(move || {
        let outcome = match run(&layout, &config, &intent, &worker) {
            Ok(outcome) => outcome,
            Err(error) => Outcome {
                ok: false,
                message: format!("{error:#}"),
                changes: Vec::new(),
            },
        };
        let _ = tx.send(outcome);
    });
    *job = Some(InstallJob {
        outcome: rx,
        progress,
        scope,
    });
}

/// A finished intent, ready to save its config changes and publish its result.
pub(crate) struct Finished {
    ok: bool,
    message: String,
    changes: Vec<Change>,
    scope: Scope,
    progress: SharedProgress,
}

/// Start a queued intent if one is waiting, and return the finished job once its worker reports.
/// Both the main loop and the overlay loop call this, so an install runs while either is open.
pub(crate) fn step(
    pending: &mut Option<Pending>,
    job: &mut Option<InstallJob>,
    app: &App,
) -> Option<Finished> {
    if let Some(pending) = pending.take() {
        start(job, pending, app);
    }
    let running = job.as_ref()?;
    let outcome = match running.outcome.try_recv() {
        Ok(outcome) => outcome,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Outcome {
            ok: false,
            message: "The extensions worker stopped unexpectedly. Nothing was saved.".into(),
            changes: Vec::new(),
        },
    };
    let job = job.take()?;
    Some(Finished {
        ok: outcome.ok,
        message: outcome.message,
        changes: outcome.changes,
        scope: job.scope,
        progress: job.progress,
    })
}

/// Save the config changes of a successful install, then publish the result to the overlay.
/// Returns the message to show in the transcript.
pub(crate) fn finish(
    finished: Finished,
    save: impl FnOnce(&[Change], Scope) -> Result<()>,
) -> String {
    let Finished {
        ok,
        message,
        changes,
        scope,
        progress,
    } = finished;
    let (mut ok, mut message) = (ok, message);
    if ok && !changes.is_empty() {
        if let Err(error) = save(&changes, scope) {
            ok = false;
            message = format!("Done, but saving the config failed: {error:#}");
        }
    }
    if let Ok(mut shown) = progress.lock() {
        shown.label = None;
        shown.result = Some((ok, message.clone()));
    }
    message
}

/// Main-loop driver: starts and drains extension jobs, persisting through the live shell path.
/// Returns whether the main loop should redraw.
pub(crate) fn poll(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> bool {
    let Some(finished) = step(
        &mut shell.extensions_pending,
        &mut shell.extensions_job,
        app,
    ) else {
        return false;
    };
    let message = finish(finished, |changes, scope| {
        shell.persist(app, agent, changes, scope).map(|_| ())
    });
    shell.note("Extensions", message);
    true
}

fn run(
    layout: &Layout,
    config: &Config,
    intent: &Intent,
    progress: &SharedProgress,
) -> Result<Outcome> {
    run_at(&smithery::registry_url(), layout, config, intent, progress)
}

/// [`run`] against a given Smithery registry, so tests can point it at a local fake.
fn run_at(
    registry: &str,
    layout: &Layout,
    config: &Config,
    intent: &Intent,
    progress: &SharedProgress,
) -> Result<Outcome> {
    match intent {
        Intent::InstallCuratedMcp { id } => install_curated(config, id, progress),
        Intent::TestMcp { id } => test_mcp(config, id, progress),
        Intent::ToggleMcp { id, enable } => Ok(Outcome {
            ok: true,
            message: format!("{} `{id}`", if *enable { "Enabled" } else { "Disabled" }),
            changes: vec![Change::Set {
                path: format!("mcp.{id}.enabled"),
                value: toml::Value::Boolean(*enable),
            }],
        }),
        Intent::RemoveMcp { id } => Ok(Outcome {
            ok: true,
            message: format!("Removed `{id}` from the config"),
            changes: vec![Change::Remove {
                path: format!("mcp.{id}"),
            }],
        }),
        Intent::InstallPlugin { id } => install_plugin(config, id, progress),
        Intent::RemovePlugin { id } => {
            PluginStore::new(plugins_root(config)).remove(id)?;
            Ok(Outcome {
                ok: true,
                message: format!("Removed plugin {id}"),
                changes: Vec::new(),
            })
        }
        Intent::RemoveSkill { id } => {
            let message = packages::remove(layout, id)?;
            Ok(Outcome {
                ok: true,
                message,
                changes: Vec::new(),
            })
        }
        Intent::SmitheryInstall { name } => smithery_install(registry, config, name, progress),
        Intent::SmitheryKey { name, key } => smithery_key(registry, config, name, key, progress),
        Intent::SmitheryDeploy { name, values } => {
            smithery_deploy(registry, config, name, values, progress)
        }
        Intent::SmitheryDiscover { query } => smithery_discover(registry, config, query, progress),
        Intent::CatalogBrowse { tab, query, page } => {
            let update = catalog_update(registry, config, *tab, query, *page);
            let ok = update.error.is_none();
            let message = update
                .error
                .clone()
                .unwrap_or_else(|| update.message.clone());
            if let Ok(mut shown) = progress.lock() {
                shown.catalog = Some(update);
            }
            Ok(Outcome {
                ok,
                message,
                changes: Vec::new(),
            })
        }
        Intent::AddMarketplace { name, index } => {
            Marketplaces::new(marketplaces_root(config)).add(name, index)?;
            let update = catalog_update(registry, config, Tab::Plugins, "", 1);
            if let Ok(mut shown) = progress.lock() {
                shown.catalog = Some(update);
            }
            Ok(Outcome {
                ok: true,
                message: format!("Added marketplace {name}"),
                changes: Vec::new(),
            })
        }
    }
}

/// Provider IO belongs on the command worker, never in the overlay. Errors retain cached rows.
fn catalog_update(
    registry: &str,
    config: &Config,
    tab: Tab,
    query: &str,
    page: usize,
) -> CatalogUpdate {
    let page = page.max(1);
    let result = (|| -> Result<(Vec<Row>, bool, String)> {
        match tab {
            Tab::Mcp => {
                let key = smithery::resolve_api_key(&config.user_dir)?;
                let (found, more) = smithery::browse_page_at(registry, query, page, key.as_deref())
                    .map_err(|error| anyhow!("{}", error.message))?;
                let rows: Vec<_> = found.iter().map(remote_row).collect();
                let message = format!(
                    "Smithery page {page}: {} usable servers · {}",
                    rows.len(),
                    if more {
                        "n loads more"
                    } else {
                        "end of results"
                    }
                );
                Ok((rows, more, message))
            }
            Tab::Plugins => {
                let markets = Marketplaces::new(marketplaces_root(config));
                if markets.list()?.is_empty() {
                    return Ok((Vec::new(), false,
                        "No marketplaces registered. a adds a trusted JSON index; /marketplace add <name> <index> also works.".into()));
                }
                markets.refresh(None)?;
                // Validate the cached listing here so a corrupt index is not silently shown as empty.
                markets.entries(None)?;
                let rows: Vec<_> = plugin_rows(config)
                    .into_iter()
                    .filter(|row| row.origin == Origin::Remote)
                    .collect();
                let message = format!(
                    "{} marketplace plugins · / filters by name, description or marketplace · a adds an index",
                    rows.len()
                );
                Ok((rows, false, message))
            }
            Tab::Skills => Ok((
                Vec::new(),
                false,
                "Skill packages have no catalog; install one with /skills install <git-url>."
                    .into(),
            )),
        }
    })();
    match result {
        Ok((rows, more, message)) => CatalogUpdate {
            tab,
            query: query.into(),
            page,
            rows,
            more,
            message,
            error: None,
        },
        Err(error) => CatalogUpdate {
            tab,
            query: query.into(),
            page,
            rows: Vec::new(),
            more: false,
            message: String::new(),
            error: Some(format!("{error:#}")),
        },
    }
}

fn say(progress: &SharedProgress, line: impl Into<String>) {
    if let Ok(mut shown) = progress.lock() {
        shown.lines.push(line.into());
    }
}

fn install_curated(config: &Config, id: &str, progress: &SharedProgress) -> Result<Outcome> {
    let plan = plan_curated(config, id)?;
    if let Some(reason) = plan.blocked {
        return Ok(Outcome {
            ok: false,
            message: reason,
            changes: Vec::new(),
        });
    }
    say(progress, format!("running: {}", plan.command));
    say(
        progress,
        format!("caches under {}", plan.location.display()),
    );
    say(
        progress,
        "starting the server to check it (first run downloads packages)",
    );
    let (name, server) = curated_server(config, id)?;
    let tools = health_check(config, &name, &server)?;
    match tools {
        Ok(count) => {
            say(
                progress,
                format!("health check passed with {count} tool(s)"),
            );
            let change = crate::tools::mcp::smithery::config_change(&name, &server)?;
            Ok(Outcome {
                ok: true,
                message: format!("`{name}` connected with {count} tool(s)"),
                changes: vec![change],
            })
        }
        Err(why) => Ok(Outcome {
            ok: false,
            message: format!("health check failed, nothing saved: {why}"),
            changes: Vec::new(),
        }),
    }
}

fn test_mcp(config: &Config, id: &str, progress: &SharedProgress) -> Result<Outcome> {
    let server = config
        .mcp
        .get(id)
        .cloned()
        .ok_or_else(|| anyhow!("`{id}` is not configured"))?;
    say(progress, format!("testing `{id}`"));
    Ok(match health_check(config, id, &server)? {
        Ok(count) => Outcome {
            ok: true,
            message: format!("`{id}` connected with {count} tool(s)"),
            changes: Vec::new(),
        },
        Err(why) => Outcome {
            ok: false,
            message: format!("`{id}` failed: {why}"),
            changes: Vec::new(),
        },
    })
}

/// Connect to one server. `Ok(Ok(n))` when it lists `n > 0` tools; `Ok(Err(why))` otherwise.
fn health_check(
    config: &Config,
    name: &str,
    server: &McpServerConfig,
) -> Result<std::result::Result<usize, String>> {
    let specs = BTreeMap::from([(name.to_string(), server.clone())]);
    let load = mcp::connect_extra(&specs, &config.project_root);
    let count = load.tools.len();
    if count == 0 {
        let why = load
            .notices
            .first()
            .cloned()
            .unwrap_or_else(|| "the server listed no tools".into());
        return Ok(Err(why));
    }
    Ok(Ok(count))
}

fn smithery_install(
    registry: &str,
    config: &Config,
    name: &str,
    progress: &SharedProgress,
) -> Result<Outcome> {
    if smithery::resolve_api_key(&config.user_dir)?.is_none() {
        put_form(progress, Form::key(name, None));
        return Ok(waiting());
    }
    smithery_settings(registry, config, name, progress)
}

fn smithery_key(
    registry: &str,
    config: &Config,
    name: &str,
    key: &str,
    progress: &SharedProgress,
) -> Result<Outcome> {
    let key = key.trim();
    if let Err(error) = smithery::validate_key_at(registry, key) {
        let message = format!(
            "Smithery did not accept that key. Paste it again, or press Esc to cancel. ({error:#})"
        );
        put_form(progress, Form::key(name, Some(message)));
        return Ok(waiting());
    }
    smithery::save_key(&smithery::key_file(&config.user_dir)?, key)?;
    say(progress, "Key accepted and saved.");
    smithery_settings(registry, config, name, progress)
}

/// Open the settings form for a registry server, built from its connection schema.
fn smithery_settings(
    registry: &str,
    config: &Config,
    name: &str,
    progress: &SharedProgress,
) -> Result<Outcome> {
    let key = smithery::resolve_api_key(&config.user_dir)?;
    let Some(result) = resolve_smithery(registry, name, key.as_deref()) else {
        return Ok(failed(missing_details(name)));
    };
    put_form(progress, smithery_form(&result, name, None));
    Ok(waiting())
}

fn smithery_deploy(
    registry: &str,
    config: &Config,
    name: &str,
    values: &BTreeMap<String, String>,
    progress: &SharedProgress,
) -> Result<Outcome> {
    let key = smithery::resolve_api_key(&config.user_dir)?;
    let Some(result) = resolve_smithery(registry, name, key.as_deref()) else {
        return Ok(failed(missing_details(name)));
    };
    let server = match smithery::plan_install(&result, values) {
        Ok(server) => server,
        Err(error) => {
            put_form(
                progress,
                smithery_form(&result, name, Some(format!("{error:#}"))),
            );
            return Ok(waiting());
        }
    };
    let local = smithery::unique_name(&smithery::to_config_name(name), |taken| {
        config.mcp.contains_key(taken)
    });
    match health_check(config, &local, &server)? {
        Ok(count) => Ok(Outcome {
            ok: true,
            message: format!("Connected, {count} tools. Saved as `{local}`."),
            changes: vec![smithery::config_change(&local, &server)?],
        }),
        Err(why) => Ok(failed(format!(
            "Nothing was saved: could not connect ({why}). Check the settings, then run /extensions and install again."
        ))),
    }
}

fn smithery_discover(
    registry: &str,
    config: &Config,
    query: &str,
    progress: &SharedProgress,
) -> Result<Outcome> {
    let opts = smithery::SearchOptions {
        limit: smithery::DEFAULT_LIMIT,
        api_key: smithery::resolve_api_key(&config.user_dir)?,
        semantic: false,
    };
    let found = match smithery::search_at(registry, query, &opts) {
        Ok(found) => found,
        Err(error) => {
            return Ok(failed(format!(
                "Smithery search failed: {}. Try again in a moment.",
                error.message
            )));
        }
    };
    let count = found.len();
    let mut rows = mcp_rows(config);
    rows.extend(found.iter().map(remote_row));
    if let Ok(mut shown) = progress.lock() {
        shown.rows = Some(rows);
    }
    Ok(Outcome {
        ok: true,
        message: format!(
            "Found {count} Smithery servers for `{query}`. Press i on one to install."
        ),
        changes: Vec::new(),
    })
}

fn resolve_smithery(
    registry: &str,
    name: &str,
    key: Option<&str>,
) -> Option<smithery::SearchResult> {
    smithery::resolve_entry(
        registry,
        &serde_json::json!({ "id": name, "qualifiedName": name }),
        key,
    )
}

fn missing_details(name: &str) -> String {
    format!(
        "Smithery did not return details for `{name}`. Check the name and your connection, then try again."
    )
}

fn smithery_form(result: &smithery::SearchResult, name: &str, error: Option<String>) -> Form {
    let fields = result
        .inputs
        .iter()
        .map(|input| Field {
            key: input.key.clone(),
            label: input.key.clone(),
            hint: input.description.clone().unwrap_or_default(),
            required: input.required,
            secret: is_secret_key(&input.key),
            value: input.default.clone().unwrap_or_default(),
        })
        .collect();
    Form::config(
        name,
        &result.display_name,
        &format!(
            "Transport: {}. Enter reviews the install; Esc cancels.",
            result.transport
        ),
        fields,
        error,
    )
}

/// Setting names that hold credentials are masked while typing.
fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["key", "token", "secret", "password"]
        .iter()
        .any(|word| key.contains(word))
}

fn remote_row(found: &smithery::SearchResult) -> Row {
    let mut tags = vec!["smithery".to_owned()];
    if found.verified {
        tags.push("verified".into());
    }
    Row {
        id: found.qualified_name.clone(),
        title: found.display_name.clone(),
        summary: found.description.clone(),
        tags,
        origin: Origin::Remote,
        state: State::Available,
        details: vec![
            format!("transport: {}", found.transport),
            format!("tools: {}", found.tools.len()),
        ],
    }
}

fn put_form(progress: &SharedProgress, form: Form) {
    if let Ok(mut shown) = progress.lock() {
        shown.form = Some(form);
    }
}

/// Outcome for a stage that is waiting on the overlay's form, not an error.
fn waiting() -> Outcome {
    Outcome {
        ok: true,
        message: "Waiting for your input.".into(),
        changes: Vec::new(),
    }
}

fn failed(message: String) -> Outcome {
    Outcome {
        ok: false,
        message,
        changes: Vec::new(),
    }
}

fn install_plugin(config: &Config, id: &str, progress: &SharedProgress) -> Result<Outcome> {
    let store = PluginStore::new(plugins_root(config));
    say(
        progress,
        format!("installing {id} into {}", plugins_root(config).display()),
    );
    let plugin = Marketplaces::new(marketplaces_root(config)).install(id, &store)?;
    Ok(Outcome {
        ok: true,
        message: format!("{} installed at {}", plugin.name, plugin.path.display()),
        changes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_ids_are_unique_and_run_through_a_runtime() {
        let mut ids: Vec<&str> = CURATED.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total);
        for item in CURATED {
            assert!(matches!(item.runtime, "npx" | "uvx"), "{}", item.id);
        }
    }

    #[test]
    fn curated_server_caches_inside_the_user_dir() {
        let config = Config {
            user_dir: PathBuf::from("/tmp/home/.dial"),
            project_root: PathBuf::from("/work/project"),
            ..Default::default()
        };
        let (name, server) = curated_server(&config, "filesystem").unwrap();
        assert_eq!(name, "filesystem");
        assert_eq!(server.program, PathBuf::from("npx"));
        assert_eq!(
            server.args.last().map(String::as_str),
            Some("/work/project")
        );
        assert_eq!(
            server.env.get("npm_config_cache").map(String::as_str),
            Some("/tmp/home/.dial/mcp/npm")
        );
        assert!(server.enabled);
    }

    #[test]
    fn plan_blocks_an_already_configured_server() {
        let mut config = Config {
            user_dir: PathBuf::from("/tmp/x"),
            ..Default::default()
        };
        config
            .mcp
            .insert("memory".into(), McpServerConfig::default());
        let plan = plan_curated(&config, "memory").unwrap();
        assert_eq!(
            plan.blocked.as_deref(),
            Some("`memory` is already configured")
        );
    }

    #[test]
    fn plan_names_the_exact_command_and_location() {
        let config = Config {
            user_dir: PathBuf::from("/tmp/x"),
            project_root: PathBuf::from("/work/project"),
            ..Default::default()
        };
        let plan = plan_curated(&config, "filesystem").unwrap();
        assert_eq!(
            plan.command,
            "npx -y @modelcontextprotocol/server-filesystem /work/project"
        );
        assert_eq!(plan.location, PathBuf::from("/tmp/x/mcp"));
    }

    #[test]
    fn unknown_curated_id_is_an_error() {
        assert!(plan_curated(&Config::default(), "nope").is_err());
    }

    #[test]
    fn snapshot_offers_uninstalled_curated_servers_only() {
        let mut config = Config {
            user_dir: PathBuf::from("/tmp/x"),
            ..Default::default()
        };
        config.mcp.insert("time".into(), McpServerConfig::default());
        let rows = mcp_rows(&config);
        assert!(
            rows.iter()
                .any(|r| r.id == "time" && r.origin == Origin::Installed)
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.id == "time" && r.origin == Origin::Curated)
        );
        assert!(
            rows.iter()
                .any(|r| r.id == "memory" && r.origin == Origin::Curated)
        );
    }
}

#[cfg(test)]
mod smithery_flow_tests {
    use super::*;
    use crate::ui::overlay::extensions::model::FormKind;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// `(request head, path and query)` to `(status, body)`.
    type Handler = Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

    /// Loopback HTTP fixture: each connection gets one response from `handler`.
    fn serve(handler: Handler) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, body) = handler(&head, &target);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    /// A registry with one server, `@acme/notes`, whose connection is `connection`.
    /// A bearer key of `bad-key` is rejected, like Smithery does for a bad key.
    fn registry(connection: Value) -> String {
        let details = json!({
            "qualifiedName": "@acme/notes",
            "displayName": "Notes",
            "description": "Notes over MCP",
            "connections": [connection],
        })
        .to_string();
        let handler: Handler = Arc::new(move |head: &str, target: &str| {
            if head.contains("Bearer bad-key") {
                (401, "{}".to_owned())
            } else if target.starts_with("/servers?") {
                let servers = json!({"servers": [{
                    "id": "acme/notes",
                    "qualifiedName": "@acme/notes",
                    "displayName": "Notes",
                    "description": "Notes over MCP",
                    "verified": true,
                    "isDeployed": true,
                    "useCount": 5,
                }]});
                (200, servers.to_string())
            } else if target == "/servers/acme/notes" {
                (200, details.clone())
            } else {
                (404, "{}".to_owned())
            }
        });
        serve(handler)
    }

    /// A direct HTTP connection to the loopback MCP endpoint below.
    fn fake_connection() -> Value {
        json!({
            "type": "http",
            "deploymentUrl": mcp_endpoint(),
            "configSchema": {
                "properties": {
                    "apiKey": {"type": "string", "format": "password"},
                    "root": {"type": "string", "default": "/tmp"},
                },
                "required": ["apiKey"],
            },
        })
    }

    /// Streamable-HTTP MCP fixture: JSON replies to requests, 202 to notifications.
    fn mcp_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer_mcp(stream);
            }
        });
        format!("http://{addr}/mcp")
    }

    fn answer_mcp(mut stream: std::net::TcpStream) {
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let result = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            }),
            Some("tools/list") => json!({"tools": [
                {"name": "search", "description": "Search notes", "inputSchema": {"type": "object"}},
                {"name": "read", "description": "Read a note", "inputSchema": {"type": "object"}},
            ]}),
            _ => {
                let _ = stream.write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        };
        let text = json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        );
    }

    fn stage(
        base: &str,
        config: &Config,
        progress: &SharedProgress,
        intent: Intent,
    ) -> (Outcome, Option<Form>) {
        let layout =
            Layout::with_user_dir(&config.user_dir, config.user_dir.clone()).expect("layout");
        let outcome = run_at(base, &layout, config, &intent, progress).expect("stage runs");
        let form = progress.lock().expect("progress").form.take();
        (outcome, form)
    }

    #[test]
    fn smithery_install_asks_for_key_then_shows_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        };
        let base = registry(fake_connection());
        let progress = SharedProgress::default();
        let name = "@acme/notes";
        let go = |intent| stage(&base, &config, &progress, intent);

        // No key yet: the overlay asks for one.
        let (out, form) = go(Intent::SmitheryInstall { name: name.into() });
        assert!(out.ok);
        assert_eq!(
            form.expect("key form").kind,
            FormKind::Key { name: name.into() }
        );

        // A rejected key stays on the key form with a plain message.
        let (out, form) = go(Intent::SmitheryKey {
            name: name.into(),
            key: "bad-key".into(),
        });
        assert!(out.ok);
        let error = form.expect("key form again").error.expect("error");
        assert!(error.contains("did not accept that key"), "{error}");

        // A good key is saved, then the settings form opens from the registry's schema.
        let (_, form) = go(Intent::SmitheryKey {
            name: name.into(),
            key: "good-key".into(),
        });
        let form = form.expect("settings form");
        assert!(matches!(form.kind, FormKind::Config { .. }));
        let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["apiKey", "root"]);
        assert!(form.fields[0].required && form.fields[0].secret);
        assert_eq!(form.fields[1].value, "/tmp");
        assert!(smithery::key_file(dir.path()).expect("key file").exists());

        // A missing required setting reopens the form with the reason.
        let (out, form) = go(Intent::SmitheryDeploy {
            name: name.into(),
            values: BTreeMap::new(),
        });
        assert!(out.ok);
        let error = form.expect("form again").error.expect("error");
        assert!(error.contains("apiKey"), "{error}");

        // Discover lists the registry's servers as remote rows.
        let (out, _) = go(Intent::SmitheryDiscover {
            query: "notes".into(),
        });
        assert!(out.ok, "{}", out.message);
        let rows = progress
            .lock()
            .expect("progress")
            .rows
            .take()
            .expect("rows");
        let found = rows
            .iter()
            .find(|row| row.id == "@acme/notes")
            .expect("remote row");
        assert_eq!(found.origin, Origin::Remote);
    }

    #[test]
    fn smithery_deploy_connects_a_direct_server_and_reports_its_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        };
        smithery::save_key(
            &smithery::key_file(dir.path()).expect("key file"),
            "good-key",
        )
        .expect("save key");
        // No settings: the direct endpoint connects straight away.
        let base = registry(json!({
            "type": "http",
            "deploymentUrl": mcp_endpoint(),
            "configSchema": {"properties": {}, "required": []},
        }));
        let progress = SharedProgress::default();
        let (out, form) = stage(
            &base,
            &config,
            &progress,
            Intent::SmitheryDeploy {
                name: "@acme/notes".into(),
                values: BTreeMap::new(),
            },
        );
        assert!(form.is_none());
        assert!(out.ok, "{}", out.message);
        assert!(
            out.message.starts_with("Connected, 2 tools"),
            "{}",
            out.message
        );
        assert!(
            out.message.contains("Saved as `acme-notes`"),
            "{}",
            out.message
        );
        assert!(
            matches!(&out.changes[..], [Change::Set { path, .. }] if path.as_str() == "mcp.acme-notes")
        );
    }

    #[test]
    fn smithery_deploy_reports_a_failed_connection_and_saves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        };
        smithery::save_key(
            &smithery::key_file(dir.path()).expect("key file"),
            "good-key",
        )
        .expect("save key");
        let base = registry(json!({
            "type": "http",
            "deploymentUrl": "http://127.0.0.1:9/mcp",
            "configSchema": {"properties": {}, "required": []},
        }));
        let progress = SharedProgress::default();
        let (out, form) = stage(
            &base,
            &config,
            &progress,
            Intent::SmitheryDeploy {
                name: "@acme/notes".into(),
                values: BTreeMap::new(),
            },
        );
        assert!(form.is_none());
        assert!(!out.ok);
        assert!(
            out.message.starts_with("Nothing was saved"),
            "{}",
            out.message
        );
        assert!(out.changes.is_empty());
    }
}
