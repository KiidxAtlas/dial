//! Plugin installer. Stages a plugin tree from a git repository, an npm tarball, or a local
//! directory, verifies its integrity, and atomically moves it into `<root>/<name>/`. Install
//! metadata (`source`, `integrity`, `version`, `enabled`) is stored in the plugin's
//! `dial-plugin.json`, next to the optional runtime fields `program`, `args`, `env`, and `python`.
pub(crate) mod python;

use crate::hex;
use crate::runtime::process;
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Duration,
};

pub const MANIFEST_FILE: &str = "dial-plugin.json";
const STAGING_PREFIX: &str = ".staging-";
const MAX_METADATA_BYTES: u64 = 16 << 20;
const MAX_TARBALL_BYTES: u64 = 256 << 20;
const MAX_EXTRACTED_BYTES: u64 = 1 << 30;
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub name: String,
    pub version: Option<String>,
    pub source: String,
    pub integrity: String,
    pub enabled: bool,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginUpdate {
    pub name: String,
    /// Installed version, or the integrity value when the plugin has no version.
    pub from: Option<String>,
    /// Resolved version, or the resolved integrity value when the plugin has no version.
    pub to: Option<String>,
    pub changed: bool,
}

pub struct PluginStore {
    root: PathBuf,
}

/// A plugin tree staged under `<root>/.staging-*/tree`, verified but not yet installed.
struct Staged {
    dir: tempfile::TempDir,
    tree: PathBuf,
    name: String,
    version: Option<String>,
    integrity: String,
    source: String,
    manifest: Map<String, Value>,
}

struct Fetched {
    integrity: String,
    version: Option<String>,
    default_name: String,
}

enum Source<'a> {
    Git {
        url: &'a str,
        reference: Option<&'a str>,
    },
    Npm {
        name: &'a str,
        version: Option<&'a str>,
    },
    File(&'a str),
}

#[derive(Deserialize)]
struct Record {
    name: String,
    #[serde(default)]
    version: Option<String>,
    source: String,
    integrity: String,
    #[serde(default)]
    enabled: bool,
}

impl PluginStore {
    pub fn new(root: impl Into<PathBuf>) -> PluginStore {
        PluginStore { root: root.into() }
    }

    pub fn install(&self, source: &str) -> Result<InstalledPlugin> {
        let staged = self.stage(source)?;
        validate_staged_runtime(&staged)?;
        ensure!(
            !self.root.join(&staged.name).exists(),
            "plugin {} is already installed; use update to replace it",
            staged.name
        );
        let environment = python::prepare(&staged.manifest, &staged.tree, &self.root)?;
        let _lease = super::lease::exclusive(&self.root, &staged.name)?;
        ensure!(
            !self.root.join(&staged.name).exists(),
            "plugin {} was installed concurrently; use update",
            staged.name
        );
        let installed = self.finalize(&staged, true)?;
        fs::rename(&staged.tree, &installed.path)
            .with_context(|| format!("installing plugin {}", staged.name))?;
        if let Some(environment) = environment {
            let _path = environment.keep();
        }
        Ok(installed)
    }

    /// Every installed plugin with a readable record, sorted by name.
    pub fn list(&self) -> Result<Vec<InstalledPlugin>> {
        let mut plugins: Vec<InstalledPlugin> = self
            .scan()?
            .into_iter()
            .filter_map(|(_, record)| record.ok())
            .collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(plugins)
    }

