//! Data and install work behind `/extensions`. The overlay in `ui::overlay::extensions` only shows
//! rows and emits intents; this module builds the rows, plans installs and runs them on a worker
//! thread. Config changes come back as [`Change`]s and are saved on the main thread by [`poll`],
//! and only after the install and its health check succeed, or when the check shows the server
//! only lacks an OAuth sign-in (the entry is saved so `/mcp login <name>` can complete it).
use crate::agent::Agent;
use crate::app::{App, Shell};
use crate::config::Config;
use crate::config::paths::Layout;
use crate::config::settings_store::{Change, Scope};
use crate::config::types::McpServerConfig;
use crate::storage::skills::packages;
use crate::storage::skills::packages::registry::Entry as SkillEntry;
use crate::tools::integrations::mcp::catalog::{
    self, CURATED, Candidate, CheckFailure, Endpoints, Install, Source,
};
use crate::tools::integrations::mcp::{foreign, smithery};
use crate::tools::integrations::plugin::installer::PluginStore;
use crate::tools::integrations::plugin::marketplace::{Marketplaces, SkillListing};
use crate::ui::overlay::extensions::ExtensionsOverlay;
use crate::ui::overlay::extensions::model::{
    CatalogUpdate, Field, Form, Intent, JobProgress, Origin, Row, SharedCatalog, SharedProgress,
    State, Tab,
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
        mcp: mcp_rows(config, foreign::home_dir().as_deref()),
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
        fetch_catalog(panel, layout, config, tab, String::new());
    }
}

/// Fetch page 1 of `tab`'s catalog for `query` on a background thread; the overlay applies it on
/// a later tick, replacing any fetch still pending for that tab.
pub(crate) fn fetch_catalog(
    panel: &mut ExtensionsOverlay,
    layout: &Layout,
    config: &Config,
    tab: Tab,
    query: String,
) {
    let listing = SharedCatalog::default();
    let config = config.clone();
    let layout = layout.clone();
    let shared = listing.clone();
    let search = query.clone();
    thread::spawn(move || {
        let update = catalog_update(&Endpoints::live(), &layout, &config, tab, &search, 1);
        if let Ok(mut slot) = shared.lock() {
            *slot = Some(update);
        }
    });
    panel.attach_catalog(tab, query, listing);
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

fn mcp_rows(config: &Config, home: Option<&Path>) -> Vec<Row> {
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
        let candidate = catalog::curated_candidate(item, &config.user_dir, &config.project_root);
        let (requirements, credential_details) = credential_metadata(&candidate);
        let mut details = vec![
            format!("runs: {} {}", item.runtime, item.args.join(" ")),
            format!("needs: {} on PATH", item.runtime),
        ];
        if item.project == catalog::ProjectRoot::WorkingDir {
            details.push(format!("runs in: {}", config.project_root.display()));
        }
        details.extend(credential_details);
        details.push(format!(
            "installs to: {}",
            mcp_cache_dir(&config.user_dir).display()
        ));
        details.push(format!("tools: {}", item.tools));
        rows.push(Row {
            id: item.id.into(),
            title: item.title.into(),
            summary: item.summary.into(),
            tags: std::iter::once("curated".to_owned())
                .chain(requirements)
                .collect(),
            origin: Origin::Curated,
            state: State::Available,
            details,
        });
    }
    let configured: BTreeSet<&str> = config.mcp.keys().map(String::as_str).collect();
    for entry in foreign::not_configured(foreign::discover(home, &config.project_root), &configured)
    {
        let mut tags = vec!["found".into(), entry.provider.to_lowercase()];
        tags.extend(entry.also_in.iter().map(|p| p.to_lowercase()));
        let mut details = vec![
            format!(
                "configured for {} in {}",
                entry.provider,
                entry.source.display()
            ),
            format!("runs: {}", server_command(&entry.server)),
            "Enter copies it into Dial's config; Dial never reads that file at startup.".into(),
        ];
        if !entry.also_in.is_empty() {
            details.push(format!("also defined by: {}", entry.also_in.join(", ")));
        }
        if !entry.server.env.is_empty() {
            let keys: Vec<&str> = entry.server.env.keys().map(String::as_str).collect();
            details.push(format!("environment: {}", keys.join(", ")));
        }
        rows.push(Row {
            id: entry.name.clone(),
            title: entry.name,
            summary: format!("{} · {}", entry.provider, server_summary(&entry.server)),
            tags,
            origin: Origin::Foreign,
            state: State::Available,
            details,
        });
    }
    rows
}

