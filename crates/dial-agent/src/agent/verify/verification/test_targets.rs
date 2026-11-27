//! `verify` targets: files or single tests the model names, mapped to the owning project's runner,
//! and the passed/failed counts a runner reports.
//!
//! An entry is `path` or `path::selector`. The selector is a pytest node (`test_div`,
//! `TestCalc::test_div`), a unittest `Class::method`, a Rust or Go test filter, or a jest/vitest
//! test name. Commands are built by the harness; the path and selector are shell-quoted model input.
use super::{
    FocusedCheck, RustSelection, cargo_check, go_test_command, javascript_test_command,
    javascript_test_runner, normalize, owning_package, owning_target, pytest_configured,
    python_owner, rust_selection, scoped_python, shell_quote,
};
use std::path::{Component, Path};

const MAX_ENTRY_BYTES: usize = 4096;
const JAVASCRIPT_EXTENSIONS: [&str; 8] = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts"];
/// Every target check is named `verify: <entry>`; the controller generated its command.
const NAME_PREFIX: &str = "verify: ";
/// Marks a plain script whose exit status is its only evidence.
const EXIT_ONLY_SUFFIX: &str = " (exit status only)";
/// Cap on files expanded for a directory `node --test` target.
const MAX_DIR_TEST_FILES: usize = 64;
const MAX_DIR_WALK_DEPTH: usize = 6;
const MAX_DIR_WALK_DIRS: usize = 256;
/// Project-level verification checks selectable as `<kind>` or `<dir>:<kind>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectCheck {
    Clippy,
    Fmt,
    Lint,
    Typecheck,
    Check,
    Build,
    Test,
}

impl ProjectCheck {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "clippy" => Some(Self::Clippy),
            "fmt" => Some(Self::Fmt),
            "lint" => Some(Self::Lint),
            "typecheck" => Some(Self::Typecheck),
            "check" => Some(Self::Check),
            "build" => Some(Self::Build),
            "test" => Some(Self::Test),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Clippy => "clippy",
            Self::Fmt => "fmt",
            Self::Lint => "lint",
            Self::Typecheck => "typecheck",
            Self::Check => "check",
            Self::Build => "build",
            Self::Test => "test",
        }
    }

    fn is_behavioral(self) -> bool {
        matches!(self, Self::Test)
    }
}

/// Parse `<kind>` or `<dir>:<kind>` (single colon, never `::`).
fn parse_project_check(entry: &str) -> Option<(Option<&str>, ProjectCheck)> {
    if let Some(kind) = ProjectCheck::parse(entry) {
        return Some((None, kind));
    }
    let (dir, kind_str) = entry.rsplit_once(':')?;
    if dir.is_empty() || dir.ends_with(':') {
        return None;
    }
    let kind = ProjectCheck::parse(kind_str.trim())?;
    Some((Some(dir.trim()), kind))
}

/// Test totals reported by a runner's summary lines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
}

/// A test runner selected for a `verify` entry: behavioral evidence like a configured test.
/// Static project checks (`clippy`, `fmt`, `lint`, `typecheck`, `check`, `build`) are not behavioral.
pub fn behavioral_target(name: &str) -> bool {
    let Some(entry) = name.strip_prefix(NAME_PREFIX) else {
        return false;
    };
    if entry.ends_with(EXIT_ONLY_SUFFIX) {
        return false;
    }
    let base = entry.split_once("::").map_or(entry, |(prefix, _)| prefix);
    parse_project_check(base).is_none_or(|(_, kind)| kind.is_behavioral())
}

/// A plain script run for its exit status: untrusted model code outside any test runner, so it
/// takes the approval path of a `bash` call rather than the generated-check boundary.
pub fn exit_status_target(name: &str) -> bool {
    name.starts_with(NAME_PREFIX) && name.ends_with(EXIT_ONLY_SUFFIX)
}