    /// Each plugin directory that has an install manifest, with its record or the reason it cannot be
    /// read. Directories without a manifest are not plugins and are skipped.
    fn scan(&self) -> Result<Vec<(String, Result<InstalledPlugin>)>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("reading plugin directory"),
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if dir_name.starts_with('.')
                || validate_name(dir_name).is_err()
                || !path.is_dir()
                || !path.join(MANIFEST_FILE).is_file()
            {
                continue;
            }
            let record = read_record(&path).and_then(|plugin| {
                ensure!(
                    plugin.name == dir_name,
                    "manifest in {} names plugin {:?}",
                    path.display(),
                    plugin.name
                );
                Ok(plugin)
            });
            found.push((dir_name.to_owned(), record));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(found)
    }

    pub fn get(&self, name: &str) -> Result<InstalledPlugin> {
        let dir = self.plugin_dir(name)?;
        ensure!(
            dir.join(MANIFEST_FILE).is_file(),
            "plugin {name} is not installed"
        );
        let plugin = read_record(&dir)?;
        ensure!(
            plugin.name == name,
            "manifest in {} names plugin {:?}",
            dir.display(),
            plugin.name
        );
        Ok(plugin)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _lease = super::lease::exclusive(&self.root, name)?;
        let plugin = self.get(name)?;
        let environment = python::environment(&plugin.path, &self.root)?;
        fs::remove_dir_all(&plugin.path).with_context(|| format!("removing plugin {name}"))?;
        python::cleanup(environment)
    }

    /// Explicit purge only; ordinary removal intentionally preserves extension state.
    pub fn purge_state(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        for root in [".state", ".settings", ".cache"] {
            let parent = self.root.join(root);
            if parent.exists() {
                ensure!(
                    !fs::symlink_metadata(&parent)?.file_type().is_symlink(),
                    "extension state root must not be a symlink"
                );
            }
            let path = if root == ".settings" {
                parent.join(format!("{name}.json"))
            } else {
                parent.join(name)
            };
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    fs::remove_dir_all(&path)?
                }
                Ok(_) => fs::remove_file(&path)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        validate_name(name)?;
        let _lease = super::lease::exclusive(&self.root, name)?;
        let plugin = self.get(name)?;
        let manifest_path = plugin.path.join(MANIFEST_FILE);
        let mut map = read_manifest(&manifest_path)?;
        map.insert("enabled".into(), Value::Bool(enabled));
        write_json(&manifest_path, &map)
    }

    /// Re-resolves each plugin's recorded source and reinstalls it when its integrity changed.
    /// Stops at the first failure; updates applied before it remain in place.
    pub fn update(&self, name: Option<&str>) -> Result<Vec<PluginUpdate>> {
        let targets = match name {
            Some(name) => vec![self.get(name)?],
            None => self.list()?,
        };
        targets
            .iter()
            .map(|plugin| {
                self.refresh(plugin, &plugin.source)
                    .with_context(|| format!("updating plugin {}", plugin.name))
            })
            .collect()
    }

    /// Resolves every recorded source without installing anything.
    pub fn check_updates(&self) -> Result<Vec<PluginUpdate>> {
        self.list()?
            .iter()
            .map(|plugin| {
                let staged = self
                    .stage(&plugin.source)
                    .with_context(|| format!("resolving plugin {}", plugin.name))?;
                ensure!(
                    staged.name == plugin.name,
                    "source of {} now provides plugin {:?}",
                    plugin.name,
                    staged.name
                );
                Ok(PluginUpdate {
                    name: plugin.name.clone(),
                    from: Some(display_version(&plugin.version, &plugin.integrity)),
                    to: Some(display_version(&staged.version, &staged.integrity)),
                    changed: staged.integrity != plugin.integrity || staged.source != plugin.source,
                })
            })
            .collect()
    }

    /// Replaces an installed plugin with the tree resolved from `source`, keeping its enabled flag.
    pub(crate) fn reinstall(&self, name: &str, source: &str) -> Result<PluginUpdate> {
        let old = self.get(name)?;
        self.refresh(&old, source)
    }

    /// Runtime specs of every enabled plugin, one outcome per plugin. A broken plugin yields an
    /// error naming it, so healthy plugins are still returned. Only enumeration failures are `Err`.
    pub fn enabled_runtimes(&self) -> Result<Vec<Result<PluginRuntime>>> {
        let mut runtimes = Vec::new();
        for (dir_name, record) in self.scan()? {
            let outcome = record
                .and_then(|plugin| {
                    let spec = runtime_spec(&plugin)?;
                    Ok(spec.map(|(program, args, env)| (plugin, program, args, env)))
                })
                .with_context(|| format!("plugin {dir_name}"));
            match outcome {
                Ok(Some(runtime)) => runtimes.push(Ok(runtime)),
                Ok(None) => {}
                Err(error) => runtimes.push(Err(error)),
            }
        }
        Ok(runtimes)
    }

    fn plugin_dir(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    fn refresh(&self, old: &InstalledPlugin, source: &str) -> Result<PluginUpdate> {
        let staged = self.stage(source)?;
        ensure!(
            staged.name == old.name,
            "source for {} now provides plugin {:?}",
            old.name,
            staged.name
        );
        let from = Some(display_version(&old.version, &old.integrity));
        let runtime_healthy = !staged.manifest.contains_key("python")
            || read_manifest(&old.path.join(MANIFEST_FILE))
                .and_then(|map| python::runtime(&map, &old.path))
                .is_ok();
        if staged.source == old.source && staged.integrity == old.integrity && runtime_healthy {
            return Ok(PluginUpdate {
                name: old.name.clone(),
                from: from.clone(),
                to: from,
                changed: false,
            });
        }
        validate_staged_runtime(&staged)?;
        let environment = python::prepare(&staged.manifest, &staged.tree, &self.root)?;
        let _lease = super::lease::exclusive(&self.root, &old.name)?;
        let current = self.get(&old.name)?;
        ensure!(
            current.integrity == old.integrity && current.source == old.source,
            "plugin {} changed concurrently; retry update",
            old.name
        );
        let previous_environment = python::environment(&current.path, &self.root)?;
        let installed = self.finalize(&staged, current.enabled)?;
        swap_into_place(staged.dir, &staged.tree, &installed.path)?;
        if let Some(environment) = environment {
            let _path = environment.keep();
        }
        python::cleanup(previous_environment)?;
        Ok(PluginUpdate {
            name: old.name.clone(),
            from,
            to: Some(display_version(&installed.version, &installed.integrity)),
            changed: true,
        })
    }

    fn stage(&self, source: &str) -> Result<Staged> {
        let parsed = parse_source(source)?;
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let dir = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&self.root)
            .context("creating staging directory")?;
        let tree = dir.path().join("tree");
        let fetched = match parsed {
            Source::Git { url, reference } => stage_git(url, reference, &tree)?,
            Source::Npm { name, version } => stage_npm(name, version, &tree)?,
            Source::File(path) => stage_file(Path::new(path), &tree)?,
        };
        let manifest_path = tree.join(MANIFEST_FILE);
        let manifest = if manifest_path.exists() {
            read_manifest(&manifest_path)?
        } else {
            Map::new()
        };
        let name = match manifest.get("name") {
            None => fetched.default_name,
            Some(Value::String(name)) => name.clone(),
            Some(_) => bail!("{MANIFEST_FILE} field \"name\" must be a string"),
        };
        validate_name(&name)?;
        let version = match manifest.get("version") {
            None | Some(Value::Null) => fetched.version,
            Some(Value::String(version)) => Some(version.clone()),
            Some(_) => bail!("{MANIFEST_FILE} field \"version\" must be a string"),
        };
        Ok(Staged {
            dir,
            tree,
            name,
            version,
            integrity: fetched.integrity,
            source: source.to_string(),
            manifest,
        })
    }

    /// Writes the install metadata into the staged manifest and returns the installed record.
    fn finalize(&self, staged: &Staged, enabled: bool) -> Result<InstalledPlugin> {
        let mut map = staged.manifest.clone();
        map.insert("name".into(), Value::String(staged.name.clone()));
        map.insert("source".into(), Value::String(staged.source.clone()));
        map.insert("integrity".into(), Value::String(staged.integrity.clone()));
        map.insert("enabled".into(), Value::Bool(enabled));
        match &staged.version {
            Some(version) => {
                map.insert("version".into(), Value::String(version.clone()));
            }
            None => {
                map.remove("version");
            }
        }
        write_json(&staged.tree.join(MANIFEST_FILE), &map)?;
        Ok(InstalledPlugin {
            name: staged.name.clone(),
            version: staged.version.clone(),
            source: staged.source.clone(),
            integrity: staged.integrity.clone(),
            enabled,
            path: self.root.join(&staged.name),
        })
    }
}

