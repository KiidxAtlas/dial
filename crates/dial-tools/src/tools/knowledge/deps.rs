//! The `dep_source` tool: read-only source of a dependency at the version this workspace locked or
//! installed (Cargo.lock + the cargo registry, node_modules, the project's virtualenv, go.mod + the
//! module cache or `vendor/`, Gradle/Maven `-sources.jar` files, installed Ruby gems, Composer's
//! `vendor/` and the NuGet global packages folder). It never fetches, builds or runs package code.
//!
//! Read authority. A package inside a workspace root follows the workspace's protected paths. A
//! package in a shared cache outside every root (cargo registry, Go module cache, local Maven
//! repository, Gradle module cache, gem directories, NuGet packages) is read where the shell
//! sandbox would let `bash` read it as well (the Dev policy), and under the strict policy too
//! when it lies in one of the resolved dependency caches (see [`Caches::contain`]): a read-only
//! grant for this tool alone. Every
//! denied (credential or Dial state) path still wins. Other paths stay unreadable under the
//! strict policy unless the user attaches them with `/add-dir`. Every read stays inside the
//! resolved package root, which in turn must lie inside the cache or install directory it was
//! resolved from; symlinks are not followed out of it.
use crate::config::types::Config;
use crate::tools::exec::sandbox::Policy;
use crate::tools::integrations::ext::{Concurrency, Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace, opt_str, req_str, tool_schema};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const DESCRIPTION: &str = "Read-only source of a dependency at the version this workspace locked or installed: \
cargo (Cargo.lock + cargo registry), npm (node_modules), python (project .venv site-packages), go (go.mod + \
module cache or vendor/), jvm (gradle.lockfile/pom.xml + -sources.jar in ~/.m2 or the Gradle cache; name is \
group:artifact), ruby (Gemfile.lock + installed gems), composer (composer.lock + vendor/), nuget \
(packages.lock.json or obj/project.assets.json + NuGet packages folder). Never fetches or runs package code. \
`name`: crate/package/module (`name@version` picks one of several locked versions); `ecosystem` is inferred \
from workspace manifests when omitted. Without `query` or `path` it reports version, root and top-level files. \
`query`: literal text searched in the package (path:line results). `path`: package-relative file (optional \
`:N-M` line range) or directory to list; may combine with `query`.";

const ECOSYSTEMS: &[&str] = &[
    "cargo", "npm", "python", "go", "jvm", "ruby", "composer", "nuget",
];
/// Lines of a file excerpt when no range is given, and the most one call returns.
const EXCERPT_LINES: usize = 200;
const MAX_EXCERPT_LINES: usize = 400;
/// Bytes of text one result carries; well under the per-result model limit.
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_MATCHES: usize = 60;
const MAX_SCANNED_FILES: usize = 20_000;
const MAX_SEARCH_FILE: u64 = 2 * 1024 * 1024;
const MAX_READ_FILE: u64 = 8 * 1024 * 1024;
const MAX_LISTING: usize = 200;
const MAX_LINE_CHARS: usize = 240;
/// Directories a package search never enters.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv"];

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(DepSource)]
}

struct DepSource;

