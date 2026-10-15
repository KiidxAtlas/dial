//! Plugin marketplaces: named JSON indexes of installable plugins. Each index is fetched over
//! HTTPS or read from a local file, validated, and snapshotted under `<root>/<name>.json`. The
//! registry in `<root>/marketplaces.json` records each marketplace's index location and refresh
//! time. Nothing here hardcodes a location; the caller chooses `root`.
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

const REGISTRY_FILE: &str = "marketplaces.json";
const RESERVED_NAME: &str = "marketplaces";
const MAX_INDEX_BYTES: u64 = 16 << 20;

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceInfo {
    pub name: String,
    pub index: String,
    pub refreshed_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketplaceEntry {
    pub marketplace: String,
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub description: Option<String>,
}

pub struct Marketplaces {
    root: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    marketplaces: BTreeMap<String, Registered>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Registered {
    index: String,
    refreshed_at: Option<u64>,
}

#[derive(Deserialize)]
struct Index {
    plugins: Vec<IndexPlugin>,
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

    pub fn list(&self) -> Result<Vec<MarketplaceInfo>> {
        Ok(self
            .read_registry()?
            .marketplaces
            .into_iter()
            .map(|(name, registered)| MarketplaceInfo {
                name,
                index: registered.index,
                refreshed_at: registered.refreshed_at,
            })
            .collect())
    }

    /// Re-fetches one marketplace's index, or every registered index when `name` is `None`.
    pub fn refresh(&self, name: Option<&str>) -> Result<()> {
        let mut registry = self.read_registry()?;
        let names = self.select(&registry, name)?;
        for name in names {
            let registered = registry
                .marketplaces
                .get(&name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("marketplace {name} is not registered"))?;
            let text = fetch_index(&registered.index)
                .with_context(|| format!("refreshing marketplace {name}"))?;
            parse_index(&text).with_context(|| format!("marketplace {name}"))?;
            write_atomic(&self.snapshot_path(&name), text.as_bytes())?;
            registry.marketplaces.insert(
                name,
                Registered {
                    index: registered.index,
                    refreshed_at: Some(now()),
                },
            );
            self.write_registry(&registry)?;
        }
        Ok(())
    }

    /// Lists entries from the stored snapshots. Does not touch the network.
    pub fn entries(&self, name: Option<&str>) -> Result<Vec<MarketplaceEntry>> {
        let registry = self.read_registry()?;
        let names = self.select(&registry, name)?;
        let mut entries = Vec::new();
        for marketplace in names {
            let text = fs::read_to_string(self.snapshot_path(&marketplace)).with_context(|| {
                format!("marketplace {marketplace} has no snapshot; run refresh first")
            })?;
            for plugin in parse_index(&text)? {
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
            let candidates: Vec<&MarketplaceEntry> = entries
                .iter()
                .filter(|entry| entry.name == plugin.name)
                .collect();
            // Prefer the marketplace entry that already points at the installed source.
            let chosen = candidates
                .iter()
                .copied()
                .find(|entry| entry.source == plugin.source)
                .or_else(|| candidates.first().copied());
            let Some(entry) = chosen else {
                continue;
            };
            let source_changed = entry.source != plugin.source;
            let version_changed = entry.version.is_some() && entry.version != plugin.version;
            if source_changed || version_changed {
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

    fn read_registry(&self) -> Result<Registry> {
        let path = self.root.join(REGISTRY_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
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

fn parse_index(text: &str) -> Result<Vec<IndexPlugin>> {
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
    Ok(index.plugins)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::installer::test_support::write_plugin;
    use super::*;
    use serde_json::json;

    struct Fixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().to_path_buf();
            Fixture { _temp: temp, base }
        }

        fn plugin_dir(&self, name: &str, version: &str) -> PathBuf {
            let dir = self.base.join("plugins").join(name);
            write_plugin(&dir, &json!({"name": name, "version": version}).to_string());
            dir
        }

        fn write_index(&self, file: &str, plugins: serde_json::Value) -> PathBuf {
            let path = self.base.join(file);
            fs::write(&path, json!({ "plugins": plugins }).to_string()).unwrap();
            path
        }

        fn store(&self) -> PluginStore {
            PluginStore::new(self.base.join("store"))
        }

        fn marketplaces(&self) -> Marketplaces {
            Marketplaces::new(self.base.join("marketplaces"))
        }
    }

    #[test]
    fn add_list_entries_and_install_by_reference() {
        let fx = Fixture::new();
        let alpha = fx.plugin_dir("alpha", "1.0.0");
        let index = fx.write_index(
            "index.json",
            json!([{
                "name": "alpha",
                "version": "1.0.0",
                "source": format!("file:{}", alpha.display()),
                "description": "first"
            }]),
        );
        let marketplaces = fx.marketplaces();
        marketplaces.add("local", index.to_str().unwrap()).unwrap();

        let infos = marketplaces.list().unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "local");
        assert!(infos[0].refreshed_at.is_some());

        let entries = marketplaces.entries(None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].marketplace, "local");
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[0].description.as_deref(), Some("first"));

        let store = fx.store();
        let installed = marketplaces.install("alpha@local", &store).unwrap();
        assert_eq!(installed.name, "alpha");
        assert_eq!(installed.version.as_deref(), Some("1.0.0"));
        assert_eq!(store.list().unwrap().len(), 1);

        let err = marketplaces
            .add("local", index.to_str().unwrap())
            .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn bare_name_is_ambiguous_across_marketplaces() {
        let fx = Fixture::new();
        let beta_a = fx.base.join("plugins/beta-a");
        write_plugin(
            &beta_a,
            &json!({"name": "beta", "version": "1.0.0"}).to_string(),
        );
        let beta_b = fx.base.join("plugins/beta-b");
        write_plugin(
            &beta_b,
            &json!({"name": "beta", "version": "2.0.0"}).to_string(),
        );
        let index_a = fx.write_index(
            "a.json",
            json!([{"name": "beta", "version": "1.0.0", "source": format!("file:{}", beta_a.display())}]),
        );
        let index_b = fx.write_index(
            "b.json",
            json!([{"name": "beta", "version": "2.0.0", "source": format!("file:{}", beta_b.display())}]),
        );
        let marketplaces = fx.marketplaces();
        marketplaces
            .add("mkt-a", index_a.to_str().unwrap())
            .unwrap();
        marketplaces
            .add("mkt-b", index_b.to_str().unwrap())
            .unwrap();

        let store = fx.store();
        let err = marketplaces.install("beta", &store).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("beta@mkt-a"), "{message}");
        assert!(message.contains("beta@mkt-b"), "{message}");
        assert!(store.list().unwrap().is_empty());

        let installed = marketplaces.install("beta@mkt-b", &store).unwrap();
        assert_eq!(installed.version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn refresh_and_upgrade_apply_new_version() {
        let fx = Fixture::new();
        let alpha = fx.plugin_dir("alpha", "1.0.0");
        let index = fx.write_index(
            "index.json",
            json!([{"name": "alpha", "version": "1.0.0", "source": format!("file:{}", alpha.display())}]),
        );
        let marketplaces = fx.marketplaces();
        marketplaces.add("local", index.to_str().unwrap()).unwrap();
        let store = fx.store();
        marketplaces.install("alpha", &store).unwrap();

        write_plugin(
            &alpha,
            &json!({"name": "alpha", "version": "2.0.0"}).to_string(),
        );
        fx.write_index(
            "index.json",
            json!([{"name": "alpha", "version": "2.0.0", "source": format!("file:{}", alpha.display())}]),
        );

        let updates = marketplaces.upgrade(&store, None).unwrap();
        assert_eq!(updates.len(), 1);
        assert!(updates[0].changed);
        assert_eq!(
            store.get("alpha").unwrap().version.as_deref(),
            Some("2.0.0")
        );
    }

    #[test]
    fn remove_drops_registration_and_snapshot() {
        let fx = Fixture::new();
        let alpha = fx.plugin_dir("alpha", "1.0.0");
        let index = fx.write_index(
            "index.json",
            json!([{"name": "alpha", "source": format!("file:{}", alpha.display())}]),
        );
        let marketplaces = fx.marketplaces();
        marketplaces.add("local", index.to_str().unwrap()).unwrap();
        marketplaces.remove("local").unwrap();
        assert!(marketplaces.list().unwrap().is_empty());
        assert!(marketplaces.entries(None).unwrap().is_empty());
        assert!(!fx.base.join("marketplaces/local.json").exists());
        assert!(marketplaces.remove("local").is_err());
    }

    #[test]
    fn reserved_and_invalid_names_are_rejected() {
        let fx = Fixture::new();
        let marketplaces = fx.marketplaces();
        assert!(marketplaces.add("marketplaces", "missing.json").is_err());
        assert!(marketplaces.add("../escape", "missing.json").is_err());
    }
}