// Display-only summaries deliberately omit arbitrary arguments and URL components: even a path
// segment or unnamed positional argument can be a credential. Execution uses the original config.
fn server_summary(server: &McpServerConfig) -> String {
    if server.url.is_some() {
        "remote MCP endpoint (URL hidden)".into()
    } else {
        server
            .program
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

pub(super) fn server_command(server: &McpServerConfig) -> String {
    let summary = server_summary(server);
    if server.url.is_none() && !server.args.is_empty() {
        format!("{summary} ({} arguments hidden)", server.args.len())
    } else {
        summary
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
    intent: Intent,
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
            &intent,
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

fn fail(progress: &SharedProgress, intent: &Intent, message: String) {
    if let Ok(mut shown) = progress.lock() {
        if let Intent::CatalogBrowse { tab, query, page } = intent {
            shown.catalog = Some(CatalogUpdate {
                tab: *tab,
                query: query.clone(),
                page: *page,
                rows: Vec::new(),
                more: false,
                message: String::new(),
                error: Some(message.clone()),
            });
        }
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
            &intent,
            "another extension change is still running".into(),
        );
        return;
    }
    let config = match app.reload() {
        Ok(config) => config,
        Err(error) => {
            return fail(
                &progress,
                &intent,
                format!("could not read settings: {error:#}"),
            );
        }
    };
    let scope = match intent_scope(&app.layout, &intent, scope) {
        Ok(scope) => scope,
        Err(error) => {
            return fail(
                &progress,
                &intent,
                format!("could not read settings: {error:#}"),
            );
        }
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
    let job_intent = intent.clone();
    thread::spawn(move || {
        let outcome = match run(&layout, &config, &intent, &worker) {
            Ok(outcome) => outcome,
            Err(error) => Outcome {
                ok: false,
                message: format!("{error:#}"),
                changes: Vec::new(),
            },
        };
        if !outcome.ok {
            fail(&worker, &intent, outcome.message.clone());
        }
        let _ = tx.send(outcome);
    });
    *job = Some(InstallJob {
        outcome: rx,
        progress,
        scope,
        intent: job_intent,
    });
}

/// Existing MCP entries are changed in their owning layer, not the install destination.
fn intent_scope(layout: &Layout, intent: &Intent, selected: Scope) -> Result<Scope> {
    match intent {
        Intent::ToggleMcp { id, .. } | Intent::RemoveMcp { id } => {
            super::extensions::mcp_scope(layout, id)
        }
        _ => Ok(selected),
    }
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
    if !outcome.ok {
        fail(&running.progress, &running.intent, outcome.message.clone());
    }
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
    if ok
        && !changes.is_empty()
        && let Err(error) = save(&changes, scope)
    {
        ok = false;
        message = format!("Done, but saving the config failed: {error:#}");
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
        Intent::ImportMcp { id } => {
            import_mcp(config, foreign::home_dir().as_deref(), id, progress)
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
/// form first; a server that starts and lists tools is saved, and so is a remote server that only
/// asks for an OAuth sign-in (the result directs the user to `/mcp login`). Anything else saves
/// nothing.
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
    match catalog::check(config, &local, &server) {
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
        Err(CheckFailure::SignIn(_)) => sign_in_saved(&local, &server, progress),
        Err(why) => Ok(failed(format!("health check failed, nothing saved: {why}"))),
    }
}

/// The server is reachable and answered with an OAuth challenge. `/mcp login` needs a config
/// entry, so the entry is saved and the user is directed to sign in; Dial never signs in for them.
fn sign_in_saved(
    local: &str,
    server: &McpServerConfig,
    progress: &SharedProgress,
) -> Result<Outcome> {
    say(progress, "the server asks for an OAuth sign-in");
    Ok(Outcome {
        ok: true,
        message: sign_in_message(local),
        changes: vec![smithery::config_change(local, server)?],
    })
}

fn sign_in_message(local: &str) -> String {
    format!(
        "Saved `{local}`; it needs a sign-in before it lists tools. Run `/mcp login {local}` and finish in your browser, then `/mcp reconnect` to connect it."
    )
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
        Err(CheckFailure::SignIn(why)) => Outcome {
            ok: false,
            message: format!("`{id}` needs a sign-in ({why})"),
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
) -> Result<std::result::Result<usize, CheckFailure>> {
    Ok(catalog::check(config, name, server))
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
        Err(CheckFailure::SignIn(_)) => sign_in_saved(&local, &server, progress),
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

/// Credential declarations are separate from install readiness: a runnable command is not proof
/// that a server needs no authentication. Both initial curated rows and catalog results use this
/// mapping, without inspecting configured values or rendering them.
fn credential_metadata(candidate: &Candidate) -> (Vec<String>, Vec<String>) {
    let mut tags = Vec::new();
    let mut details = Vec::new();
    let required_secret = candidate
        .inputs
        .iter()
        .any(|input| input.required && input.secret);
    let optional_secret = candidate
        .inputs
        .iter()
        .any(|input| !input.required && input.secret);
    if candidate.origin() == Source::Smithery {
        tags.push("Smithery key required".into());
        tags.push("server auth unknown".into());
        details.push("Smithery platform API key: required to install through Smithery; this is separate from the server's own credentials.".into());
        details.push("Server credentials and configuration: unknown until Smithery loads the settings on install.".into());
    } else {
        if required_secret {
            tags.push("API key/token required".into());
            details.push(
                "Credentials: required secret input declared (API key, token or other credential)."
                    .into(),
            );
        }
        if optional_secret {
            tags.push("API key/token optional".into());
            details.push("Credentials: optional secret input declared.".into());
        }
        if candidate.origin() != Source::Curated && !required_secret {
            tags.push("auth unknown".into());
            details.push("Authentication requirements: unknown beyond the declared inputs. Listing metadata may omit credentials, remote authentication or settings checked on install; no API-key-free guarantee.".into());
        } else if !required_secret && !optional_secret {
            tags.push("no key declared".into());
            details.push("Credentials: none declared by the curated entry; this is not a guarantee that upstream authentication requirements cannot change.".into());
        }
    }
    if candidate
        .inputs
        .iter()
        .any(|input| input.required && !input.secret)
    {
        tags.push("configuration required".into());
        details.push("Configuration: required non-secret input declared; this is separate from an API key or token.".into());
    }
    details.extend(candidate.inputs.iter().map(|input| {
        format!(
            "{} {} {}: {}",
            if input.required {
                "required"
            } else {
                "optional"
            },
            if input.secret {
                "credential"
            } else {
                "configuration"
            },
            input.key,
            input.description
        )
    }));
    (tags, details)
}

/// A catalog row for one merged candidate. Smithery-first rows open their settings form; every
/// other remote row installs from `i` through [`catalog::resolve`], and curated rows by id.
fn candidate_row(candidate: &Candidate) -> Row {
    let (requirements, credential_details) = credential_metadata(candidate);
    // The first source is used by install routing. Requirements come next so long summaries and
    // multiple listing badges cannot hide the credential status.
    let mut tags = vec![candidate.origin().label().to_owned()];
    tags.extend(requirements);
    tags.extend(
        candidate
            .sources
            .iter()
            .skip(1)
            .map(|source| source.label().to_owned()),
    );
    tags.extend(candidate.badges.iter().cloned());
    let state = if candidate.origin() == Source::Smithery {
        Some("settings on install")
    } else if matches!(candidate.install, Install::Deferred(_)) {
        Some("checked on install")
    } else if !candidate.installable() {
        Some("manual setup")
    } else {
        None
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
    details.extend(credential_details);
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

/// Copies a server another coding agent configured into Dial's config. The entry is re-read from
/// that agent's file (the overlay only holds its id), validated like any Dial server, and
/// health-checked before anything is saved.
fn import_mcp(
    config: &Config,
    home: Option<&Path>,
    id: &str,
    progress: &SharedProgress,
) -> Result<Outcome> {
    if config.mcp.contains_key(id) {
        return Ok(failed(format!("`{id}` is already configured")));
    }
    let Some(found) = foreign::find(home, &config.project_root, id) else {
        return Ok(failed(format!(
            "`{id}` is no longer defined by another tool; press r to refresh"
        )));
    };
    let server = found.server;
    if let Err(error) = crate::config::tools::mcp::validate_server(id, &server) {
        return Ok(failed(format!("{error:#}")));
    }
    if server.enabled {
        if server.url.is_none() && !catalog::on_path(&server.program.to_string_lossy()) {
            return Ok(failed(format!(
                "{} is not on PATH. Install it, then try again.",
                server.program.display()
            )));
        }
        say(
            progress,
            format!(
                "importing from {}: {}",
                found.provider,
                server_command(&server)
            ),
        );
        say(progress, "starting the server to check it");
        match catalog::check(config, id, &server) {
            Ok(count) => say(
                progress,
                format!("health check passed with {count} tool(s)"),
            ),
            Err(CheckFailure::SignIn(_)) => {
                say(progress, "the server asks for an OAuth sign-in");
                return Ok(Outcome {
                    ok: true,
                    message: format!(
                        "Imported `{id}` from {}. {}",
                        found.provider,
                        sign_in_message(id)
                    ),
                    changes: vec![smithery::config_change(id, &server)?],
                });
            }
            Err(why) => {
                return Ok(failed(format!("health check failed, nothing saved: {why}")));
            }
        }
    }
    Ok(Outcome {
        ok: true,
        message: format!("Imported `{id}` from {}", found.provider),
        changes: vec![smithery::config_change(id, &server)?],
    })
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
mod tests;

#[cfg(test)]
mod smithery_flow_tests;

#[cfg(test)]
mod marketplace_tests;
