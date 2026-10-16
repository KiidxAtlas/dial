//! Which language servers handle which file, and where each one is rooted.
//!
//! Built-in defaults (`defaults.json`) are tried in order. Each names a
//! command, the file types it serves and the root markers identifying its project. A server only
//! applies to a file when one of its markers is found in the file's directory or an ancestor; the
//! nearest such directory inside the workspace is the server's root, so monorepo packages each get
//! their own session. Commands resolve from project-local bin directories (`node_modules/.bin`,
//! `.venv/bin`, ...) before `PATH`.
//!
//! `[lsp.<name>]` entries override the default of the same name field by field, or add a server.
//! A configured server claiming an extension replaces every default for that extension. Several
//! servers may serve one file: primary servers first, linters last.

use super::config::{LspConfig, LspServerConfig};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Builtin {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    file_types: Vec<String>,
    #[serde(default)]
    root_markers: Vec<String>,
    #[serde(default)]
    is_linter: bool,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    init_options: Option<Value>,
}

static DEFAULTS: LazyLock<Vec<Builtin>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("defaults.json")).expect("bundled LSP defaults are valid")
});

/// Where a missing default server comes from.
fn install_hint(command: &str) -> &'static str {
    match command {
        "rust-analyzer" => "run `rustup component add rust-analyzer`",
        "typescript-language-server" => {
            "run `npm install -g typescript-language-server typescript`"
        }
        "pyright-langserver" => "run `npm install -g pyright`",
        "basedpyright-langserver" => "run `pip install basedpyright`",
        "pylsp" => "run `pip install python-lsp-server`",
        "gopls" => "run `go install golang.org/x/tools/gopls@latest`",
        "clangd" => "install clangd (it ships with LLVM and the Xcode command line tools)",
        "lua-language-server" => "install lua-language-server",
        "zls" => "install zls",
        _ => "install it",
    }
}

const PYTHON_MARKERS: &[&str] = &[
    "pyproject.toml",
    "ty.toml",
    "requirements.txt",
    "setup.py",
    "setup.cfg",
    "Pipfile",
    "pyrightconfig.json",
    "ruff.toml",
    ".ruff.toml",
];

/// Project-local bin directories checked before `PATH`, gated by the project's markers.
const LOCAL_BINS: &[(&[&str], &str)] = &[
    (
        &[
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
        ],
        "node_modules/.bin",
    ),
    (PYTHON_MARKERS, ".venv/bin"),
    (PYTHON_MARKERS, "venv/bin"),
    (PYTHON_MARKERS, ".env/bin"),
    (&["Gemfile", "Gemfile.lock"], "vendor/bundle/bin"),
    (&["Gemfile", "Gemfile.lock"], "bin"),
    (&["go.mod", "go.sum", "go.work"], "bin"),
];

/// LSP `languageId` for a file extension; `fallback` (the server's name) otherwise.
pub fn language_id<'a>(extension: &str, fallback: &'a str) -> &'a str {
    match extension {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "mod" => "go.mod",
        "c" => "c",
        "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" | "cu" | "cuh" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "lua" => "lua",
        "zig" => "zig",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" | "sbt" => "scala",
        "hs" | "lhs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "gleam" => "gleam",
        "rb" | "rake" | "gemspec" => "ruby",
        "erb" => "erb",
        "sh" | "bash" | "zsh" => "shellscript",
        "php" => "php",
        "cs" | "csx" => "csharp",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "jsonc" => "jsonc",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "tf" | "tfvars" => "terraform",
        "nix" => "nix",
        "odin" => "odin",
        "dart" => "dart",
        "md" | "markdown" => "markdown",
        "tex" | "bib" => "latex",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "vim" => "vim",
        "swift" => "swift",
        "tla" => "tlaplus",
        _ => fallback,
    }
}

#[derive(Clone)]
struct Entry {
    name: String,
    program: PathBuf,
    args: Vec<String>,
    /// Lower-case extensions without the dot, or exact lower-case file names.
    file_types: Vec<String>,
    root_markers: Vec<String>,
    linter: bool,
    settings: Value,
    init_options: Value,
    language_id: Option<String>,
    configured: bool,
}

impl Entry {
    fn serves(&self, extension: Option<&str>, file_name: &str) -> bool {
        self.file_types
            .iter()
            .any(|kind| Some(kind.as_str()) == extension || kind == file_name)
    }
}