/// Program, arguments and environment a plugin runs with.
pub type RuntimeSpec = (PathBuf, Vec<String>, BTreeMap<String, String>);
/// An installed plugin together with its runtime spec.
pub type PluginRuntime = (
    InstalledPlugin,
    PathBuf,
    Vec<String>,
    BTreeMap<String, String>,
);

/// Resolves a plugin's runtime executable. Returns `None` when the plugin is disabled or declares
/// no runtime. Legacy programs must be files inside the plugin directory; Python runtimes use a
/// managed interpreter installed explicitly during install/update, never during discovery.
pub fn runtime_spec(plugin: &InstalledPlugin) -> Result<Option<RuntimeSpec>> {
    if !plugin.enabled {
        return Ok(None);
    }
    let map = read_manifest(&plugin.path.join(MANIFEST_FILE))?;
    validate_compatibility(&map)?;
    runtime_from_manifest(&map, &plugin.path)
}

/// Parses the runtime fields of a manifest whose plugin tree is at `base`.
fn runtime_from_manifest(map: &Map<String, Value>, base: &Path) -> Result<Option<RuntimeSpec>> {
    if map.contains_key("python") {
        return python::runtime(map, base).map(Some);
    }
    let Some(program) = map.get("program") else {
        return Ok(None);
    };
    let program = program
        .as_str()
        .ok_or_else(|| anyhow!("{MANIFEST_FILE} field \"program\" must be a string"))?;
    let program = resolve_inside(base, program)?;
    let args = match map.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("{MANIFEST_FILE} \"args\" entries must be strings"))
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("{MANIFEST_FILE} field \"args\" must be an array"),
    };
    let env = manifest_env(map)?;
    Ok(Some((program, args, env)))
}

