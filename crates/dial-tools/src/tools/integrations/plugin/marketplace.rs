//! Plugin marketplaces: named indexes of installable plugins and skills. Each index is fetched over
//! HTTPS or read from a local file, validated, and snapshotted under `<root>/<name>.json`. The
//! registry in `<root>/marketplaces.json` records each marketplace's index location, whether it is
//! enabled, and when it was refreshed. Curated built-in indexes ship inside the binary,
//! so browsing them needs no network. The caller chooses `root`.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::installer::{
    InstalledPlugin, PluginStore, PluginUpdate, download, validate_name, validate_source,
    write_atomic,
};
use crate::storage::skills::packages::{registry::Entry as SkillEntry, source as skill_source};

const REGISTRY_FILE: &str = "marketplaces.json";
const RESERVED_NAME: &str = "marketplaces";
const MAX_INDEX_BYTES: u64 = 16 << 20;
/// The original built-in marketplace; retained for registry migration compatibility.
pub const DEFAULT_NAME: &str = "anthropic-skills";
/// Index location of bundled catalogs. It is not a URL, so it cannot be added by hand.
pub const BUILTIN_INDEX: &str = "builtin";
/// Each source has its own ordinary marketplace index and independent enable/remove state.
const BUILTINS: &[(&str, &str)] = &[
    (DEFAULT_NAME, include_str!("default-marketplace.json")),
    ("openai-skills", include_str!("openai-marketplace.json")),
    ("vercel-skills", include_str!("vercel-marketplace.json")),
];

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceInfo {
    pub name: String,
    pub index: String,
    pub refreshed_at: Option<u64>,
    pub enabled: bool,
    /// True for a bundled catalog, which ships inside Dial.
    pub builtin: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceEntry {
    pub marketplace: String,
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub description: Option<String>,
}

/// A skill a marketplace offers. `url` uses the skill install syntax, `repo[@ref][#path]`.
#[derive(Clone, Debug, Serialize)]
pub struct SkillListing {
    pub marketplace: String,
    pub name: String,
    pub description: String,
    pub url: String,
}

pub struct Marketplaces {
    root: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    marketplaces: BTreeMap<String, Registered>,
    /// Legacy marker: the original Anthropic default was already offered, even if removed.
    #[serde(default)]
    seeded: bool,
    /// Names already offered, saved on the next write so removals survive future catalog additions.
    #[serde(default)]
    seeded_builtins: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Registered {
    index: String,
    refreshed_at: Option<u64>,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Deserialize)]
struct Index {
    #[serde(default)]
    plugins: Vec<IndexPlugin>,
    #[serde(default)]
    skills: Vec<SkillEntry>,
}

#[derive(Clone, Deserialize)]
struct IndexPlugin {
    name: String,
    #[serde(default)]
    version: Option<String>,
    source: String,
    #[serde(default)]
    description: Option<String>,
}

/// An installed plugin paired with the marketplace entry that should replace it.
struct Planned {
    plugin: InstalledPlugin,
    entry: MarketplaceEntry,
}

impl Marketplaces {
    pub fn new(root: impl Into<PathBuf>) -> Marketplaces {
        Marketplaces { root: root.into() }
    }

    pub fn add(&self, name: &str, index: &str) -> Result<()> {
        validate_marketplace_name(name)?;
        let mut registry = self.read_registry()?;
        ensure!(
            !registry.marketplaces.contains_key(name),
            "marketplace {name} already exists"
        );
        let index = normalize_index(index)?;
        let text = fetch_index(&index)?;
        parse_index(&text)?;
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        write_atomic(&self.snapshot_path(name), text.as_bytes())?;
        registry.marketplaces.insert(
            name.to_string(),
            Registered {
                index,
                refreshed_at: Some(now()),
                enabled: true,
            },
        );
        self.write_registry(&registry)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        validate_marketplace_name(name)?;
        let mut registry = self.read_registry()?;
        ensure!(
            registry.marketplaces.remove(name).is_some(),
            "marketplace {name} is not registered"
        );
        self.write_registry(&registry)?;
        match fs::remove_file(self.snapshot_path(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing snapshot of {name}")),
        }
    }

    /// Turns a marketplace on or off without forgetting its index. A disabled marketplace offers
    /// no plugins or skills, and `refresh` leaves it alone.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        validate_marketplace_name(name)?;
        let mut registry = self.read_registry()?;
        let registered = registry
            .marketplaces
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("marketplace {name} is not registered"))?;
        registered.enabled = enabled;
        self.write_registry(&registry)
    }

    pub fn list(&self) -> Result<Vec<MarketplaceInfo>> {
        Ok(self
            .read_registry()?
            .marketplaces
            .into_iter()
            .map(|(name, registered)| MarketplaceInfo {
                builtin: registered.index == BUILTIN_INDEX,
                name,
                index: registered.index,
                refreshed_at: registered.refreshed_at,
                enabled: registered.enabled,
            })
            .collect())
    }

    /// Re-fetches one marketplace's index, or every enabled index when `name` is `None`.
    /// Built-in indexes are part of Dial and have nothing to fetch.
    pub fn refresh(&self, name: Option<&str>) -> Result<()> {
        let mut registry = self.read_registry()?;
        let names = self.select(&registry, name)?;
        for name in names {
            let registered = registry
                .marketplaces
                .get(&name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("marketplace {name} is not registered"))?;
            if !registered.enabled || registered.index == BUILTIN_INDEX {
                continue;
            }
            let text = fetch_index(&registered.index)
                .with_context(|| format!("refreshing marketplace {name}"))?;
            parse_index(&text).with_context(|| format!("marketplace {name}"))?;
            write_atomic(&self.snapshot_path(&name), text.as_bytes())?;
            registry.marketplaces.insert(
                name,
                Registered {
                    index: registered.index,
                    refreshed_at: Some(now()),
                    enabled: registered.enabled,
                },
            );
            self.write_registry(&registry)?;
        }
        Ok(())
    }

    /// Lists plugin entries from the enabled marketplaces' stored indexes. Does not touch the network.
    pub fn entries(&self, name: Option<&str>) -> Result<Vec<MarketplaceEntry>> {
        let mut entries = Vec::new();
        for (marketplace, index) in self.indexes(name)? {
            for plugin in index.plugins {
                entries.push(MarketplaceEntry {
                    marketplace: marketplace.clone(),
                    name: plugin.name,
                    version: plugin.version,
                    source: plugin.source,
                    description: plugin.description,
                });
            }
        }
        Ok(entries)
    }

    /// Lists skill entries from the enabled marketplaces' stored indexes. Does not touch the network.
    pub fn skills(&self, name: Option<&str>) -> Result<Vec<SkillListing>> {
        let mut skills = Vec::new();
        for (marketplace, index) in self.indexes(name)? {
            for skill in index.skills {
                skills.push(SkillListing {
                    marketplace: marketplace.clone(),
                    name: skill.name,
                    description: skill.description,
                    url: skill.url,
                });
            }
        }
        Ok(skills)
    }

    /// Parsed index of each selected, enabled marketplace.
    fn indexes(&self, name: Option<&str>) -> Result<Vec<(String, Index)>> {
        let registry = self.read_registry()?;
        let names = self.select(&registry, name)?;
        let mut out = Vec::new();
        for marketplace in names {
            let registered = &registry.marketplaces[&marketplace];
            if !registered.enabled {
                continue;
            }
            let text = self.index_text(&marketplace, &registered.index)?;
            let index = parse_index(&text).with_context(|| format!("marketplace {marketplace}"))?;
            out.push((marketplace, index));
        }
        Ok(out)
    }

    /// Installs `plugin@marketplace`, or a bare `plugin` that exactly one marketplace lists.
    pub fn install(&self, plugin_ref: &str, store: &PluginStore) -> Result<InstalledPlugin> {
        let entry = self.resolve(plugin_ref)?;
        let installed = store
            .install(&entry.source)
            .with_context(|| format!("installing {}@{}", entry.name, entry.marketplace))?;
        if installed.name != entry.name {
            let removed = store.remove(&installed.name);
            let leftover = match removed {
                Ok(()) => String::new(),
                Err(error) => format!(
                    "; removing the installed {} also failed ({error:#}), so it is still installed",
                    installed.name
                ),
            };
            bail!(
                "marketplace {} lists {} but its source provides plugin {}{leftover}",
                entry.marketplace,
                entry.name,
                installed.name
            );
        }
        Ok(installed)
    }

    /// Refreshes every index, then updates installed plugins whose marketplace entry changed in
    /// version or source. `name` restricts the work to one installed plugin.
    pub fn upgrade(&self, store: &PluginStore, name: Option<&str>) -> Result<Vec<PluginUpdate>> {
        self.refresh(None)?;
        self.plan_upgrades(store, name)?
            .into_iter()
            .map(|planned| {
                store
                    .reinstall(&planned.plugin.name, &planned.entry.source)
                    .with_context(|| format!("upgrading plugin {}", planned.plugin.name))
            })
            .collect()
    }

    fn resolve(&self, plugin_ref: &str) -> Result<MarketplaceEntry> {
        let (plugin, marketplace) = match plugin_ref.split_once('@') {
            Some((plugin, marketplace)) => {
                ensure!(
                    !plugin.is_empty() && !marketplace.is_empty(),
                    "plugin reference {plugin_ref:?} must be plugin@marketplace"
                );
                (plugin, Some(marketplace))
            }
            None => (plugin_ref, None),
        };
        validate_name(plugin)?;
        let mut matches: Vec<MarketplaceEntry> = self
            .entries(marketplace)?
            .into_iter()
            .filter(|entry| entry.name == plugin)
            .collect();
        match matches.len() {
            0 => match marketplace {
                Some(marketplace) => {
                    bail!("plugin {plugin} is not listed by marketplace {marketplace}")
                }
                None => bail!("plugin {plugin} is not listed by any marketplace"),
            },
            1 => Ok(matches.remove(0)),
            _ => {
                let candidates = matches
                    .iter()
                    .map(|entry| format!("{}@{}", entry.name, entry.marketplace))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("plugin {plugin} is listed by several marketplaces; use one of: {candidates}")
            }
        }
    }

    fn plan_upgrades(&self, store: &PluginStore, name: Option<&str>) -> Result<Vec<Planned>> {
        let entries = self.entries(None)?;
        let installed = match name {
            Some(name) => vec![store.get(name)?],
            None => store.list()?,
        };
        let mut planned = Vec::new();
        for plugin in installed {
            // Only an entry that still points at the installed source is the same publisher. A removed
            // or disabled catalog, a direct install, or another publisher's entry under the same name
            // never replaces the plugin implicitly; that needs an explicit `plugin@marketplace` install.
            let matched: Vec<&MarketplaceEntry> = entries
                .iter()
                .filter(|entry| entry.name == plugin.name && entry.source == plugin.source)
                .collect();
            let Some(entry) = matched.first().copied() else {
                if name.is_some() {
                    bail!(
                        "plugin {} is not listed by an enabled marketplace at its installed source {}; to change publisher, remove it and install {}@<marketplace> explicitly",
                        plugin.name,
                        plugin.source,
                        plugin.name
                    );
                }
                continue;
            };
            if entry.version.is_some() && entry.version != plugin.version {
                planned.push(Planned {
                    plugin,
                    entry: entry.clone(),
                });
            }
        }
        Ok(planned)
    }

    fn select(&self, registry: &Registry, name: Option<&str>) -> Result<Vec<String>> {
        match name {
            Some(name) => {
                validate_marketplace_name(name)?;
                ensure!(
                    registry.marketplaces.contains_key(name),
                    "marketplace {name} is not registered"
                );
                Ok(vec![name.to_string()])
            }
            None => Ok(registry.marketplaces.keys().cloned().collect()),
        }
    }

    fn snapshot_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }

    fn index_text(&self, name: &str, index: &str) -> Result<String> {
        if index == BUILTIN_INDEX {
            return BUILTINS
                .iter()
                .find(|(builtin, _)| *builtin == name)
                .map(|(_, text)| (*text).to_string())
                .ok_or_else(|| anyhow::anyhow!("unknown built-in marketplace {name}"));
        }
        fs::read_to_string(self.snapshot_path(name))
            .with_context(|| format!("marketplace {name} has no snapshot; run refresh first"))
    }

    fn read_registry(&self) -> Result<Registry> {
        let path = self.root.join(REGISTRY_FILE);
        let mut registry: Registry = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Registry::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        // The old boolean records only Anthropic, including an intentional removal.
        if registry.seeded {
            registry.seeded_builtins.insert(DEFAULT_NAME.to_string());
        }
        for &(name, _) in BUILTINS {
            if registry.seeded_builtins.insert(name.to_string()) {
                registry
                    .marketplaces
                    .entry(name.to_string())
                    .or_insert(Registered {
                        index: BUILTIN_INDEX.to_string(),
                        refreshed_at: None,
                        enabled: true,
                    });
            }
        }
        registry.seeded = true;
        Ok(registry)
    }

    fn write_registry(&self, registry: &Registry) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let mut text = serde_json::to_string_pretty(registry)?;
        text.push('\n');
        write_atomic(&self.root.join(REGISTRY_FILE), text.as_bytes())
    }
}