impl Tool for DepSource {
    fn name(&self) -> &str {
        "dep_source"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "dep_source",
            DESCRIPTION,
            json!({
                "name": {"type": "string", "description": "crate, package or module name (jvm: group:artifact); optional @version"},
                "ecosystem": {"type": "string", "enum": ECOSYSTEMS},
                "query": {"type": "string", "description": "literal text to find in the package source"},
                "path": {"type": "string", "description": "package-relative file or directory; files accept :N-M"},
            }),
            &["name"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
    }

    fn concurrency(&self, _args: &Value) -> Concurrency {
        Concurrency::Shared
    }

    /// Manifests, lockfiles and installed packages may lie anywhere in the workspace; shared
    /// caches outside every root are not native write targets.
    fn resources(
        &self,
        workspace: &Workspace,
        _args: &Value,
    ) -> Option<crate::tools::surface::resources::Access> {
        Some(crate::tools::surface::resources::workspace_reads(workspace))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        run(ctx.workspace, args, &Caches::from_env()).map_or_else(ToolOutput::err, ToolOutput::ok)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ecosystem {
    Cargo,
    Npm,
    Python,
    Go,
    Jvm,
    Ruby,
    Composer,
    NuGet,
}

impl Ecosystem {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "cargo" | "rust" => Ok(Self::Cargo),
            "npm" | "node" => Ok(Self::Npm),
            "python" | "pip" | "uv" => Ok(Self::Python),
            "go" => Ok(Self::Go),
            "jvm" | "maven" | "gradle" | "java" | "kotlin" => Ok(Self::Jvm),
            "ruby" | "gem" | "rubygems" | "bundler" => Ok(Self::Ruby),
            "composer" | "php" => Ok(Self::Composer),
            "nuget" | "dotnet" => Ok(Self::NuGet),
            _ => Err(format!(
                "unknown ecosystem {name:?}; use one of: {}",
                ECOSYSTEMS.join(", ")
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Python => "python",
            Self::Go => "go",
            Self::Jvm => "jvm",
            Self::Ruby => "ruby",
            Self::Composer => "composer",
            Self::NuGet => "nuget",
        }
    }

    /// Ecosystems whose manifests sit in `root`, in a fixed order.
    fn present(root: &Path) -> Vec<Self> {
        let any = |names: &[&str]| names.iter().any(|name| root.join(name).exists());
        let mut found = Vec::new();
        if any(&["Cargo.lock", "Cargo.toml"]) {
            found.push(Self::Cargo);
        }
        if any(&["package.json", "node_modules"]) {
            found.push(Self::Npm);
        }
        if any(&[
            "pyproject.toml",
            "uv.lock",
            "requirements.txt",
            "setup.py",
            ".venv",
            "venv",
        ]) {
            found.push(Self::Python);
        }
        if any(&["go.mod"]) {
            found.push(Self::Go);
        }
        if any(&[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "gradle.lockfile",
        ]) {
            found.push(Self::Jvm);
        }
        if any(&["Gemfile", "Gemfile.lock", "gems.rb", "gems.locked"]) {
            found.push(Self::Ruby);
        }
        if any(&["composer.json", "composer.lock"]) {
            found.push(Self::Composer);
        }
        if any(&["packages.lock.json", "Directory.Packages.props", "obj"])
            || nuget::has_project(root)
        {
            found.push(Self::NuGet);
        }
        found
    }
}

/// Shared dependency caches outside the workspace. Injected so tests never read the real ones.
#[derive(Debug, Clone, Default)]
pub(crate) struct Caches {
    pub cargo_home: Option<PathBuf>,
    pub go_mod_cache: Option<PathBuf>,
    /// The local Maven repository (`~/.m2/repository`).
    pub maven_repo: Option<PathBuf>,
    /// The Gradle user home (`GRADLE_USER_HOME`, else `~/.gradle`).
    pub gradle_home: Option<PathBuf>,
    /// Gem installation directories, each holding a `gems/` directory (`GEM_HOME`, `GEM_PATH`,
    /// user and version-manager installs).
    pub gem_paths: Vec<PathBuf>,
    /// The NuGet global packages folder (`NUGET_PACKAGES`, else `~/.nuget/packages`).
    pub nuget_packages: Option<PathBuf>,
}

impl Caches {
    fn from_env() -> Self {
        let env = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = env("HOME").or_else(|| env("USERPROFILE"));
        let gopath = env("GOPATH")
            .and_then(|paths| std::env::split_paths(&paths).next())
            .or_else(|| home.as_ref().map(|home| home.join("go")));
        let mut gem_paths: Vec<PathBuf> = env("GEM_HOME").into_iter().collect();
        if let Some(paths) = std::env::var_os("GEM_PATH") {
            gem_paths
                .extend(std::env::split_paths(&paths).filter(|path| !path.as_os_str().is_empty()));
        }
        if let Some(home) = &home {
            gem_paths.extend(subdirs(&home.join(".gem/ruby")));
            gem_paths.extend(subdirs(&home.join(".local/share/gem/ruby")));
            for version in subdirs(&home.join(".rbenv/versions")) {
                gem_paths.extend(subdirs(&version.join("lib/ruby/gems")));
            }
        }
        gem_paths.dedup();
        Self {
            cargo_home: env("CARGO_HOME").or_else(|| home.as_ref().map(|home| home.join(".cargo"))),
            go_mod_cache: env("GOMODCACHE").or_else(|| gopath.map(|path| path.join("pkg/mod"))),
            maven_repo: home.as_ref().map(|home| home.join(".m2/repository")),
            gradle_home: env("GRADLE_USER_HOME")
                .or_else(|| home.as_ref().map(|home| home.join(".gradle"))),
            gem_paths,
            nuget_packages: env("NUGET_PACKAGES")
                .or_else(|| home.as_ref().map(|home| home.join(".nuget/packages"))),
        }
    }

    /// Whether canonical `base` lies in a dependency cache: the strict-policy read grant.
    fn contain(&self, base: &Path) -> bool {
        let cargo = self
            .cargo_home
            .iter()
            .flat_map(|home| [home.join("registry/src"), home.join("git/checkouts")]);
        let gradle = self
            .gradle_home
            .iter()
            .map(|home| home.join(jvm::GRADLE_FILES));
        let gems = self
            .gem_paths
            .iter()
            .flat_map(|path| [path.join("gems"), path.join("bundler/gems")]);
        cargo
            .chain(self.go_mod_cache.iter().cloned())
            .chain(self.maven_repo.iter().cloned())
            .chain(gradle)
            .chain(gems)
            .chain(self.nuget_packages.iter().cloned())
            .filter_map(|cache| cache.canonicalize().ok())
            .any(|cache| base.starts_with(cache))
    }
}

/// A resolved dependency. `root` (a directory, or one module file) must lie inside `base`, the
/// cache or install directory it was resolved from.
#[derive(Debug)]
struct Package {
    ecosystem: Ecosystem,
    name: String,
    version: String,
    root: PathBuf,
    base: PathBuf,
    how: String,
    others: Vec<String>,
}

pub(crate) fn run(ws: &Workspace, args: &Value, caches: &Caches) -> Result<String, String> {
    let name = req_str(args, "name")?.trim();
    let (name, wanted) = split_version(name);
    validate_name(name)?;
    let query = opt_str(args, "query")?.filter(|query| !query.is_empty());
    let path = opt_str(args, "path")?
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let package = match opt_str(args, "ecosystem")? {
        Some(ecosystem) => resolve(
            ws.root(),
            Ecosystem::parse(ecosystem)?,
            name,
            wanted,
            caches,
        )?,
        None => infer(ws.root(), name, wanted, caches)?,
    };
    let root = authorize(ws, &package, caches)?;
    let mut out = format!(
        "{} {} {} (resolved from {})\nroot: {}\n",
        package.ecosystem.label(),
        package.name,
        package.version,
        package.how,
        root.display()
    );
    if !package.others.is_empty() {
        let _ = writeln!(out, "other locked versions: {}", package.others.join(", "));
    }
    if package.ecosystem == Ecosystem::Jvm {
        // A JVM package is its `-sources.jar`, read in place.
        let (inner, range) = match path {
            Some(path) => {
                let (path, range) = split_range(path)?;
                (Some(path), range)
            }
            None => (None, None),
        };
        jvm::browse(&root, inner, range, query, &mut out)?;
        return Ok(out);
    }
    let (target, range) = match path {
        Some(path) => {
            let (path, range) = split_range(path)?;
            (inside(&root, path)?, range)
        }
        None => (root.clone(), None),
    };
    match query {
        Some(query) => search(&root, &target, query, &mut out),
        None if target.is_dir() => list(&root, &target, &mut out),
        None if path.is_some() => excerpt(&root, &target, range, &mut out)?,
        None => {}
    }
    Ok(out)
}

/// `name@version` (a leading `@` is an npm scope, not a version).
fn split_version(name: &str) -> (&str, Option<&str>) {
    match name.rfind('@') {
        Some(at) if at > 0 => (&name[..at], Some(&name[at + 1..]).filter(|v| !v.is_empty())),
        _ => (name, None),
    }
}

/// Package names become path components, so only plain names (one npm scope, slash-separated
/// Go/Composer paths, or one JVM `group:artifact`) are accepted.
fn validate_name(name: &str) -> Result<(), String> {
    let plain = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+'))
    };
    let ok = if let Some(scoped) = name.strip_prefix('@') {
        scoped
            .split_once('/')
            .is_some_and(|(scope, rest)| plain(scope) && plain(rest))
    } else if let Some((group, artifact)) = name.split_once(':') {
        // JVM coordinates: the group's dot segments become directories.
        plain(artifact) && plain(group) && group.split('.').all(plain)
    } else {
        // Go module paths have several segments; none may be empty or a dot segment.
        name.split('/').all(plain)
    };
    if ok {
        Ok(())
    } else {
        Err(format!("invalid package name {name:?}"))
    }
}

fn infer(
    root: &Path,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    // `group:artifact` names only exist on the JVM.
    if name.contains(':') {
        return resolve(root, Ecosystem::Jvm, name, wanted, caches);
    }
    let present = Ecosystem::present(root);
    if present.is_empty() {
        return Err(
            "no Cargo, npm, Python, Go, Gradle/Maven, Ruby, Composer or .NET project in the workspace root; pass `ecosystem`"
                .into(),
        );
    }
    let mut failures = Vec::new();
    for ecosystem in present {
        match resolve(root, ecosystem, name, wanted, caches) {
            Ok(package) => return Ok(package),
            Err(error) => failures.push(format!("{}: {error}", ecosystem.label())),
        }
    }
    Err(failures.join("; "))
}

fn resolve(
    root: &Path,
    ecosystem: Ecosystem,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    if name.contains(':') && ecosystem != Ecosystem::Jvm {
        return Err(format!(
            "{name:?} is a JVM group:artifact name; {} packages have no `:`",
            ecosystem.label()
        ));
    }
    match ecosystem {
        Ecosystem::Cargo => cargo(root, name, wanted, caches),
        Ecosystem::Npm => npm(root, name, wanted),
        Ecosystem::Python => python(root, name, wanted),
        Ecosystem::Go => go(root, name, wanted, caches),
        Ecosystem::Jvm => jvm::resolve(root, name, wanted, caches),
        Ecosystem::Ruby => ruby::resolve(root, name, wanted, caches),
        Ecosystem::Composer => composer::resolve(root, name, wanted),
        Ecosystem::NuGet => nuget::resolve(root, name, wanted, caches),
    }
}

/// Numeric-aware version order: `1.10.0` > `1.9.3`.
fn version_cmp(a: &str, b: &str) -> Ordering {
    let parts = |v: &str| -> Vec<(u64, String)> {
        v.split(['.', '-', '+'])
            .map(|part| (part.parse().unwrap_or(0), part.to_owned()))
            .collect()
    };
    parts(a).cmp(&parts(b))
}

fn cargo(
    root: &Path,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    let text = fs::read_to_string(root.join("Cargo.lock")).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            "no Cargo.lock in the workspace root (`cargo generate-lockfile --offline` creates one)"
                .to_owned()
        }
        _ => format!("cannot read Cargo.lock: {error}"),
    })?;
    let lock: toml::Value =
        toml::from_str(&text).map_err(|error| format!("Cargo.lock does not parse: {error}"))?;
    let same = |a: &str| a.replace('-', "_") == name.replace('-', "_");
    let mut locked: Vec<(String, String, Option<String>)> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let field = |key: &str| entry.get(key).and_then(toml::Value::as_str);
            let package = field("name")?;
            same(package).then(|| {
                (
                    package.to_owned(),
                    field("version").unwrap_or_default().to_owned(),
                    field("source").map(str::to_owned),
                )
            })
        })
        .collect();
    locked.sort_by(|a, b| version_cmp(&b.1, &a.1));
    let index = match wanted {
        Some(version) => locked.iter().position(|(_, v, _)| v == version),
        None => (!locked.is_empty()).then_some(0),
    };
    let Some(index) = index else {
        let versions: Vec<&str> = locked.iter().map(|(_, v, _)| v.as_str()).collect();
        return Err(if versions.is_empty() {
            format!("{name} is not in Cargo.lock")
        } else {
            format!(
                "{name}@{} is not locked; locked versions: {}",
                wanted.unwrap_or_default(),
                versions.join(", ")
            )
        });
    };
    let (package, version, source) = locked.remove(index);
    let others = locked.into_iter().map(|(_, v, _)| v).collect();
    let Some(source) = source else {
        return Err(format!(
            "{package} {version} is a workspace or path dependency; read it with `read`"
        ));
    };
    let home = caches
        .cargo_home
        .as_ref()
        .ok_or("CARGO_HOME and HOME are unset")?;
    let (base, found) = if source.starts_with("registry+") || source.starts_with("sparse+") {
        let base = home.join("registry/src");
        let leaf = format!("{package}-{version}");
        let found = subdirs(&base)
            .into_iter()
            .map(|index| index.join(&leaf))
            .find(|dir| dir.join("Cargo.toml").is_file());
        (base, found)
    } else if let Some(rest) = source.strip_prefix("git+") {
        let base = home.join("git/checkouts");
        let revision = rest.rsplit_once('#').map_or("", |(_, rev)| rev);
        (base.clone(), git_checkout(&base, revision, &package))
    } else {
        return Err(format!(
            "{package} {version} has an unsupported source {source}"
        ));
    };
    let root = found.ok_or_else(|| {
        format!(
            "{package} {version} is locked but its source is not in {} (offline `cargo fetch` downloads it; nothing is fetched here)",
            base.display()
        )
    })?;
    Ok(Package {
        ecosystem: Ecosystem::Cargo,
        name: package,
        version,
        root,
        base,
        how: format!("Cargo.lock, {source}"),
        others,
    })
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// The checkout of `revision` holding the crate `package` (crates may sit in subdirectories).
fn git_checkout(base: &Path, revision: &str, package: &str) -> Option<PathBuf> {
    if revision.len() < 7 {
        return None;
    }
    let declares = |dir: &Path| {
        fs::read_to_string(dir.join("Cargo.toml"))
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
            .is_some_and(|manifest| {
                manifest
                    .get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(toml::Value::as_str)
                    == Some(package)
            })
    };
    for repository in subdirs(base) {
        for checkout in subdirs(&repository) {
            let name = checkout.file_name()?.to_string_lossy().into_owned();
            if !revision.starts_with(&name) || name.len() < 7 {
                continue;
            }
            let mut frontier = vec![checkout];
            for _ in 0..3 {
                if let Some(found) = frontier.iter().find(|dir| declares(dir)) {
                    return Some(found.clone());
                }
                frontier = frontier
                    .iter()
                    .flat_map(|dir| subdirs(dir))
                    .filter(|dir| !dir.ends_with(".git") && !dir.ends_with("target"))
                    .collect();
            }
        }
    }
    None
}