fn manifest_env(map: &Map<String, Value>) -> Result<BTreeMap<String, String>> {
    let env = match map.get("env") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .ok_or_else(|| anyhow!("{MANIFEST_FILE} \"env\" values must be strings"))?;
                Ok((key.clone(), value.to_owned()))
            })
            .collect::<Result<BTreeMap<_, _>>>()?,
        Some(_) => bail!("{MANIFEST_FILE} field \"env\" must be an object"),
    };
    Ok(env)
}

/// Checks a staged plugin's runtime declaration before it can be enabled, so a broken declaration
/// never becomes an installed, enabled record.
fn validate_staged_runtime(staged: &Staged) -> Result<()> {
    let validate = || -> Result<()> {
        validate_compatibility(&staged.manifest)?;
        if let Some(value) = staged.manifest.get("contributions") {
            super::contributions::validate(&super::contributions::Contributions::parse(
                value.clone(),
            )?)?;
        }
        if staged.manifest.contains_key("python") {
            python::validate(&staged.manifest, &staged.tree)?;
        } else {
            runtime_from_manifest(&staged.manifest, &staged.tree)?;
        }
        Ok(())
    };
    validate().with_context(|| format!("plugin {} has an invalid runtime declaration", staged.name))
}

/// Manifest API compatibility is independent of transport protocol negotiation.
pub fn validate_compatibility(map: &Map<String, Value>) -> Result<()> {
    if let Some(version) = map.get("apiVersion") {
        match version {
            Value::Number(number) => ensure!(
                number.as_u64() == Some(1),
                "extension requires an unsupported host API version (host is 1)"
            ),
            Value::Object(range) => {
                ensure!(
                    range.keys().all(|k| matches!(k.as_str(), "min" | "max")),
                    "unknown API compatibility field"
                );
                let min = range
                    .get("min")
                    .and_then(Value::as_u64)
                    .context("apiVersion.min must be an integer")?;
                let max = range
                    .get("max")
                    .and_then(Value::as_u64)
                    .context("apiVersion.max must be an integer")?;
                ensure!(
                    min <= 1 && max >= 1 && min <= max,
                    "extension is incompatible with host API 1"
                );
            }
            _ => bail!("apiVersion must be 1 or {{min,max}}"),
        }
    }
    Ok(())
}