/// The focused check that runs one `verify` entry, or why it cannot run.
pub fn target_check(
    root: &Path,
    python: &Path,
    override_argv: &[String],
    entry: &str,
) -> Result<FocusedCheck, String> {
    let entry = entry.trim();
    if entry.is_empty() || entry.len() > MAX_ENTRY_BYTES || entry.chars().any(char::is_control) {
        return Err(format!(
            "`{entry}` is not a verify target; name a test file/directory, `path::test`, or project check (`clippy`/`fmt`/`lint`/`typecheck`/`build`/`check`/`test` or `dir:kind`)"
        ));
    }
    let (path_text, selector) = match entry.split_once("::") {
        Some((path, selector)) => (path.trim(), Some(selector.trim())),
        None => (entry, None),
    };
    if path_text.is_empty() {
        return Err(format!(
            "`{entry}` is not a verify target; name a test file in the workspace, optionally `path::test`"
        ));
    }
    if selector.is_some_and(|selector| {
        selector.is_empty()
            || !selector
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | ' '))
    }) {
        return Err(format!(
            "`{entry}`: the test selector after `::` must be a test, class or module name"
        ));
    }
    if let Some((dir_opt, kind)) = parse_project_check(path_text)
        && !root.join(path_text).is_file()
    {
        return project_check(root, python, override_argv, entry, dir_opt, kind, selector);
    }
    let path = workspace_relative(root, Path::new(path_text))?;
    let path = path.as_path();
    let joined = root.join(path);
    if joined.is_dir() {
        return directory_check(root, python, override_argv, entry, path, selector);
    }
    if !joined.is_file() {
        return Err(format!(
            "`{path_text}` does not exist; write the test first or name an existing file or directory"
        ));
    }
    let check = |command: String| FocusedCheck {
        name: format!("{NAME_PREFIX}{entry}"),
        command,
        timeout_secs: 0,
    };
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    match extension {
        "py" => {
            python_check(root, python, override_argv, path, selector).map(|(command, exit_only)| {
                FocusedCheck {
                    name: if exit_only {
                        format!("{NAME_PREFIX}{entry}{EXIT_ONLY_SUFFIX}")
                    } else {
                        format!("{NAME_PREFIX}{entry}")
                    },
                    command,
                    timeout_secs: 0,
                }
            })
        }
        "rs" => rust_check(root, path, selector).map(|rust| FocusedCheck {
            name: format!("{NAME_PREFIX}{entry}"),
            ..rust
        }),
        "go" => {
            let (_, mut command) = go_test_command(root, path)
                .ok_or_else(|| format!("no owning Go module was found for `{path_text}`"))?;
            if let Some(selector) = selector {
                command.push_str(&format!(" -run {}", shell_quote(&format!("^{selector}$"))));
            }
            Ok(check(command))
        }
        _ if JAVASCRIPT_EXTENSIONS.contains(&extension) => {
            javascript_check(root, path, selector).map(check)
        }
        _ => Err(format!(
            "no test runner is known for `{path_text}`; verify runs test files (Python, Rust, Go, JavaScript/TypeScript), so check other outputs with bash"
        )),
    }
}

/// `path` relative to the workspace `root`. Absolute paths are accepted when they lie under the
/// root as given or as canonicalized (e.g. `/app/x.py` with root `/app`); `..` never is.
fn workspace_relative(root: &Path, path: &Path) -> Result<std::path::PathBuf, String> {
    let relative = if path.is_absolute() {
        let canonical = root.canonicalize().ok();
        let stripped = path
            .strip_prefix(root)
            .ok()
            .or_else(|| path.strip_prefix(canonical.as_deref()?).ok());
        let Some(stripped) = stripped else {
            return Err(format!(
                "`{}` is outside the workspace `{}`; verify only runs tests inside it",
                path.display(),
                root.display()
            ));
        };
        stripped.to_path_buf()
    } else {
        path.to_path_buf()
    };
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "`{}` must stay inside the workspace without `..`",
            path.display()
        ));
    }
    if relative.as_os_str().is_empty() {
        return Err(format!(
            "`{}` names the workspace, not a test file",
            path.display()
        ));
    }
    Ok(relative)
}