fn npm(root: &Path, name: &str, wanted: Option<&str>) -> Result<Package, String> {
    let base = root.join("node_modules");
    let dir = base.join(name);
    let manifest =
        fs::read_to_string(dir.join("package.json")).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => format!(
                "{name} is not installed in node_modules (install it with the project's package manager; nothing is fetched here)"
            ),
            _ => format!("cannot read node_modules/{name}/package.json: {error}"),
        })?;
    let manifest: Value = serde_json::from_str(&manifest)
        .map_err(|error| format!("node_modules/{name}/package.json does not parse: {error}"))?;
    let version = manifest["version"].as_str().unwrap_or("unknown").to_owned();
    if let Some(wanted) = wanted.filter(|wanted| *wanted != version) {
        return Err(format!(
            "{name}@{wanted} is not installed; node_modules has {version}"
        ));
    }
    Ok(Package {
        ecosystem: Ecosystem::Npm,
        name: name.to_owned(),
        version,
        root: dir,
        base,
        how: format!("node_modules/{name}/package.json"),
        others: Vec::new(),
    })
}

/// PEP 503 normalization: case-insensitive, runs of `-`, `_` and `.` are equal.
fn normalize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('_') {
                out.push('_');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// `site-packages` directories of the project's own virtualenvs.
fn site_packages(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for venv in [".venv", "venv", "env"] {
        let windows = root.join(venv).join("Lib/site-packages");
        if windows.is_dir() {
            found.push((format!("{venv}/Lib/site-packages"), windows));
        }
        for lib in subdirs(&root.join(venv).join("lib")) {
            let site = lib.join("site-packages");
            if site.is_dir() {
                let python = lib.file_name().unwrap_or_default().to_string_lossy();
                found.push((format!("{venv}/lib/{python}/site-packages"), site));
            }
        }
    }
    found
}

fn python(root: &Path, name: &str, wanted: Option<&str>) -> Result<Package, String> {
    let sites = site_packages(root);
    if sites.is_empty() {
        return Err(
            "no .venv, venv or env virtualenv with site-packages in the workspace root".into(),
        );
    }
    let wanted_name = normalize(name);
    for (label, site) in &sites {
        let dists: Vec<(PathBuf, String, String)> = fs::read_dir(site)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name().to_string_lossy().into_owned();
                let stem = file.strip_suffix(".dist-info")?;
                let (dist, version) = stem.split_once('-')?;
                Some((entry.path(), dist.to_owned(), version.to_owned()))
            })
            .collect();
        let top_level = |info: &Path| -> Vec<String> {
            fs::read_to_string(info.join("top_level.txt"))
                .map(|text| {
                    text.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let module_at = |module: &str| {
            let dir = site.join(module);
            let file = site.join(format!("{module}.py"));
            if dir.is_dir() {
                Some(dir)
            } else {
                file.is_file().then_some(file)
            }
        };
        // A distribution name first (`PyYAML`), then an import name (`yaml`).
        let by_dist = dists
            .iter()
            .find(|(_, dist, _)| normalize(dist) == wanted_name);
        let resolved = match by_dist {
            Some((info, dist, version)) => {
                let modules = top_level(info);
                let candidates = modules
                    .iter()
                    .map(String::as_str)
                    .chain([wanted_name.as_str(), name]);
                candidates.filter_map(module_at).next().map(|module| {
                    let how = format!(
                        "{label}/{}",
                        info.file_name().unwrap_or_default().to_string_lossy()
                    );
                    (dist.clone(), version.clone(), module, how)
                })
            }
            None => module_at(name).map(|module| {
                let owner = dists
                    .iter()
                    .find(|(info, _, _)| top_level(info).iter().any(|m| m == name));
                match owner {
                    Some((info, dist, version)) => (
                        dist.clone(),
                        version.clone(),
                        module,
                        format!(
                            "{label}/{}",
                            info.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    ),
                    None => (
                        name.to_owned(),
                        "unknown".to_owned(),
                        module,
                        format!("{label}/{name}"),
                    ),
                }
            }),
        };
        if let Some((dist, version, module, how)) = resolved {
            if let Some(wanted) = wanted.filter(|wanted| *wanted != version) {
                return Err(format!(
                    "{name}=={wanted} is not installed; {label} has {version}"
                ));
            }
            return Ok(Package {
                ecosystem: Ecosystem::Python,
                name: dist,
                version,
                root: module,
                base: site.clone(),
                how,
                others: Vec::new(),
            });
        }
    }
    let searched: Vec<&str> = sites.iter().map(|(label, _)| label.as_str()).collect();
    Err(format!(
        "{name} is not installed in {} (nothing is installed or fetched here)",
        searched.join(", ")
    ))
}

/// Module cache path escaping: each upper-case letter becomes `!` plus its lower case.
fn go_escape(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `(module, version)` pairs required by go.mod, plus `replace` directives as (from, to, version).
#[allow(clippy::type_complexity)]
fn go_mod(text: &str) -> (Vec<(String, String)>, Vec<(String, String, Option<String>)>) {
    let mut requires = Vec::new();
    let mut replaces = Vec::new();
    let mut block: Option<&str> = None;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let body = match block {
            Some(_) if line == ")" => {
                block = None;
                continue;
            }
            Some(kind) => Some((kind, line)),
            None => {
                let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
                let rest = rest.trim();
                match (keyword, rest) {
                    ("require" | "replace", "(") => {
                        block = Some(if keyword == "require" {
                            "require"
                        } else {
                            "replace"
                        });
                        None
                    }
                    ("require", rest) => Some(("require", rest)),
                    ("replace", rest) => Some(("replace", rest)),
                    _ => None,
                }
            }
        };
        let Some((kind, body)) = body else {
            continue;
        };
        if kind == "require" {
            let mut words = body.split_whitespace();
            if let (Some(module), Some(version)) = (words.next(), words.next()) {
                requires.push((module.to_owned(), version.to_owned()));
            }
        } else if let Some((from, to)) = body.split_once("=>") {
            let from = from
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            let mut to = to.split_whitespace();
            if let Some(target) = to.next() {
                replaces.push((from, target.to_owned(), to.next().map(str::to_owned)));
            }
        }
    }
    (requires, replaces)
}

fn go(root: &Path, name: &str, wanted: Option<&str>, caches: &Caches) -> Result<Package, String> {
    let text = fs::read_to_string(root.join("go.mod"))
        .map_err(|_| "no go.mod in the workspace root".to_owned())?;
    let (requires, replaces) = go_mod(&text);
    let matches: Vec<&(String, String)> = match requires.iter().find(|(module, _)| module == name) {
        Some(exact) => vec![exact],
        None => requires
            .iter()
            .filter(|(module, _)| module.rsplit('/').next() == Some(name))
            .collect(),
    };
    let (module, version) = match matches.as_slice() {
        [one] => (one.0.clone(), one.1.clone()),
        [] => return Err(format!("{name} is not required in go.mod")),
        many => {
            let names: Vec<&str> = many.iter().map(|(module, _)| module.as_str()).collect();
            return Err(format!(
                "{name} is ambiguous; use the module path: {}",
                names.join(", ")
            ));
        }
    };
    if let Some(wanted) = wanted.filter(|wanted| *wanted != version) {
        return Err(format!(
            "{module}@{wanted} is not required; go.mod has {version}"
        ));
    }
    let vendored = root.join("vendor").join(&module);
    if vendored.is_dir() {
        return Ok(Package {
            ecosystem: Ecosystem::Go,
            name: module.clone(),
            version,
            root: vendored,
            base: root.join("vendor"),
            how: format!("go.mod, vendor/{module}"),
            others: Vec::new(),
        });
    }
    let (source, source_version, how) = match replaces.iter().find(|(from, _, _)| *from == module) {
        Some((_, target, _)) if target.starts_with('.') || target.starts_with('/') => {
            return Err(format!(
                "{module} is replaced by the local directory {target}; read it with `read`"
            ));
        }
        Some((_, target, Some(replacement))) => (
            target.clone(),
            replacement.clone(),
            format!("go.mod replace {module} => {target} {replacement}"),
        ),
        _ => (module.clone(), version.clone(), "go.mod".to_owned()),
    };
    let base = caches
        .go_mod_cache
        .clone()
        .ok_or("GOMODCACHE, GOPATH and HOME are unset")?;
    let dir = base.join(format!(
        "{}@{}",
        go_escape(&source),
        go_escape(&source_version)
    ));
    if !dir.is_dir() {
        return Err(format!(
            "{source}@{source_version} is required but not in the module cache {} (`go mod download` fetches it; nothing is fetched here)",
            base.display()
        ));
    }
    Ok(Package {
        ecosystem: Ecosystem::Go,
        name: module,
        version: source_version,
        root: dir,
        base,
        how,
        others: Vec::new(),
    })
}

/// The canonical package root, once the read is within Dial's read authority (module docs).
fn authorize(ws: &Workspace, package: &Package, caches: &Caches) -> Result<PathBuf, String> {
    let canonical = |path: &Path| {
        path.canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    let base = canonical(&package.base)?;
    let root = canonical(&package.root)?;
    if !root.starts_with(&base) {
        return Err(format!(
            "{} resolves to {}, outside {}; refusing to follow it",
            package.root.display(),
            root.display(),
            base.display()
        ));
    }
    if ws.is_protected(&root) {
        return Err(format!("{} is in protected Dial storage", root.display()));
    }
    if ws.roots().any(|workspace| root.starts_with(workspace)) {
        return Ok(root);
    }
    let (deny_read, granted) = match ws.sandbox_policy() {
        Policy::Dev { deny_read, .. } => (deny_read, true),
        Policy::Strict { deny_read, .. } => (deny_read, caches.contain(&base)),
    };
    let denied = deny_read.iter().any(|denied| {
        let denied = denied.canonicalize().unwrap_or_else(|_| denied.clone());
        root.starts_with(&denied) || denied.starts_with(&root)
    });
    if denied {
        Err(format!(
            "{} is in a path the sandbox denies reading",
            root.display()
        ))
    } else if granted {
        Ok(root)
    } else {
        Err(format!(
            "{} is outside the workspace and the strict sandbox grants no reads there; call `ask` action=directory with path {:?} for a yes/no approval, then retry",
            root.display(),
            base.display()
        ))
    }
}

/// Inclusive 1-based line range.
type LineRange = (usize, usize);

/// `path:N-M` or `path:N` (N to N+199).
fn split_range(input: &str) -> Result<(&str, Option<LineRange>), String> {
    let Some((path, selector)) = input.rsplit_once(':') else {
        return Ok((input, None));
    };
    let number = |text: &str| text.parse::<usize>().ok().filter(|n| *n > 0);
    let range = match selector.split_once('-') {
        Some((start, end)) => number(start).zip(number(end)),
        None => number(selector).map(|start| (start, start + EXCERPT_LINES - 1)),
    };
    match range {
        Some((start, end)) if start <= end => Ok((path, Some((start, end)))),
        Some(_) => Err(format!("line range in {input:?} ends before it starts")),
        None => Ok((input, None)),
    }
}

/// `input` resolved inside `root`; anything that would leave it is refused.
fn inside(root: &Path, input: &str) -> Result<PathBuf, String> {
    let relative = Path::new(input);
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "path {input:?} must be relative to the package root, without `..`"
        ));
    }
    if root.is_file() {
        // A single-module package: only that file is readable.
        let own = root
            .file_name()
            .is_some_and(|name| relative == Path::new(name));
        return if own {
            Ok(root.to_path_buf())
        } else {
            Err(format!(
                "this package is the single file {}",
                root.display()
            ))
        };
    }
    let target = root
        .join(relative)
        .canonicalize()
        .map_err(|_| format!("{input}: no such file or directory in the package"))?;
    if !target.starts_with(root) {
        return Err(format!("path {input:?} escapes the package root"));
    }
    Ok(target)
}

fn relative(root: &Path, path: &Path) -> String {
    let rest = if root.is_file() {
        path.file_name().map(Path::new).unwrap_or(path)
    } else {
        path.strip_prefix(root).unwrap_or(path)
    };
    let text = rest.to_string_lossy();
    if text.is_empty() {
        ".".into()
    } else {
        text.into_owned()
    }
}

fn list(root: &Path, dir: &Path, out: &mut String) {
    let mut entries: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => format!("{name}/"),
                Ok(kind) if kind.is_symlink() => format!("{name}@"),
                _ => name,
            }
        })
        .collect();
    entries.sort();
    let _ = writeln!(out, "{}: {} entries", relative(root, dir), entries.len());
    for entry in entries.iter().take(MAX_LISTING) {
        let _ = writeln!(out, "  {entry}");
    }
    if entries.len() > MAX_LISTING {
        let _ = writeln!(out, "  ... {} more", entries.len() - MAX_LISTING);
    }
}