/// Validates a plugin or marketplace name: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ensure!(
        valid,
        "invalid plugin name {name:?}: must match ^[A-Za-z0-9][A-Za-z0-9._-]{{0,127}}$"
    );
    Ok(())
}

pub(crate) fn validate_source(source: &str) -> Result<()> {
    parse_source(source).map(|_| ())
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp file in {}", dir.display()))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn display_version(version: &Option<String>, integrity: &str) -> String {
    version.clone().unwrap_or_else(|| integrity.to_string())
}

fn parse_source(source: &str) -> Result<Source<'_>> {
    if let Some(rest) = source.strip_prefix("git+") {
        let (url, reference) = match rest.split_once('#') {
            Some((url, reference)) => (url, Some(reference)),
            None => (rest, None),
        };
        ensure!(
            !url.is_empty() && !url.starts_with('-'),
            "invalid git url in {source:?}"
        );
        if let Some(reference) = reference {
            ensure!(
                !reference.is_empty() && !reference.starts_with('-'),
                "invalid git ref in {source:?}"
            );
        }
        return Ok(Source::Git { url, reference });
    }
    if let Some(rest) = source.strip_prefix("npm:") {
        let (name, version) = match rest.rfind('@') {
            Some(index) if index > 0 => (&rest[..index], Some(&rest[index + 1..])),
            _ => (rest, None),
        };
        ensure!(
            valid_npm_name(name),
            "invalid npm package name in {source:?}"
        );
        if let Some(version) = version {
            ensure!(
                !version.is_empty()
                    && version
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')),
                "invalid npm version in {source:?}"
            );
        }
        return Ok(Source::Npm { name, version });
    }
    if let Some(path) = source.strip_prefix("file:") {
        ensure!(!path.is_empty(), "file: source needs a directory path");
        return Ok(Source::File(path));
    }
    bail!(
        "unsupported plugin source {source:?}: expected git+<url>[#ref], npm:<name>[@version], or file:<dir>"
    )
}

fn valid_npm_name(name: &str) -> bool {
    let (scope, package) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, package)) => (Some(scope), package),
            None => return false,
        },
        None => (None, name),
    };
    let part_ok = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    };
    part_ok(package) && scope.is_none_or(part_ok)
}

fn stage_git(url: &str, reference: Option<&str>, tree: &Path) -> Result<Fetched> {
    let tree_str = tree.to_string_lossy().into_owned();
    git(None, &["clone", "--quiet", "--", url, tree_str.as_str()])
        .with_context(|| format!("cloning {url}"))?;
    if let Some(reference) = reference {
        git(Some(tree), &["checkout", "--quiet", reference, "--"])
            .with_context(|| format!("checking out {reference}"))?;
    }
    let commit = git(Some(tree), &["rev-parse", "--verify", "HEAD^{commit}"])?;
    fs::remove_dir_all(tree.join(".git")).context("removing .git metadata")?;
    Ok(Fetched {
        integrity: commit,
        version: None,
        default_name: name_from_url(url),
    })
}

