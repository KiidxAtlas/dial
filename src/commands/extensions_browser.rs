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
        skills::packages::{self, registry::Entry as SkillEntry},
    },
    tools::{
        mcp::{
            catalog::{self, CURATED, Candidate, Endpoints, Install, Source},
            smithery,
        },
        plugin::{
            installer::PluginStore,
            marketplace::{Marketplaces, SkillListing},
        },
    },
    ui::overlay::extensions::model::{
        CatalogUpdate, Field, Form, Intent, JobProgress, Origin, Row, SharedCatalog,
        SharedProgress, State, Tab,
    },
};
use anyhow::{Result, anyhow};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::AtomicBool,
        mpsc::{Receiver, TryRecvError, channel},
    },
    thread,
};

/// Rows for the three tabs, built from disk and config.
#[derive(Default)]
pub(crate) struct Snapshot {
    pub mcp: Vec<Row>,
    pub plugins: Vec<Row>,
    pub skills: Vec<Row>,
    pub marketplaces: Vec<Row>,
}

pub(crate) fn snapshot(layout: &Layout, config: &Config) -> Snapshot {
    Snapshot {
        mcp: mcp_rows(config),
        plugins: plugin_rows(config),
        skills: skill_rows(layout, config),
        marketplaces: marketplace_rows(config),
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
    panel.set_snapshot(rows.mcp, rows.plugins, rows.skills, rows.marketplaces);
    // Listings are fetched in the background; the rows above already show what is cached.
    for tab in [Tab::Mcp, Tab::Plugins, Tab::Skills] {
        let listing = SharedCatalog::default();
        let config = config.clone();
        let layout = layout.clone();
        let shared = listing.clone();
        thread::spawn(move || {
            let update = catalog_update(&Endpoints::live(), &layout, &config, tab, "", 1);
            if let Ok(mut slot) = shared.lock() {
                *slot = Some(update);
            }
        });
        panel.attach_catalog(tab, listing);
    }
}

fn skill_rows(layout: &Layout, config: &Config) -> Vec<Row> {
    let installed = packages::list_packages(layout).unwrap_or_default();
    let mut taken = installed_skill_names(&installed);
    let mut rows: Vec<Row> = installed
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
        .collect();
    let found = match Marketplaces::new(marketplaces_root(config)).skills(None) {
        Ok(listed) => listed.into_iter().map(listing_entry).collect(),
        Err(_) => Vec::new(),
    };
    rows.extend(available_skills(found, &mut taken));
    rows
}

/// Installed skill package names and the skill names they provide. Available skills with these
/// names are hidden, so an installed skill is never offered again.
fn installed_skill_names(installed: &[packages::Package]) -> BTreeSet<String> {
    installed
        .iter()
        .flat_map(|package| {
            std::iter::once(package.name.clone()).chain(package.skills.iter().cloned())
        })
        .collect()
}

fn listing_entry(skill: SkillListing) -> (String, SkillEntry) {
    (
        skill.marketplace,
        SkillEntry {
            name: skill.name,
            description: skill.description,
            url: skill.url,
        },
    )
}

/// Rows for skills that are not installed. `taken` starts with the installed names and grows, so
/// a skill listed by several sources shows once.
fn available_skills(found: Vec<(String, SkillEntry)>, taken: &mut BTreeSet<String>) -> Vec<Row> {
    found
        .into_iter()
        .filter(|(_, entry)| taken.insert(entry.name.clone()))
        .map(|(marketplace, entry)| skill_row(&marketplace, &entry))
        .collect()
}

fn skill_row(marketplace: &str, entry: &SkillEntry) -> Row {
    Row {
        id: entry.url.clone(),
        title: entry.name.clone(),
        summary: entry.description.clone(),
        tags: vec![marketplace.to_owned(), "skill".into()],
        origin: Origin::Remote,
        state: State::Available,
        details: vec![
            format!("source: {}", entry.url),
            format!("listed by: {marketplace}"),
            "installs as a user skill package; no install scripts run".into(),
        ],
    }
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
        let needs = item.env.iter().any(|(_, _, required)| *required);
        let mut details = vec![
            format!("runs: {} {}", item.runtime, item.args.join(" ")),
            format!("needs: {} on PATH", item.runtime),
        ];
        details.extend(item.env.iter().map(|(key, description, required)| {
            format!(
                "{} {key}: {description}",
                if *required { "needs" } else { "optional" }
            )
        }));
        details.push(format!(
            "installs to: {}",
            mcp_cache_dir(&config.user_dir).display()
        ));
        details.push(format!("tools: {}", item.tools));
        rows.push(Row {
            id: item.id.into(),
            title: item.title.into(),
            summary: item.summary.into(),
            tags: vec![
                "curated".into(),
                if needs { "needs values" } else { "no key" }.into(),
            ],
            origin: Origin::Curated,
            state: State::Available,
            details,
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

/// Rows for the Marketplaces tab: one per registered marketplace, with what it offers.
fn marketplace_rows(config: &Config) -> Vec<Row> {
    let markets = Marketplaces::new(marketplaces_root(config));
    let Ok(infos) = markets.list() else {
        return Vec::new();
    };
    infos
        .into_iter()
        .map(|info| {
            let plugins = markets
                .entries(Some(info.name.as_str()))
                .map_or(0, |e| e.len());
            let skills = markets
                .skills(Some(info.name.as_str()))
                .map_or(0, |s| s.len());
            let refreshed = info
                .refreshed_at
                .map_or_else(|| "never".to_owned(), |time| time.to_string());
            Row {
                id: info.name.clone(),
                title: info.name.clone(),
                summary: format!("{plugins} plugins · {skills} skills"),
                tags: if info.builtin {
                    vec!["built-in".into()]
                } else {
                    Vec::new()
                },
                origin: Origin::Installed,
                state: if info.enabled {
                    State::Enabled
                } else {
                    State::Disabled
                },
                details: vec![
                    format!(
                        "index: {}",
                        if info.builtin {
                            "built into Dial".to_owned()
                        } else {
                            info.index.clone()
                        }
                    ),
                    format!("refreshed: {refreshed}"),
                ],
            }
        })
        .collect()
}

fn mcp_cache_dir(user_dir: &Path) -> PathBuf {
    user_dir.join("mcp")
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
    run_at(&Endpoints::live(), layout, config, intent, progress)
}

/// [`run`] against the given registries, so tests can point them at a local fake.
fn run_at(
    endpoints: &Endpoints,
    layout: &Layout,
    config: &Config,
    intent: &Intent,
    progress: &SharedProgress,
) -> Result<Outcome> {
    match intent {
        Intent::InstallCuratedMcp { id } => {
            install_mcp(endpoints, config, id, &BTreeMap::new(), progress)
        }
        Intent::InstallMcp { id, values } => install_mcp(endpoints, config, id, values, progress),
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
        Intent::SmitheryInstall { name } => {
            smithery_install(&endpoints.smithery, config, name, progress)
        }
        Intent::SmitheryKey { name, key } => {
            smithery_key(&endpoints.smithery, config, name, key, progress)
        }
        Intent::SmitheryDeploy { name, values } => {
            smithery_deploy(&endpoints.smithery, config, name, values, progress)
        }
        Intent::CatalogBrowse { tab, query, page } => {
            let update = catalog_update(endpoints, layout, config, *tab, query, *page);
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
            Ok(Outcome {
                ok: true,
                message: format!("Added marketplace {name}"),
                changes: Vec::new(),
            })
        }
        Intent::RemoveMarketplace { name } => {
            Marketplaces::new(marketplaces_root(config)).remove(name)?;
            Ok(Outcome {
                ok: true,
                message: format!(
                    "Removed marketplace {name}. Plugins and skills it installed stay installed."
                ),
                changes: Vec::new(),
            })
        }
        Intent::ToggleMarketplace { name, enable } => {
            Marketplaces::new(marketplaces_root(config)).set_enabled(name, *enable)?;
            Ok(Outcome {
                ok: true,
                message: format!(
                    "{} marketplace {name}",
                    if *enable { "Enabled" } else { "Disabled" }
                ),
                changes: Vec::new(),
            })
        }
        Intent::InstallSkill { source } => install_skill(layout, source, progress),
    }
}

/// Provider IO belongs on the command worker, never in the overlay. Errors retain cached rows.
fn catalog_update(
    endpoints: &Endpoints,
    layout: &Layout,
    config: &Config,
    tab: Tab,
    query: &str,
    page: usize,
) -> CatalogUpdate {
    let page = page.max(1);
    let result = (|| -> Result<(Vec<Row>, bool, String)> {
        match tab {
            Tab::Mcp => {
                let found = catalog::discover(endpoints, config, query, page);
                let rows: Vec<_> = found.candidates.iter().map(candidate_row).collect();
                let mut message = format!(
                    "MCP page {page}: {} servers from the official registry, Smithery, npm, GitHub and curated · {}",
                    rows.len(),
                    if found.more {
                        "n loads more"
                    } else {
                        "end of results"
                    }
                );
                if !found.notices.is_empty() {
                    message.push_str(&format!(" · unavailable: {}", found.notices.join("; ")));
                }
                Ok((rows, found.more, message))
            }
            Tab::Plugins => {
                let markets = Marketplaces::new(marketplaces_root(config));
                markets.refresh(None)?;
                // Validate the cached listing here so a corrupt index is not silently shown as empty.
                markets.entries(None)?;
                let rows: Vec<_> = plugin_rows(config)
                    .into_iter()
                    .filter(|row| row.origin == Origin::Remote)
                    .collect();
                let message = if rows.is_empty() {
                    "No plugins listed by enabled marketplaces. The Marketplaces tab adds or enables one.".to_owned()
                } else {
                    format!(
                        "{} marketplace plugins · / filters by name, description or marketplace",
                        rows.len()
                    )
                };
                Ok((rows, false, message))
            }
            Tab::Skills => {
                let installed = packages::list_packages(layout).unwrap_or_default();
                let mut taken = installed_skill_names(&installed);
                let mut found = Vec::new();
                let mut notices = Vec::new();
                match Marketplaces::new(marketplaces_root(config)).skills(None) {
                    Ok(listed) => found.extend(listed.into_iter().map(listing_entry)),
                    Err(error) => notices.push(format!("marketplaces: {error:#}")),
                }
                for (name, url) in &config.skills.registries {
                    match packages::registry::fetch_index(url) {
                        Ok(entries) => {
                            found.extend(entries.into_iter().map(|entry| (name.clone(), entry)))
                        }
                        Err(error) => notices.push(format!("{name}: {error:#}")),
                    }
                }
                let rows = available_skills(found, &mut taken);
                let mut message = format!(
                    "{} skills from marketplaces and skill registries · installed skills are hidden",
                    rows.len()
                );
                if !notices.is_empty() {
                    message.push_str(&format!(" · unavailable: {}", notices.join("; ")));
                }
                Ok((rows, false, message))
            }
            Tab::Marketplaces => {
                let markets = Marketplaces::new(marketplaces_root(config));
                markets.refresh(None)?;
                let count = markets.list()?.len();
                Ok((
                    Vec::new(),
                    false,
                    format!(
                        "{count} marketplaces · a adds an index · e enables or disables · x removes"
                    ),
                ))
            }
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

/// Installs a curated, official-registry, npm or GitHub server. Missing required values reopen the
/// form first; a server that starts and lists tools is saved, anything else saves nothing.
fn install_mcp(
    endpoints: &Endpoints,
    config: &Config,
    id: &str,
    values: &BTreeMap<String, String>,
    progress: &SharedProgress,
) -> Result<Outcome> {
    let candidate = match catalog::resolve(endpoints, config, id) {
        Ok(candidate) => candidate,
        Err(error) => return Ok(failed(format!("{error:#}"))),
    };
    let local = catalog::config_name(&candidate.name);
    if config.mcp.contains_key(&local) {
        return Ok(failed(format!("`{local}` is already configured")));
    }
    if candidate.missing_required(values).is_some() {
        put_form(progress, mcp_form(&candidate, None));
        return Ok(waiting());
    }
    let server = match candidate.server(values) {
        Ok(server) => server,
        Err(error) => return Ok(failed(format!("{error:#}"))),
    };
    if server.url.is_none() && !catalog::on_path(&server.program.to_string_lossy()) {
        return Ok(failed(format!(
            "{} is not on PATH. Install it, then try again.",
            server.program.display()
        )));
    }
    say(
        progress,
        format!("running: {}", candidate.install_summary()),
    );
    say(
        progress,
        format!("caches under {}", mcp_cache_dir(&config.user_dir).display()),
    );
    say(
        progress,
        "starting the server to check it (first run downloads packages)",
    );
    match catalog::check(&config.project_root, &local, &server) {
        Ok(count) => {
            say(
                progress,
                format!("health check passed with {count} tool(s)"),
            );
            Ok(Outcome {
                ok: true,
                message: format!("`{local}` connected with {count} tool(s)"),
                changes: vec![smithery::config_change(&local, &server)?],
            })
        }
        Err(why) => Ok(failed(format!("health check failed, nothing saved: {why}"))),
    }
}

/// The form that asks for a candidate's values. Secret values are masked as they are typed.
fn mcp_form(candidate: &Candidate, error: Option<String>) -> Form {
    let fields = candidate
        .inputs
        .iter()
        .map(|input| Field {
            key: input.key.clone(),
            label: input.key.clone(),
            hint: input.description.clone(),
            required: input.required,
            secret: input.secret,
            value: String::new(),
        })
        .collect();
    Form::mcp_inputs(
        &candidate.name,
        &format!("Set up {}", candidate.title),
        "Values are saved in this server's config entry. Enter reviews the install; Esc cancels.",
        fields,
        error,
    )
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
    Ok(catalog::check(&config.project_root, name, server))
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
    catalog::looks_secret(key)
}

/// A catalog row for one merged candidate. Smithery-first rows open their settings form; every
/// other remote row installs from `i` through [`catalog::resolve`], and curated rows by id.
fn candidate_row(candidate: &Candidate) -> Row {
    let mut tags: Vec<String> = candidate
        .sources
        .iter()
        .map(|source| source.label().to_owned())
        .collect();
    tags.extend(candidate.badges.iter().cloned());
    let state = if candidate.origin() == Source::Smithery {
        Some("settings on install")
    } else if matches!(candidate.install, Install::Deferred(_)) {
        Some("checked on install")
    } else if !candidate.installable() {
        Some("manual setup")
    } else if candidate.inputs.iter().any(|input| input.required) {
        Some("needs values")
    } else if matches!(candidate.origin(), Source::Npm | Source::Github) {
        // These listings do not declare settings, so "no key" would be a guess.
        None
    } else {
        Some("no key")
    };
    tags.extend(state.map(str::to_owned));
    let origin = if candidate.origin() == Source::Curated {
        Origin::Curated
    } else {
        Origin::Remote
    };
    let install = if candidate.origin() == Source::Smithery {
        "Smithery settings form (press i)".to_owned()
    } else {
        candidate.install_summary()
    };
    let mut details = vec![format!("install: {install}")];
    details.extend(candidate.inputs.iter().map(|input| {
        format!(
            "{} {}: {}",
            if input.required { "needs" } else { "optional" },
            input.key,
            input.description
        )
    }));
    if let Some(repository) = &candidate.repository {
        details.push(format!("repository: {repository}"));
    }
    details.push(format!(
        "listed by: {}",
        candidate
            .sources
            .iter()
            .map(|source| source.label())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Row {
        id: candidate.name.clone(),
        title: candidate.title.clone(),
        summary: candidate.description.clone(),
        tags,
        origin,
        state: State::Available,
        details,
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

/// Installs a skill package from its `repo[@ref][#path]` listing into the user scope. The
/// skill-package code checks the path and refuses symlinks; no install scripts run.
fn install_skill(layout: &Layout, source: &str, progress: &SharedProgress) -> Result<Outcome> {
    let cancel = AtomicBool::new(false);
    let message = packages::install(
        layout,
        &BTreeMap::new(),
        crate::storage::skills::Scope::User,
        source,
        &cancel,
        &mut |line: &str| say(progress, line),
    )?;
    Ok(Outcome {
        ok: true,
        message,
        changes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::mcp::catalog::Install;

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

    fn listed(name: &str, source: Source) -> Candidate {
        Candidate {
            name: name.into(),
            title: name.into(),
            description: "a listed server".into(),
            sources: vec![source],
            badges: Vec::new(),
            install: Install::Blocked("test".into()),
            inputs: Vec::new(),
            repository: None,
        }
    }

    #[test]
    fn a_server_listed_twice_is_one_row_tagged_with_both_listings() {
        let found = catalog::merge(vec![
            listed("io.github.acme/notes", Source::Official),
            listed("@acme/notes", Source::Smithery),
        ]);
        let rows: Vec<Row> = found.iter().map(candidate_row).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "io.github.acme/notes");
        assert!(rows[0].tags.iter().any(|t| t == "official"));
        assert!(rows[0].tags.iter().any(|t| t == "smithery"));
        assert_eq!(rows[0].origin, Origin::Remote);
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
        let endpoints = Endpoints::at(base);
        let outcome = run_at(&endpoints, &layout, config, &intent, progress).expect("stage runs");
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

    #[test]
    fn mcp_catalog_search_keeps_nuxt_when_smithery_details_are_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        };
        let layout = Layout::with_user_dir(dir.path(), dir.path().into()).expect("layout");
        let official = serve(Arc::new(|_, _| (503, "{}".into())));
        let smithery = serve(Arc::new(|_, target| {
            if target.starts_with("/servers?") {
                (
                    200,
                    json!({
                        "servers": [{"qualifiedName":"@nuxt/mcp", "displayName":"Nuxt"}],
                        "pagination": {"totalPages":1}
                    })
                    .to_string(),
                )
            } else {
                (404, "{}".into())
            }
        }));
        let update = catalog_update(
            &Endpoints {
                official,
                smithery,
                ..Endpoints::at("http://127.0.0.1:9")
            },
            &layout,
            &config,
            Tab::Mcp,
            "nuxt",
            1,
        );
        assert!(update.error.is_none());
        assert!(update.rows.iter().any(|row| row.id == "nuxt"));
        let remote = update
            .rows
            .iter()
            .find(|row| row.id == "@nuxt/mcp")
            .unwrap();
        assert!(remote.tags.iter().any(|tag| tag == "settings on install"));
        assert!(update.message.contains("unavailable: official registry"));
        assert!(!update.more);
    }

    #[test]
    fn curated_servers_that_need_values_open_a_form_before_anything_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        };
        let endpoints = Endpoints::at("http://127.0.0.1:9");
        let progress = SharedProgress::default();
        let out =
            install_mcp(&endpoints, &config, "github", &BTreeMap::new(), &progress).expect("runs");
        assert!(out.ok && out.changes.is_empty());
        let form = progress
            .lock()
            .expect("progress")
            .form
            .take()
            .expect("values form");
        assert_eq!(
            form.kind,
            FormKind::McpInputs {
                id: "github".into()
            }
        );
        assert!(form.fields[0].required && form.fields[0].secret);

        // An id that no listing knows fails with the reason and saves nothing.
        let missing = install_mcp(
            &endpoints,
            &config,
            "io.example/none",
            &BTreeMap::new(),
            &progress,
        )
        .expect("runs");
        assert!(!missing.ok && missing.changes.is_empty());
    }
}

#[cfg(test)]
mod marketplace_tests {
    use super::*;
    use crate::tools::plugin::marketplace::DEFAULT_NAME;

    fn config_in(dir: &tempfile::TempDir) -> Config {
        Config {
            user_dir: dir.path().into(),
            project_root: dir.path().into(),
            ..Default::default()
        }
    }

    fn layout_for(config: &Config) -> Layout {
        Layout::with_user_dir(&config.user_dir, config.user_dir.clone()).expect("layout")
    }

    #[test]
    fn default_config_offers_curated_mcp_and_default_marketplace_skills_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_in(&dir);
        let rows = snapshot(&layout_for(&config), &config);
        assert!(rows.mcp.iter().any(|row| row.origin == Origin::Curated));
        assert!(
            rows.marketplaces
                .iter()
                .any(|row| row.id == DEFAULT_NAME && row.state == State::Enabled)
        );
        assert!(
            rows.skills
                .iter()
                .any(|row| row.title == "pdf" && row.origin == Origin::Remote)
        );
        assert!(rows.plugins.is_empty());
    }

    #[test]
    fn installed_skills_hide_the_same_skill_from_the_available_list() {
        let listed = |name: &str, marketplace: &str| {
            (
                marketplace.to_owned(),
                SkillEntry {
                    name: name.into(),
                    description: String::new(),
                    url: format!("https://example.com/{marketplace}.git#{name}"),
                },
            )
        };
        let mut taken = BTreeSet::from(["pdf".to_owned()]);
        let rows = available_skills(
            vec![listed("pdf", "a"), listed("docx", "a"), listed("docx", "b")],
            &mut taken,
        );
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, vec!["docx"]);
        assert!(rows[0].tags.iter().any(|tag| tag == "a"));
    }

    #[test]
    fn skill_install_rejects_path_traversal_before_fetching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_in(&dir);
        let progress = SharedProgress::default();
        let result = install_skill(
            &layout_for(&config),
            "https://example.com/org/skills.git#../outside",
            &progress,
        );
        assert!(result.is_err());
        assert!(!dir.path().join("skill-packages").exists());
    }

    #[test]
    fn adding_removing_and_disabling_marketplaces_persist_through_the_intents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_in(&dir);
        let layout = layout_for(&config);
        let index = dir.path().join("extra.json");
        std::fs::write(
            &index,
            r#"{"skills":[{"name":"extra-skill","url":"https://example.com/org/skills.git#extra-skill"}]}"#,
        )
        .expect("write index");
        let endpoints = Endpoints::at("http://127.0.0.1:9");
        let progress = SharedProgress::default();
        let names = |config: &Config| -> Vec<String> {
            Marketplaces::new(marketplaces_root(config))
                .list()
                .expect("list")
                .into_iter()
                .map(|market| market.name)
                .collect()
        };
        let go = |intent: Intent| {
            run_at(&endpoints, &layout, &config, &intent, &progress).expect("runs")
        };

        let initial = names(&config);
        assert!(
            go(Intent::AddMarketplace {
                name: "extra".into(),
                index: index.display().to_string(),
            })
            .ok
        );
        let mut expected = initial.clone();
        expected.push("extra".to_string());
        expected.sort();
        assert_eq!(names(&config), expected);

        assert!(
            go(Intent::RemoveMarketplace {
                name: "extra".into()
            })
            .ok
        );
        assert_eq!(names(&config), initial);

        assert!(
            go(Intent::ToggleMarketplace {
                name: DEFAULT_NAME.into(),
                enable: false,
            })
            .ok
        );
        let info = Marketplaces::new(marketplaces_root(&config))
            .list()
            .expect("list");
        assert!(
            !info
                .iter()
                .find(|market| market.name == DEFAULT_NAME)
                .unwrap()
                .enabled
        );
    }
}