/// Text of a file, or `None` for binary or oversized files.
fn text_of(path: &Path, limit: u64) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn clip(line: &str) -> &str {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => &line[..end],
        None => line,
    }
}

fn excerpt(
    root: &Path,
    file: &Path,
    range: Option<LineRange>,
    out: &mut String,
) -> Result<(), String> {
    let rel = relative(root, file);
    let text = text_of(file, MAX_READ_FILE)
        .ok_or_else(|| format!("{rel} is binary, not a regular file, or larger than 8 MiB"))?;
    excerpt_text(&rel, &text, range, out)
}

/// Numbered lines of `text` (shown as `rel`) within `range`, with a footer naming the next range.
fn excerpt_text(
    rel: &str,
    text: &str,
    range: Option<LineRange>,
    out: &mut String,
) -> Result<(), String> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = range.unwrap_or((1, EXCERPT_LINES));
    let end = end.min(start + MAX_EXCERPT_LINES - 1).min(lines.len());
    if start > lines.len() {
        return Err(format!("{rel} has {} lines", lines.len()));
    }
    let mut shown = start - 1;
    for (number, line) in lines[start - 1..end].iter().enumerate() {
        if out.len() > MAX_OUTPUT {
            break;
        }
        let _ = writeln!(out, "{}:{}", start + number, line);
        shown = start + number;
    }
    let _ = write!(out, "[{rel} lines {start}-{shown} of {}", lines.len());
    if shown < lines.len() {
        let _ = write!(
            out,
            "; next: path {rel}:{}-{}",
            shown + 1,
            shown + EXCERPT_LINES
        );
    }
    out.push_str("]\n");
    Ok(())
}