/// Network git operations (clone, fetch) get a deadline so a stalled remote or an SSH prompt
/// cannot hang an install forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let never = AtomicBool::new(false);
    let finished = process::run(command, None, GIT_TIMEOUT, 1 << 20, &never, &mut |_, _| {})
        .map_err(|e| anyhow!("running git; is git installed? {e}"))?;
    let what = format!("git {}", args.join(" "));
    ensure!(
        finished.success(),
        "{}",
        finished.failure(&what, GIT_TIMEOUT)
    );
    Ok(String::from_utf8_lossy(&finished.stdout).trim().to_string())
}

fn name_from_url(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

fn stage_npm(name: &str, version: Option<&str>, tree: &Path) -> Result<Fetched> {
    let url = format!(
        "{NPM_REGISTRY}/{}/{}",
        name.replace('/', "%2F"),
        version.unwrap_or("latest")
    );
    let metadata: Value = serde_json::from_slice(&download(&url, MAX_METADATA_BYTES)?)
        .with_context(|| format!("parsing npm metadata from {url}"))?;
    let resolved = metadata
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("npm metadata for {name} has no version"))?
        .to_string();
    let dist = metadata
        .get("dist")
        .ok_or_else(|| anyhow!("npm metadata for {name} has no dist"))?;
    let tarball = dist
        .get("tarball")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("npm metadata for {name} has no dist.tarball"))?;
    ensure!(
        tarball.starts_with("https://"),
        "refusing non-HTTPS tarball {tarball}"
    );
    let bytes = download(tarball, MAX_TARBALL_BYTES)?;
    let integrity = if let Some(sri) = dist.get("integrity").and_then(Value::as_str) {
        verify_sri_sha512(sri, &bytes)?
    } else if let Some(shasum) = dist.get("shasum").and_then(Value::as_str) {
        verify_sha1_hex(shasum, &bytes)?
    } else {
        bail!(
            "npm metadata for {name} has neither dist.integrity nor dist.shasum; refusing to install"
        );
    };
    extract_tarball(&bytes, tree)?;
    Ok(Fetched {
        integrity,
        version: Some(resolved),
        default_name: name.rsplit('/').next().unwrap_or(name).to_string(),
    })
}

pub(crate) fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    // Without a global ceiling a stalled registry or tarball host blocks the install forever.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(300)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {url}"))?;
    ensure!(bytes.len() as u64 <= limit, "{url} exceeds {limit} bytes");
    Ok(bytes)
}

fn verify_sri_sha512(sri: &str, bytes: &[u8]) -> Result<String> {
    let expected = sri
        .split_whitespace()
        .find_map(|token| token.strip_prefix("sha512-"))
        .ok_or_else(|| anyhow!("unsupported integrity {sri:?}; expected sha512-<base64>"))?;
    let actual = STANDARD.encode(Sha512::digest(bytes));
    ensure!(
        actual == expected,
        "integrity mismatch: tarball does not match sha512-{expected}"
    );
    Ok(format!("sha512-{actual}"))
}

fn verify_sha1_hex(expected: &str, bytes: &[u8]) -> Result<String> {
    let actual = hex(&Sha1::digest(bytes));
    ensure!(
        actual.eq_ignore_ascii_case(expected.trim()),
        "shasum mismatch: tarball does not match sha1 {expected}"
    );
    Ok(format!("sha1:{actual}"))
}

fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let mut archive =
        tar::Archive::new(flate2::read::GzDecoder::new(bytes).take(MAX_EXTRACTED_BYTES));
    for entry in archive.entries().context("reading tarball")? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let mut parts = Vec::new();
        for component in raw.components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::CurDir => {}
                _ => bail!("unsafe path in tarball: {}", raw.display()),
            }
        }
        // npm tarballs wrap everything in a leading `package/` directory.
        if parts.len() <= 1 {
            continue;
        }
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            continue;
        }
        let dest_path = dest.join(parts[1..].iter().collect::<PathBuf>());
        if kind.is_dir() {
            fs::create_dir_all(&dest_path)?;
        } else if kind.is_file() {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.unpack(&dest_path)?;
        } else {
            bail!("unsupported tar entry type {kind:?} at {}", raw.display());
        }
    }
    ensure!(
        archive.into_inner().limit() > 0,
        "tarball expands beyond {MAX_EXTRACTED_BYTES} bytes"
    );
    Ok(())
}