/// A server ready to start for one project root.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// Server name (`rust-analyzer`, `pyright`, or the `[lsp.<name>]` key).
    pub language: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Nearest project root for the file, always inside the workspace.
    pub root: PathBuf,
    pub linter: bool,
    pub settings: Value,
    pub init_options: Value,
    pub language_id: Option<String>,
}

impl Resolved {
    /// The `languageId` for a document of `path`.
    pub fn language_id_for(&self, path: &Path) -> String {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .map(normalize)
            .unwrap_or_default();
        self.language_id
            .clone()
            .unwrap_or_else(|| language_id(&extension, &self.language).to_owned())
    }
}

#[derive(Clone)]
pub struct Servers {
    /// Configured entries first, then defaults; order breaks ties between primary servers.
    entries: Vec<Entry>,
}

fn normalize(kind: &str) -> String {
    kind.trim_start_matches('.').to_ascii_lowercase()
}

pub fn extension_of(file: &str) -> Option<String> {
    Path::new(file)
        .extension()
        .and_then(OsStr::to_str)
        .map(normalize)
}

fn file_name_of(file: &str) -> String {
    Path::new(file)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Directories of the `PATH` environment variable, then the bin directories of programs Dial
/// installed itself (`<user_dir>/tools/*/bin` and `node_modules/.bin`), so an installed server is
/// found without being on the user's `PATH`.
pub fn path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    dirs.extend(installed_dirs(&crate::config::types::default_user_dir()));
    dirs
}

/// Bin directories under Dial's install root for `user_dir`; empty when nothing is installed.
fn installed_dirs(user_dir: &Path) -> Vec<PathBuf> {
    if user_dir.as_os_str().is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(user_dir.join("tools")) else {
        return Vec::new();
    };
    let mut tools: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    tools.sort();
    tools
        .into_iter()
        .flat_map(|tool| [tool.join("bin"), tool.join("node_modules/.bin")])
        .filter(|dir| dir.is_dir())
        .collect()
}

fn is_executable(path: &Path) -> bool {
    crate::platform::fs::is_executable(path)
}