fn search(root: &Path, start: &Path, query: &str, out: &mut String) {
    let mut walker = ignore::WalkBuilder::new(start);
    walker
        .standard_filters(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIPPED_DIRS.contains(&name))
        });
    let mut tally = SearchTally::default();
    for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !tally.admit_file() {
            break;
        }
        let Some(text) = text_of(entry.path(), MAX_SEARCH_FILE) else {
            continue;
        };
        if tally.lines(&relative(root, entry.path()), &text, query, out) {
            break;
        }
    }
    tally.finish(query, out);
}

/// Running counts of one package search, shared by directory and jar searches.
#[derive(Default)]
struct SearchTally {
    scanned: usize,
    matches: usize,
    capped: bool,
}

impl SearchTally {
    /// Counts one more file; false once the scan limit is reached.
    fn admit_file(&mut self) -> bool {
        self.scanned += 1;
        if self.scanned > MAX_SCANNED_FILES {
            self.capped = true;
        }
        !self.capped
    }

    /// Writes the lines of `text` containing `query`; true once the match or output cap is hit.
    fn lines(&mut self, rel: &str, text: &str, query: &str, out: &mut String) -> bool {
        for (number, line) in text.lines().enumerate() {
            if !line.contains(query) {
                continue;
            }
            self.matches += 1;
            if self.matches > MAX_MATCHES || out.len() > MAX_OUTPUT {
                self.capped = true;
                return true;
            }
            let _ = writeln!(out, "{rel}:{}: {}", number + 1, clip(line.trim()));
        }
        false
    }

    fn finish(&self, query: &str, out: &mut String) {
        match (self.matches, self.capped) {
            (0, _) => {
                let _ = writeln!(
                    out,
                    "no matches for {query:?} in {} files",
                    self.scanned.min(MAX_SCANNED_FILES)
                );
            }
            (_, true) => {
                let _ = writeln!(
                    out,
                    "[first {MAX_MATCHES} matches or output limit; narrow `query` or `path`]"
                );
            }
            _ => {}
        }
    }
}

mod composer;
mod jvm;
mod nuget;
mod ruby;

#[cfg(all(test, unix))]
mod tests;