fn stage_file(path: &Path, tree: &Path) -> Result<Fetched> {
    let source = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    ensure!(source.is_dir(), "{} is not a directory", source.display());
    let manifest_path = source.join(MANIFEST_FILE);
    let python = manifest_path.is_file() && read_manifest(&manifest_path)?.contains_key("python");
    copy_tree(&source, tree, python)?;
    Ok(Fetched {
        integrity: tree_digest(tree)?,
        version: None,
        default_name: source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    })
}

fn copy_tree(src: &Path, dst: &Path, python: bool) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        // Development environments are recreated locally, never copied into the managed store.
        if python
            && matches!(
                entry.file_name().to_str(),
                Some(".venv" | "venv" | "__pycache__")
            )
        {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&from, &to, python)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        } else {
            bail!("refusing to copy non-regular entry {}", from.display());
        }
    }
    Ok(())
}

/// Deterministic SHA-256 digest over relative file paths and file contents.
fn tree_digest(root: &Path) -> Result<String> {
    let mut files = BTreeMap::new();
    collect_files(root, root, &mut files)?;
    let mut hasher = Sha256::new();
    for (rel, path) in &files {
        let content = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(hex(&Sha256::digest(&content)).as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("sha256:{}", hex(&hasher.finalize())))
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.insert(rel, path);
        } else {
            bail!("unsupported non-regular entry {}", path.display());
        }
    }
    Ok(())
}

fn resolve_inside(base: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && rel.is_relative()
            && rel
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "program {relative:?} must be a relative path inside the plugin directory"
    );
    let base = fs::canonicalize(base).context("resolving plugin directory")?;
    let full = fs::canonicalize(base.join(rel))
        .with_context(|| format!("program {relative:?} not found in plugin"))?;
    ensure!(
        full.starts_with(&base) && full.is_file(),
        "program {relative:?} escapes the plugin directory or is not a file"
    );
    Ok(full)
}

fn read_manifest(path: &Path) -> Result<Map<String, Value>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parsing {}", path.display()))?
    {
        Value::Object(map) => Ok(map),
        _ => bail!("{} must contain a JSON object", path.display()),
    }
}

fn read_record(dir: &Path) -> Result<InstalledPlugin> {
    let map = read_manifest(&dir.join(MANIFEST_FILE))?;
    let record: Record = serde_json::from_value(Value::Object(map))
        .with_context(|| format!("invalid install record in {}", dir.display()))?;
    Ok(InstalledPlugin {
        name: record.name,
        version: record.version,
        source: record.source,
        integrity: record.integrity,
        enabled: record.enabled,
        path: dir.to_path_buf(),
    })
}

fn write_json(path: &Path, map: &Map<String, Value>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(map)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// Moves the staged tree into `target`, keeping the previous tree (if any) inside the staging
/// directory until the swap succeeds. On failure the previous tree is restored; if that restore
/// also fails, the staging directory is kept so the previous tree is not deleted.
fn swap_into_place(staging: tempfile::TempDir, tree: &Path, target: &Path) -> Result<()> {
    let previous = staging.path().join("previous");
    let had_previous = target.exists();
    if had_previous {
        fs::rename(target, &previous)
            .with_context(|| format!("moving aside {}", target.display()))?;
    }
    if let Err(error) = fs::rename(tree, target) {
        if had_previous && let Err(restore) = fs::rename(&previous, target) {
            let kept = staging.keep().join("previous");
            return Err(error).with_context(|| {
                    format!(
                        "installing {} failed and the previous install could not be restored ({restore}); it is kept at {}",
                        target.display(),
                        kept.display()
                    )
                });
        }
        return Err(error).with_context(|| format!("installing {}", target.display()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