/// The Python command for `path` and whether it is a plain script judged by exit status alone.
fn python_check(
    root: &Path,
    python: &Path,
    override_argv: &[String],
    path: &Path,
    selector: Option<&str>,
) -> Result<(String, bool), String> {
    let owner = python_owner(root, path);
    let joined = root.join(path);
    let relative = joined.strip_prefix(&owner).unwrap_or(path);
    let interpreter = scoped_python(root, &owner, python, override_argv);
    if pytest_configured(&owner) {
        let mut node = relative.to_string_lossy().into_owned();
        if let Some(selector) = selector {
            node.push_str("::");
            node.push_str(selector);
        }
        return Ok((
            format!("{interpreter} -m pytest -q {}", shell_quote(&node)),
            false,
        ));
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let test_file = stem.starts_with("test_")
        || stem.ends_with("_test")
        || path
            .components()
            .any(|part| matches!(part.as_os_str().to_str(), Some("tests" | "test")));
    if !test_file && selector.is_none() {
        return Ok((
            format!("{interpreter} {}", shell_quote(&relative.to_string_lossy())),
            true,
        ));
    }
    let mut module: Vec<String> = relative
        .with_extension("")
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(selector) = selector {
        module.extend(selector.split("::").map(str::to_owned));
    }
    let identifier = |part: &String| {
        part.chars().next().is_some_and(|c| !c.is_ascii_digit())
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if !module.iter().all(identifier) {
        return Err(format!(
            "`{}` is not importable as a unittest module; configure pytest or rename it",
            path.display()
        ));
    }
    Ok((
        format!(
            "{interpreter} -m unittest {}",
            shell_quote(&module.join("."))
        ),
        false,
    ))
}

fn rust_check(root: &Path, path: &Path, selector: Option<&str>) -> Result<FocusedCheck, String> {
    let mut packages = Default::default();
    let Some(selector) = selector else {
        return match rust_selection(root, path, &mut packages) {
            RustSelection::Check(check) => Ok(check),
            RustSelection::Gap(reason) => Err(format!("`{}`: {reason}", path.display())),
            RustSelection::Static => Err(format!(
                "`{}` has no Rust tests to run; name a test file or `path::test`",
                path.display()
            )),
        };
    };
    let file = normalize(path);
    let package = owning_package(root, &file, &mut packages)
        .map_err(|reason| format!("`{}`: {reason}", path.display()))?;
    let target = package
        .targets
        .iter()
        .find(|target| target.root == file)
        .or_else(|| owning_target(root, package, &file).map(|(target, _)| target))
        .ok_or_else(|| {
            format!(
                "`{}` is not inside any registered target's source tree",
                path.display()
            )
        })?;
    Ok(cargo_check(package, target, Some(selector.to_owned())))
}

fn javascript_check(root: &Path, path: &Path, selector: Option<&str>) -> Result<String, String> {
    // Walk up from the file's directory to `root` so monorepo / multi-repo subprojects find
    // their own package.json and local jest/vitest binary.
    if let Some((owner, runner)) = nearest_javascript_runner(root, path) {
        let rel = root
            .join(path)
            .strip_prefix(&owner)
            .unwrap_or(path)
            .to_path_buf();
        let mut command = javascript_test_command(&owner, &rel, &runner)
            .map(|check| check.command)
            .ok_or_else(|| format!("no test file was found for `{}`", path.display()))?;
        if let Some(selector) = selector {
            command.push_str(&format!(" -t {}", shell_quote(selector)));
        }
        return Ok(in_dir(root, &owner, &command));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "js" | "mjs" | "cjs" | "ts" | "mts" | "cts") {
        let mut command = format!("node --test {}", shell_quote(&path.to_string_lossy()));
        if let Some(selector) = selector {
            command.push_str(&format!(" --test-name-pattern {}", shell_quote(selector)));
        }
        return Ok(command);
    }
    Err(format!(
        "no installed jest or vitest `test` script covers `{}`; run it with bash instead",
        path.display()
    ))
}

/// Prefix `command` with `cd <rel> &&` when `dir` is a subdirectory of `root`.
fn in_dir(root: &Path, dir: &Path, command: &str) -> String {
    match dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            format!("cd {} && {command}", shell_quote(&rel.to_string_lossy()))
        }
        _ => command.to_owned(),
    }
}