/// `*` matches any run of characters, `?` one; used for root markers such as `*.cabal`.
fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whether `dir` directly contains any of `markers`.
pub fn has_markers<S: AsRef<str>>(dir: &Path, markers: &[S]) -> bool {
    let mut entries: Option<Vec<String>> = None;
    markers.iter().any(|marker| {
        let marker = marker.as_ref();
        if marker.contains(['*', '?']) {
            let names = entries.get_or_insert_with(|| {
                std::fs::read_dir(dir)
                    .map(|iter| {
                        iter.flatten()
                            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            });
            names.iter().any(|name| wildcard(marker, name))
        } else {
            dir.join(marker).exists()
        }
    })
}

/// The nearest directory between `file` and `ws_root` holding one of `markers`. A marker found
/// only above the workspace roots the server at the workspace itself. `None`: not this project.
pub fn project_root(ws_root: &Path, file: &Path, markers: &[String]) -> Option<PathBuf> {
    if markers.is_empty() {
        return Some(ws_root.to_owned());
    }
    let mut dir = file.parent()?;
    while dir.starts_with(ws_root) {
        if has_markers(dir, markers) {
            return Some(dir.to_owned());
        }
        if dir == ws_root {
            break;
        }
        dir = dir.parent()?;
    }
    ws_root
        .ancestors()
        .skip(1)
        .any(|dir| has_markers(dir, markers))
        .then(|| ws_root.to_owned())
}

/// Local bins of `roots` (gated by their markers), then `dirs`. A program with a path separator
/// is taken as is (relative to the workspace root).
fn find_program(
    program: &Path,
    roots: &[&Path],
    ws_root: &Path,
    dirs: &[PathBuf],
) -> Option<PathBuf> {
    if program.components().count() > 1 {
        let path = if program.is_relative() {
            ws_root.join(program)
        } else {
            program.to_owned()
        };
        return is_executable(&path).then_some(path);
    }
    let mut seen: Vec<&Path> = Vec::new();
    for root in roots {
        if seen.contains(root) {
            continue;
        }
        seen.push(root);
        for (markers, bin) in LOCAL_BINS {
            if has_markers(root, markers) {
                let candidate = root.join(bin).join(program);
                if is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    dirs.iter()
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// TypeScript 7's native `tsc` speaks LSP and no longer ships `lib/tsserver.js`, which
/// typescript-language-server needs; older releases reject `--lsp`.
fn typescript_speaks_lsp(tsc: &Path) -> bool {
    let real = std::fs::canonicalize(tsc).unwrap_or_else(|_| tsc.to_owned());
    let mut candidates = Vec::new();
    if real.parent().and_then(Path::file_name) == Some(OsStr::new("bin")) {
        candidates.extend(real.parent().and_then(Path::parent).map(Path::to_owned));
    }
    if let Some(bin) = tsc.parent() {
        candidates.push(bin.join("..").join("typescript"));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("package.json").is_file())
        .is_some_and(|dir| !dir.join("lib").join("tsserver.js").exists())
}

fn merge(name: &str, default: Option<&Builtin>, config: &LspServerConfig) -> Option<Entry> {
    let program = if config.program.as_os_str().is_empty() {
        PathBuf::from(&default?.command)
    } else {
        config.program.clone()
    };
    let file_types = match (&config.extensions, default) {
        (Some(extensions), _) => extensions.iter().map(|e| normalize(e)).collect(),
        (None, Some(default)) => default.file_types.iter().map(|e| normalize(e)).collect(),
        (None, None) => return None,
    };
    Some(Entry {
        name: name.to_owned(),
        program,
        args: config
            .args
            .clone()
            .or_else(|| default.map(|d| d.args.clone()))
            .unwrap_or_default(),
        file_types,
        root_markers: config
            .root_markers
            .clone()
            .or_else(|| default.map(|d| d.root_markers.clone()))
            .unwrap_or_default(),
        linter: config
            .linter
            .or(default.map(|d| d.is_linter))
            .unwrap_or(false),
        settings: config
            .settings
            .clone()
            .or_else(|| default.and_then(|d| d.settings.clone()))
            .unwrap_or(Value::Null),
        init_options: config
            .init_options
            .clone()
            .or_else(|| default.and_then(|d| d.init_options.clone()))
            .unwrap_or(Value::Null),
        language_id: config.language_id.clone(),
        configured: true,
    })
}

fn from_default(default: &Builtin) -> Entry {
    Entry {
        name: default.name.clone(),
        program: PathBuf::from(&default.command),
        args: default.args.clone(),
        file_types: default.file_types.iter().map(|e| normalize(e)).collect(),
        root_markers: default.root_markers.clone(),
        linter: default.is_linter,
        settings: default.settings.clone().unwrap_or(Value::Null),
        init_options: default.init_options.clone().unwrap_or(Value::Null),
        language_id: None,
        configured: false,
    }
}

impl Servers {
    pub fn new(config: &LspConfig) -> Self {
        let mut entries: Vec<Entry> = config
            .servers
            .iter()
            .filter(|(_, server)| !server.disabled)
            .filter_map(|(name, server)| {
                merge(name, DEFAULTS.iter().find(|d| &d.name == name), server)
            })
            .collect();
        entries.extend(
            DEFAULTS
                .iter()
                .filter(|d| !config.servers.contains_key(&d.name))
                .map(from_default),
        );
        Self { entries }
    }

    /// Entries serving `file`, configured claims replacing defaults, before any filesystem check.
    fn candidates(&self, file: &str) -> Result<Vec<&Entry>, String> {
        let extension = extension_of(file);
        let name = file_name_of(file);
        let matching: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.serves(extension.as_deref(), &name))
            .collect();
        let claimed = matching.iter().any(|entry| entry.configured);
        let matching: Vec<&Entry> = matching
            .into_iter()
            .filter(|entry| entry.configured || !claimed)
            .collect();
        if matching.is_empty() {
            return Err(match extension {
                None => {
                    format!("cannot pick a language server for {file:?}: it has no file extension")
                }
                Some(extension) => format!(
                    "no language server is configured for .{extension} files; add one in dial's config, \
                     e.g. [lsp.mylang] with program, args and extensions = [\"{extension}\"]"
                ),
            });
        }
        Ok(matching)
    }

    /// Best matching server name without starting a process or requiring a project root.
    pub fn language_for(&self, file: &str) -> Result<String, String> {
        self.candidates(file)?
            .first()
            .map(|entry| entry.name.clone())
            .ok_or_else(|| format!("no language server matches {file:?}"))
    }

    /// Names of servers that could serve `file`, independent of executables or root markers.
    /// Existing-session queries use it without pretending a process vanished.
    pub fn names_for(&self, file: &str) -> Result<Vec<String>, String> {
        Ok(self
            .candidates(file)?
            .into_iter()
            .map(|entry| entry.name.clone())
            .collect())
    }

    /// The `languageId` server `name` sends for `file`, honoring its `language_id` override; `None`
    /// when that server does not serve the file.
    pub fn language_id_of(&self, name: &str, file: &str) -> Option<String> {
        let extension = extension_of(file).unwrap_or_default();
        self.candidates(file)
            .ok()?
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| match &entry.language_id {
                Some(id) => id.clone(),
                None => language_id(&extension, &entry.name).to_owned(),
            })
    }
    /// Every usable server for `abs` (primary servers first, linters last), each rooted at its
    /// nearest project root. Servers whose markers are absent are skipped; an empty result is an
    /// actionable error naming what to install or configure.
    pub fn select_all(
        &self,
        ws_root: &Path,
        abs: &Path,
        dirs: &[PathBuf],
    ) -> Result<Vec<Resolved>, String> {
        let file = abs.to_string_lossy();
        let mut missing = Vec::new();
        let mut unrooted = Vec::new();
        let mut found = Vec::new();
        for entry in self.candidates(&file)? {
            let Some(root) = project_root(ws_root, abs, &entry.root_markers) else {
                unrooted.push(entry);
                continue;
            };
            match find_program(&entry.program, &[&root, ws_root], ws_root, dirs) {
                Some(program) => found.push(Resolved {
                    language: entry.name.clone(),
                    program,
                    args: entry
                        .args
                        .iter()
                        .map(|arg| {
                            if arg == "$PID" {
                                std::process::id().to_string()
                            } else {
                                arg.clone()
                            }
                        })
                        .collect(),
                    root,
                    linter: entry.linter,
                    settings: entry.settings.clone(),
                    init_options: entry.init_options.clone(),
                    language_id: entry.language_id.clone(),
                }),
                None => missing.push(entry),
            }
        }
        if let Some(native) = found.iter().position(|s| s.language == "typescript-native") {
            let drop = if typescript_speaks_lsp(&found[native].program) {
                "typescript-language-server"
            } else {
                "typescript-native"
            };
            found.retain(|server| server.language != drop);
        }
        found.sort_by_key(|server| server.linter);
        if !found.is_empty() {
            return Ok(found);
        }
        if let Some(entry) = missing.iter().find(|entry| entry.configured) {
            return Err(format!(
                "the language server {} is configured as {:?} but that program was not found or is not \
                 executable; fix [lsp.{}] program in dial's config",
                entry.name,
                entry.program.display(),
                entry.name
            ));
        }
        if !missing.is_empty() {
            let names: Vec<String> = missing
                .iter()
                .map(|e| e.program.display().to_string())
                .collect();
            let hints: Vec<String> = missing
                .iter()
                .map(|e| format!("{}: {}", e.name, install_hint(&e.program.to_string_lossy())))
                .collect();
            return Err(format!(
                "no language server for {} found in project bin directories or on PATH (looked for {}); {}; \
                 or point [lsp.<name>] program at one in dial's config",
                file,
                names.join(", "),
                hints.join("; ")
            ));
        }
        let markers: Vec<String> = unrooted
            .iter()
            .map(|e| format!("{} needs one of {}", e.name, e.root_markers.join(", ")))
            .collect();
        Err(format!(
            "no language server applies to {file}: no project root marker was found ({}); add the marker \
             or configure [lsp.<name>] root_markers = []",
            markers.join("; ")
        ))
    }

    /// The primary server for `abs`.
    pub fn select(&self, ws_root: &Path, abs: &Path, dirs: &[PathBuf]) -> Result<Resolved, String> {
        Ok(self.select_all(ws_root, abs, dirs)?.remove(0))
    }

    /// Built-in servers for `abs` that are missing but have a Dial install recipe, in preference
    /// order. Configured servers are never replaced: a wrong `program` is the user's to fix.
    pub fn installable(&self, ws_root: &Path, abs: &Path, dirs: &[PathBuf]) -> Vec<String> {
        let Ok(candidates) = self.candidates(&abs.to_string_lossy()) else {
            return Vec::new();
        };
        candidates
            .into_iter()
            .filter(|entry| !entry.configured && !entry.linter)
            .filter_map(|entry| {
                let root = project_root(ws_root, abs, &entry.root_markers)?;
                find_program(&entry.program, &[&root, ws_root], ws_root, dirs)
                    .is_none()
                    .then(|| entry.program.to_string_lossy().into_owned())
            })
            .filter(|program| crate::setup::deps::recipe(program).is_some())
            .collect()
    }
}

#[cfg(all(test, unix))]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn config(entries: &[(&str, &Path, &[&str])]) -> LspConfig {
        LspConfig {
            servers: entries
                .iter()
                .map(|(name, program, extensions)| {
                    (
                        name.to_string(),
                        LspServerConfig {
                            program: program.to_path_buf(),
                            args: Some(vec!["--flag".into()]),
                            extensions: Some(extensions.iter().map(|e| e.to_string()).collect()),
                            ..LspServerConfig::default()
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            ..LspConfig::default()
        }
    }

    fn project(markers: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for marker in markers {
            fs::write(dir.path().join(marker), "").unwrap();
        }
        dir
    }

    #[test]
    fn installed_dirs_lists_dial_tool_bins_only_when_present() {
        let user = tempfile::tempdir().unwrap();
        assert!(installed_dirs(user.path()).is_empty());
        assert!(installed_dirs(Path::new("")).is_empty());
        let bin = user.path().join("tools/gopls/bin");
        let npm = user.path().join("tools/yaml-language-server/node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&npm).unwrap();
        fs::create_dir_all(user.path().join("tools/empty")).unwrap();
        assert_eq!(installed_dirs(user.path()), vec![bin, npm]);
    }

    #[test]
    fn installable_names_missing_builtin_servers_with_recipes() {
        let ws = project(&["Cargo.toml"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        let servers = Servers::new(&LspConfig::default());
        let file = root.join("src/lib.rs");
        let empty = tempfile::tempdir().unwrap();
        let dirs = [empty.path().to_path_buf()];
        assert_eq!(servers.installable(&root, &file, &dirs), vec!["rust-analyzer"]);
        fake_bin(empty.path(), "rust-analyzer");
        assert!(servers.installable(&root, &file, &dirs).is_empty());
    }

    #[test]
    fn installable_skips_configured_servers() {
        let ws = project(&["Cargo.toml"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        let missing = root.join("nope");
        let servers = Servers::new(&config(&[("rust-analyzer", &missing, &["rs"])]));
        let dirs = [root.clone()];
        assert!(servers.installable(&root, &root.join("lib.rs"), &dirs).is_empty());
    }

    #[test]
    fn bundled_defaults_parse_with_dozens_of_servers() {
        assert!(DEFAULTS.len() >= 50);
        assert!(DEFAULTS.iter().any(|d| d.name == "rust-analyzer" && d.root_markers.contains(&"Cargo.toml".into())));
    }

    #[test]
    fn defaults_need_root_markers_and_the_nearest_root_wins() {
        let bin = tempfile::tempdir().unwrap();
        let ra = fake_bin(bin.path(), "rust-analyzer");
        let ws = project(&["Cargo.toml"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        fs::create_dir_all(root.join("crates/a/src")).unwrap();
        fs::write(root.join("crates/a/Cargo.toml"), "").unwrap();
        let servers = Servers::new(&LspConfig::default());
        let dirs = [bin.path().to_path_buf()];
        let got = servers
            .select(&root, &root.join("crates/a/src/lib.rs"), &dirs)
            .unwrap();
        assert_eq!(
            (got.language.as_str(), got.program, got.root),
            ("rust-analyzer", ra, root.join("crates/a"))
        );
        // A marker above the workspace roots the server there (a workspace nested in a larger
        // project), and temp directories can sit inside a checkout that has a Cargo.toml. Swap in
        // a marker that cannot exist so the unrooted error is checked without ambient ancestors.
        let mut config = LspConfig::default();
        config.servers.insert(
            "rust-analyzer".into(),
            LspServerConfig {
                root_markers: Some(vec!["dial-absent.marker".into()]),
                ..LspServerConfig::default()
            },
        );
        let bare = project(&[]);
        let bare = fs::canonicalize(bare.path()).unwrap();
        let error = Servers::new(&config)
            .select(&bare, &bare.join("main.rs"), &dirs)
            .unwrap_err();
        assert!(
            error.contains("root marker") && error.contains("dial-absent.marker"),
            "{error}"
        );
    }

    #[test]
    fn project_local_bins_beat_path_and_linters_come_last() {
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "pyright-langserver");
        let ws = project(&["pyproject.toml", "ruff.toml"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        fs::create_dir_all(root.join(".venv/bin")).unwrap();
        let local = fake_bin(&root.join(".venv/bin"), "ruff");
        let servers = Servers::new(&LspConfig::default());
        let all = servers
            .select_all(&root, &root.join("a.py"), &[bin.path().to_path_buf()])
            .unwrap();
        assert_eq!(all[0].language, "pyright");
        let last = all.last().unwrap();
        assert!(last.linter && last.program == local, "{all:?}");
    }

    #[test]
    fn missing_default_names_what_to_install() {
        let empty = tempfile::tempdir().unwrap();
        let ws = project(&["Cargo.toml"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        let error = Servers::new(&LspConfig::default())
            .select(&root, &root.join("lib.rs"), &[empty.path().to_path_buf()])
            .unwrap_err();
        assert!(
            error.contains("rust-analyzer") && error.contains("rustup") && error.contains("[lsp."),
            "{error}"
        );
    }

    #[test]
    fn unknown_and_missing_extensions_explain_how_to_configure() {
        let servers = Servers::new(&LspConfig::default());
        assert!(
            servers
                .names_for("a.unknownext")
                .unwrap_err()
                .contains("[lsp.mylang]")
        );
        assert!(
            servers
                .names_for("LICENSE")
                .unwrap_err()
                .contains("no file extension")
        );
    }

    #[test]
    fn configured_extensions_replace_defaults_and_overrides_inherit() {
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "clangd");
        let zls = fake_bin(bin.path(), "my-zls");
        let ws = project(&["Makefile"]);
        let root = fs::canonicalize(ws.path()).unwrap();
        let servers = Servers::new(&config(&[("zig", &zls, &[".zig", "H"])]));
        let dirs = [bin.path().to_path_buf()];
        assert_eq!(
            servers
                .select(&root, &root.join("a.zig"), &dirs)
                .unwrap()
                .language,
            "zig"
        );
        assert_eq!(
            servers
                .select_all(&root, &root.join("a.h"), &dirs)
                .unwrap()
                .len(),
            1,
            "config replaces clangd for .h"
        );
        assert_eq!(
            servers
                .select(&root, &root.join("a.c"), &dirs)
                .unwrap()
                .language,
            "clangd"
        );
        let mut overridden = LspConfig::default();
        overridden.servers.insert(
            "clangd".into(),
            LspServerConfig {
                args: Some(vec![]),
                ..LspServerConfig::default()
            },
        );
        let got = Servers::new(&overridden)
            .select(&root, &root.join("a.c"), &dirs)
            .unwrap();
        assert!(got.args.is_empty() && got.program.ends_with("clangd"));
        overridden.servers.get_mut("clangd").unwrap().disabled = true;
        assert!(
            Servers::new(&overridden)
                .select(&root, &root.join("a.c"), &dirs)
                .is_err()
        );
    }

    #[test]
    fn configured_program_missing_is_a_clear_error() {
        let ws = project(&[]);
        let root = fs::canonicalize(ws.path()).unwrap();
        let servers = Servers::new(&config(&[(
            "zig",
            Path::new("/definitely/not/here/zls"),
            &["zig"],
        )]));
        let error = servers.select(&root, &root.join("a.zig"), &[]).unwrap_err();
        assert!(
            error.contains("/definitely/not/here/zls") && error.contains("[lsp.zig]"),
            "{error}"
        );
    }

    #[test]
    fn wildcard_root_markers_match_one_level() {
        let ws = project(&["demo.cabal"]);
        assert!(has_markers(ws.path(), &["*.cabal"]));
        assert!(!has_markers(ws.path(), &["*.sln"]));
        assert!(wildcard("a*c?", "abbbcd") && !wildcard("a*c", "abd"));
    }

    #[test]
    fn language_ids_follow_the_extension() {
        assert_eq!(language_id("tsx", "typescript"), "typescriptreact");
        assert_eq!(language_id("hpp", "c"), "cpp");
        assert_eq!(language_id("weird", "zig"), "zig");
    }
}