fn validate_marketplace_name(name: &str) -> Result<()> {
    validate_name(name)?;
    ensure!(
        name != RESERVED_NAME,
        "marketplace name {RESERVED_NAME:?} is reserved"
    );
    Ok(())
}

fn normalize_index(index: &str) -> Result<String> {
    if index.starts_with("https://") {
        return Ok(index.to_string());
    }
    ensure!(
        !index.starts_with("http://"),
        "marketplace index {index} must use https"
    );
    let path = fs::canonicalize(index).with_context(|| {
        format!("marketplace index {index} is neither an https URL nor an existing file")
    })?;
    Ok(path.to_string_lossy().into_owned())
}

fn fetch_index(index: &str) -> Result<String> {
    if index.starts_with("https://") {
        let bytes = download(index, MAX_INDEX_BYTES)?;
        return String::from_utf8(bytes).with_context(|| format!("{index} is not UTF-8"));
    }
    fs::read_to_string(Path::new(index)).with_context(|| format!("reading {index}"))
}

fn parse_index(text: &str) -> Result<Index> {
    let index: Index = serde_json::from_str(text).context("parsing marketplace index")?;
    let mut seen = BTreeSet::new();
    for plugin in &index.plugins {
        validate_name(&plugin.name).with_context(|| format!("index entry {:?}", plugin.name))?;
        validate_source(&plugin.source).with_context(|| format!("index entry {}", plugin.name))?;
        ensure!(
            seen.insert(plugin.name.as_str()),
            "duplicate plugin {} in marketplace index",
            plugin.name
        );
    }
    let mut seen_skills = BTreeSet::new();
    for skill in &index.skills {
        ensure!(
            skill_source::valid_package_name(&skill.name),
            "skill name {:?} must use letters, digits, '-', '_' or '.'",
            skill.name
        );
        skill_source::parse(&skill.url)
            .with_context(|| format!("skill {} has an invalid url", skill.name))?;
        ensure!(
            seen_skills.insert(skill.name.as_str()),
            "duplicate skill {} in marketplace index",
            skill.name
        );
    }
    Ok(index)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