/// Nearest owning directory between `path` and `root` that has an installed jest/vitest runner.
fn nearest_javascript_runner(
    root: &Path,
    path: &Path,
) -> Option<(std::path::PathBuf, (&'static str, String))> {
    let full = root.join(path);
    let mut dir = if full.is_dir() {
        full.as_path()
    } else {
        full.parent()?
    };
    loop {
        if let Some(runner) = javascript_test_runner(dir) {
            return Some((dir.to_path_buf(), runner));
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// Run a test directory (`dial-web/tests`, `crates/dial-tools`, `tests/`, etc.).
fn directory_check(
    root: &Path,
    python: &Path,
    override_argv: &[String],
    entry: &str,
    path: &Path,
    selector: Option<&str>,
) -> Result<FocusedCheck, String> {
    let dir = root.join(path);
    let check = |command: String| FocusedCheck {
        name: format!("{NAME_PREFIX}{entry}"),
        command,
        timeout_secs: 0,
    };
    if dir.join("Cargo.toml").is_file() {
        let mut cmd = "cargo test --offline".to_owned();
        if let Some(selector) = selector {
            cmd.push(' ');
            cmd.push_str(&shell_quote(selector));
        }
        return Ok(check(in_dir(root, &dir, &cmd)));
    }
    if let Some((_, mut cmd)) = go_test_command(root, &path.join("_")) {
        if let Some(selector) = selector {
            cmd.push_str(&format!(" -run {}", shell_quote(&format!("^{selector}$"))));
        }
        return Ok(check(cmd));
    }
    let mut js_tests = Vec::new();
    let mut has_py_tests = false;
    let mut visited_dirs = 0usize;
    collect_dir_tests(
        root,
        &dir,
        0,
        &mut visited_dirs,
        &mut js_tests,
        &mut has_py_tests,
    );
    js_tests.sort();
    if !js_tests.is_empty() {
        if let Some((owner, (runner, invocation))) = nearest_javascript_runner(root, path) {
            let rel = dir.strip_prefix(&owner).unwrap_or(path);
            let mut cmd = match runner {
                "jest" => format!(
                    "{invocation} --runInBand {}",
                    shell_quote(&rel.to_string_lossy())
                ),
                _ => format!("{invocation} run {}", shell_quote(&rel.to_string_lossy())),
            };
            if let Some(selector) = selector {
                cmd.push_str(&format!(" -t {}", shell_quote(selector)));
            }
            return Ok(check(in_dir(root, &owner, &cmd)));
        }
        let node_files: Vec<String> = js_tests
            .into_iter()
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts")
                )
            })
            .take(MAX_DIR_TEST_FILES)
            .map(|p| shell_quote(&p.to_string_lossy()))
            .collect();
        if !node_files.is_empty() {
            let mut cmd = format!("node --test {}", node_files.join(" "));
            if let Some(selector) = selector {
                cmd.push_str(&format!(" --test-name-pattern {}", shell_quote(selector)));
            }
            return Ok(check(cmd));
        }
    }
    if has_py_tests {
        let owner = python_owner(root, path);
        let rel = dir.strip_prefix(&owner).unwrap_or(path);
        let interpreter = scoped_python(root, &owner, python, override_argv);
        if pytest_configured(&owner) {
            let mut cmd = format!(
                "{interpreter} -m pytest -q {}",
                shell_quote(&rel.to_string_lossy())
            );
            if let Some(selector) = selector {
                cmd.push_str(&format!(" -k {}", shell_quote(selector)));
            }
            return Ok(check(cmd));
        }
        let cmd = format!(
            "{interpreter} -m unittest discover -s {}",
            shell_quote(&rel.to_string_lossy())
        );
        return Ok(check(cmd));
    }
    Err(format!(
        "no test suite was detected in directory `{}`; name a test file or project check (`{}:test`)",
        path.display(),
        path.display()
    ))
}

/// Recursively collect JS/TS and Python test files under `dir` within bounded depth/directory caps.
fn collect_dir_tests(
    root: &Path,
    dir: &Path,
    depth: usize,
    visited_dirs: &mut usize,
    js_tests: &mut Vec<std::path::PathBuf>,
    has_py_tests: &mut bool,
) {
    if depth > MAX_DIR_WALK_DEPTH || *visited_dirs >= MAX_DIR_WALK_DIRS {
        return;
    }
    *visited_dirs += 1;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs = Vec::new();
    for item in entries.flatten() {
        let p = item.path();
        let name = item.file_name();
        let name = name.to_str().unwrap_or("");
        if p.is_dir() {
            if !matches!(
                name,
                ".git"
                    | "node_modules"
                    | "target"
                    | ".venv"
                    | "venv"
                    | "dist"
                    | "build"
                    | "__pycache__"
                    | ".tmp"
            ) {
                subdirs.push(p);
            }
            continue;
        }
        if !p.is_file() {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "py" && (name.starts_with("test_") || name.ends_with("_test.py")) {
            *has_py_tests = true;
        } else if JAVASCRIPT_EXTENSIONS.contains(&ext)
            && (name.contains(".test.") || name.contains(".spec."))
            && js_tests.len() < MAX_DIR_TEST_FILES
            && let Ok(rel) = p.strip_prefix(root)
        {
            js_tests.push(rel.to_path_buf());
        }
    }
    subdirs.sort();
    for sub in subdirs {
        if *visited_dirs >= MAX_DIR_WALK_DIRS {
            break;
        }
        collect_dir_tests(root, &sub, depth + 1, visited_dirs, js_tests, has_py_tests);
    }
}

/// Project-level check (`clippy`, `fmt`, `lint`, `typecheck`, `check`, `build`, `test`) at `dir_opt`.
fn project_check(
    root: &Path,
    python: &Path,
    override_argv: &[String],
    entry: &str,
    dir_opt: Option<&str>,
    kind: ProjectCheck,
    selector: Option<&str>,
) -> Result<FocusedCheck, String> {
    if selector.is_some() && kind != ProjectCheck::Test {
        return Err(format!(
            "`{entry}`: `::selector` is only supported on `test` project checks"
        ));
    }
    let dir = match dir_opt {
        None | Some(".") => root.to_path_buf(),
        Some(raw) => {
            let rel = workspace_relative(root, Path::new(raw))?;
            let full = root.join(rel);
            if !full.is_dir() {
                return Err(format!("`{raw}` is not an existing workspace directory"));
            }
            full
        }
    };
    let command = detect_project_command(root, &dir, python, override_argv, kind, selector)?;
    Ok(FocusedCheck {
        name: format!("{NAME_PREFIX}{entry}"),
        command: in_dir(root, &dir, &command),
        timeout_secs: 0,
    })
}

fn detect_project_command(
    root: &Path,
    dir: &Path,
    python: &Path,
    override_argv: &[String],
    kind: ProjectCheck,
    selector: Option<&str>,
) -> Result<String, String> {
    let has_cargo = dir.join("Cargo.toml").is_file();
    let has_pkg = dir.join("package.json").is_file();
    let has_go = dir.join("go.mod").is_file();
    let has_py = dir.join("pyproject.toml").is_file()
        || dir.join("pytest.ini").is_file()
        || dir.join("setup.cfg").is_file()
        || dir.join("pyrightconfig.json").is_file()
        || dir.join("mypy.ini").is_file()
        || dir.join("ruff.toml").is_file();

    if kind == ProjectCheck::Clippy {
        if has_cargo {
            return Ok("cargo clippy --offline --all-targets -- -D warnings".to_owned());
        }
        return Err(format!(
            "no Cargo.toml in `{}` for `clippy`",
            dir.strip_prefix(root).unwrap_or(dir).display()
        ));
    }

    if has_pkg && let Some(mut cmd) = node_project_command(dir, kind) {
        if kind == ProjectCheck::Test
            && let Some(sel) = selector
        {
            cmd.push_str(&format!(" -- -t {}", shell_quote(sel)));
        }
        return Ok(cmd);
    }
    if has_cargo {
        let mut cmd = match kind {
            ProjectCheck::Clippy | ProjectCheck::Lint => {
                "cargo clippy --offline --all-targets -- -D warnings"
            }
            ProjectCheck::Fmt => "cargo fmt --all -- --check",
            ProjectCheck::Check | ProjectCheck::Typecheck => "cargo check --offline --all-targets",
            ProjectCheck::Build => "cargo build --offline --all-targets",
            ProjectCheck::Test => "cargo test --offline",
        }
        .to_owned();
        if kind == ProjectCheck::Test
            && let Some(sel) = selector
        {
            cmd.push(' ');
            cmd.push_str(&shell_quote(sel));
        }
        return Ok(cmd);
    }
    if has_go {
        let cmd = match kind {
            ProjectCheck::Test => {
                let mut base = "go test ./...".to_owned();
                if let Some(sel) = selector {
                    base.push_str(&format!(" -run {}", shell_quote(&format!("^{sel}$"))));
                }
                Some(base)
            }
            ProjectCheck::Build | ProjectCheck::Check | ProjectCheck::Typecheck => {
                Some("go build ./...".to_owned())
            }
            ProjectCheck::Lint => Some("go vet ./...".to_owned()),
            ProjectCheck::Fmt => Some("test -z \"$(gofmt -l .)\"".to_owned()),
            ProjectCheck::Clippy => None,
        };
        if let Some(cmd) = cmd {
            return Ok(cmd);
        }
    }
    if has_py {
        let interpreter = scoped_python(root, dir, python, override_argv);
        let pyproject = std::fs::metadata(dir.join("pyproject.toml"))
            .ok()
            .filter(|m| m.len() <= 256 * 1024)
            .and_then(|_| std::fs::read_to_string(dir.join("pyproject.toml")).ok())
            .unwrap_or_default();
        let has_ruff = dir.join("ruff.toml").is_file() || pyproject.contains("ruff");
        match kind {
            ProjectCheck::Test => {
                if pytest_configured(dir) {
                    let mut cmd = format!("{interpreter} -m pytest -q");
                    if let Some(sel) = selector {
                        cmd.push_str(&format!(" -k {}", shell_quote(sel)));
                    }
                    return Ok(cmd);
                }
                return Ok(format!("{interpreter} -m unittest discover"));
            }
            ProjectCheck::Lint if has_ruff => {
                return Ok("ruff check .".to_owned());
            }
            ProjectCheck::Fmt if has_ruff => {
                return Ok("ruff format --check .".to_owned());
            }
            ProjectCheck::Fmt if pyproject.contains("black") => {
                return Ok(format!("{interpreter} -m black --check ."));
            }
            ProjectCheck::Typecheck if dir.join("pyrightconfig.json").is_file() => {
                return Ok("pyright".to_owned());
            }
            ProjectCheck::Typecheck if dir.join("mypy.ini").is_file() => {
                return Ok(format!("{interpreter} -m mypy ."));
            }
            ProjectCheck::Typecheck if pyproject.contains("pyright") => {
                return Ok("pyright".to_owned());
            }
            ProjectCheck::Typecheck if pyproject.contains("mypy") => {
                return Ok(format!("{interpreter} -m mypy ."));
            }
            _ => {}
        }
    }
    Err(format!(
        "no `{}` check was detected in `{}` (checked package.json, Cargo.toml, go.mod, Python config)",
        kind.label(),
        dir.strip_prefix(root)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| ".".to_owned(), |p| p.display().to_string()),
    ))
}
fn node_project_command(dir: &Path, kind: ProjectCheck) -> Option<String> {
    let path = dir.join("package.json");
    if std::fs::metadata(&path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let scripts = json.get("scripts").and_then(serde_json::Value::as_object);
    let pm = if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else if dir.join("bun.lock").is_file() || dir.join("bun.lockb").is_file() {
        "bun"
    } else {
        "npm"
    };
    let run_script = |candidates: &[&str]| -> Option<String> {
        let scripts = scripts?;
        candidates.iter().find_map(|name| {
            scripts
                .get(*name)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|_| {
                    if *name == "test" {
                        format!("{pm} test")
                    } else {
                        format!("{pm} run {name}")
                    }
                })
        })
    };
    match kind {
        ProjectCheck::Lint => run_script(&["lint", "lint:check"]).or_else(|| {
            let bin = dir.join("node_modules/.bin/eslint");
            bin.exists()
                .then(|| "./node_modules/.bin/eslint .".to_owned())
        }),
        ProjectCheck::Typecheck | ProjectCheck::Check => {
            run_script(&["typecheck", "type-check", "check"]).or_else(|| {
                let bin = dir.join("node_modules/.bin/tsc");
                bin.exists()
                    .then(|| "./node_modules/.bin/tsc --noEmit".to_owned())
            })
        }
        ProjectCheck::Fmt => run_script(&["fmt:check", "format:check", "fmt", "format"]),
        ProjectCheck::Build => run_script(&["build"]),
        ProjectCheck::Test => run_script(&["test"]),
        ProjectCheck::Clippy => None,
    }
}

/// Totals from cargo, pytest, unittest, jest and vitest summary lines; `None` without any.
pub fn test_counts(output: &str) -> Option<TestCounts> {
    let mut totals: Option<TestCounts> = None;
    let mut add = |passed: u64, failed: u64| {
        let total = totals.get_or_insert_default();
        total.passed += passed;
        total.failed += failed;
    };
    let mut unittest_ran = None;
    for line in output.lines() {
        let line = line.trim().trim_matches('=').trim();
        if let Some(rest) = line.strip_prefix("test result: ") {
            add(labelled(rest, "passed"), labelled(rest, "failed"));
        } else if let Some(rest) = line.strip_prefix("Ran ") {
            unittest_ran = rest
                .split_whitespace()
                .next()
                .and_then(|count| count.parse::<u64>().ok());
        } else if let Some(ran) = unittest_ran
            && (line == "OK" || line.starts_with("OK (") || line.starts_with("FAILED ("))
        {
            unittest_ran = None;
            let failed = assigned(line, "failures") + assigned(line, "errors");
            let skipped = assigned(line, "skipped") + assigned(line, "expected failures");
            add(ran.saturating_sub(failed + skipped), failed);
        } else if let Some(rest) = line
            .strip_prefix("Tests:")
            .or_else(|| line.strip_prefix("Tests "))
        {
            add(labelled(rest, "passed"), labelled(rest, "failed"));
        } else if pytest_summary(line) {
            let errors = labelled(line, "error") + labelled(line, "errors");
            add(labelled(line, "passed"), labelled(line, "failed") + errors);
        } else if let Some(rest) = line.strip_prefix("ℹ ").or_else(|| line.strip_prefix("# ")) {
            if let Some(count) = rest
                .strip_prefix("pass ")
                .and_then(|n| n.trim().parse::<u64>().ok())
            {
                add(count, 0);
            } else if let Some(count) = rest
                .strip_prefix("fail ")
                .and_then(|n| n.trim().parse::<u64>().ok())
            {
                add(0, count);
            }
        }
    }
    totals
}

/// pytest's closing line: `1 failed, 4 passed in 0.05s`, with or without `=` rules.
fn pytest_summary(line: &str) -> bool {
    let Some((counts, duration)) = line.rsplit_once(" in ") else {
        return false;
    };
    duration.starts_with(|c: char| c.is_ascii_digit())
        && ["passed", "failed", "error", "errors"]
            .iter()
            .any(|label| labelled(counts, label) > 0)
}

/// Sum of every `N label` pair in `text`.
fn labelled(text: &str, label: &str) -> u64 {
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '|' | '(' | ')'))
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .windows(2)
        .filter(|pair| pair[1].trim_end_matches('.') == label)
        .filter_map(|pair| pair[0].parse::<u64>().ok())
        .sum()
}

/// The value of `label=N` in unittest's `FAILED (failures=1, errors=2)`.
fn assigned(text: &str, label: &str) -> u64 {
    text.match_indices(&format!("{label}="))
        .filter_map(|(index, matched)| {
            let digits: String = text[index + matched.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u64>().ok()
        })
        .sum()
}

#[cfg(test)]
mod tests;
