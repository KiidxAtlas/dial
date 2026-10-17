use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A fixed, focused verification command selected for changed source files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusedCheck {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    CodeFailed,
    EnvironmentUnavailable,
    PermissionDenied,
    Cancelled,
    TimedOut,
    Unverified,
}

impl CheckStatus {
    /// Only an observed code failure may be sent to the repair loop. Infrastructure, permission,
    /// cancellation, timeout and unverifiable runs are reported without editing application code.
    pub fn repairable(self) -> bool {
        self == CheckStatus::CodeFailed
    }

    /// Only completed, classified checks can establish a repair baseline or its passing result.
    /// Unknown runs must still reach the tracker with `None` to invalidate an earlier baseline.
    pub(super) fn repair_exit_code(self, output: &crate::tools::ToolOutput) -> Option<i32> {
        let code = output.process_outcome()?.exit_code?;
        match self {
            Self::Passed if code == 0 => Some(code),
            Self::CodeFailed if code != 0 => Some(code),
            _ => None,
        }
    }
}

pub fn classify(
    check: &FocusedCheck,
    output: &crate::tools::ToolOutput,
    cancelled: bool,
) -> CheckStatus {
    classify_output(Some(check), output, cancelled)
}

/// The one verdict for a finished run, shared by cache, assistance and repair bookkeeping.
/// `check` supplies the configured runner for setup diagnosis; undeclared commands pass `None`.
/// Success requires observed process completion, not printed prose such as `[exit code: 0]`.
pub(super) fn classify_output(
    check: Option<&FocusedCheck>,
    output: &crate::tools::ToolOutput,
    cancelled: bool,
) -> CheckStatus {
    let process = output.process_outcome();
    let status = crate::tools::ToolStatus::from_output(output);
    if cancelled
        || status == crate::tools::ToolStatus::Aborted
        || process.as_ref().is_some_and(|process| process.cancelled)
    {
        return CheckStatus::Cancelled;
    }
    if status == crate::tools::ToolStatus::Timeout
        || process.as_ref().is_some_and(|process| process.timed_out)
    {
        return CheckStatus::TimedOut;
    }
    if status == crate::tools::ToolStatus::Blocked {
        return CheckStatus::PermissionDenied;
    }
    let complete = process
        .as_ref()
        .is_some_and(|process| process.started && process.output_complete)
        && output.execution.effects != crate::tools::outcome::Effects::Unknown;
    if complete
        && !output.is_error
        && process
            .as_ref()
            .is_some_and(|process| process.exit_code == Some(0))
        && !zero_tests(&output.text)
    {
        return CheckStatus::Passed;
    }
    if output.is_error
        && check.is_some_and(|check| environment_problem(check, &output.text).is_some())
    {
        return CheckStatus::EnvironmentUnavailable;
    }
    let Some(process) = process.filter(|_| complete) else {
        return CheckStatus::Unverified;
    };
    if zero_tests(&output.text) || process.exit_code.is_none() || process.exit_code == Some(0) {
        return CheckStatus::Unverified;
    }
    CheckStatus::CodeFailed
}

#[cfg(test)]
mod authority_tests;

fn python_owner(root: &Path, changed: &Path) -> PathBuf {
    if changed.is_absolute()
        || changed
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return root.into();
    }
    let joined = root.join(changed);
    let mut dir = joined.parent().unwrap_or(root);
    while dir.starts_with(root) {
        if let (Ok(real), Ok(real_root)) = (dir.canonicalize(), root.canonicalize())
            && !real.starts_with(real_root)
        {
            return root.into();
        }
        if [
            "pyproject.toml",
            "setup.cfg",
            "setup.py",
            "uv.lock",
            "poetry.lock",
            "pytest.ini",
        ]
        .iter()
        .any(|name| dir.join(name).is_file())
            || venv_python(dir).is_file()
        {
            return dir.into();
        }
        if dir == root {
            break;
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }
    root.into()
}

fn venv_python(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        root.join(".venv/Scripts/python.exe")
    }
    #[cfg(not(windows))]
    {
        root.join(".venv/bin/python")
    }
}
fn scoped_python(root: &Path, owner: &Path, fallback: &Path, override_argv: &[String]) -> String {
    let command = if override_argv.is_empty() {
        if owner != root
            && root.join("uv.lock").is_file()
            && !owner.join("poetry.lock").is_file()
            && !venv_python(owner).is_file()
        {
            "uv run --offline --no-sync python".into()
        } else {
            python_runner(owner, fallback)
        }
    } else {
        let program = override_argv
            .first()
            .map(|arg| {
                if arg.contains(['/', '\\']) && Path::new(arg).is_relative() {
                    root.join(arg).to_string_lossy().into_owned()
                } else {
                    arg.clone()
                }
            })
            .unwrap_or_default();
        let args = override_argv
            .iter()
            .skip(1)
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{}{}{}",
            shell_invocation(&program),
            if args.is_empty() { "" } else { " " },
            args
        )
    };
    if owner == root {
        command
    } else {
        #[cfg(windows)]
        {
            format!(
                "Set-Location -LiteralPath {}; if (!$?) {{ exit 1 }}; {}",
                shell_quote(&owner.to_string_lossy()),
                command
            )
        }
        #[cfg(not(windows))]
        {
            format!("cd {} && {command}", shell_quote(&owner.to_string_lossy()))
        }
    }
}

/// Shell evidence may include an artifact link after the exit-code row.
pub(crate) fn exit_code(output: &str) -> Option<i32> {
    output.lines().rev().find_map(|line| {
        line.strip_prefix("[exit code: ")?
            .strip_suffix(']')?
            .parse()
            .ok()
    })
}

/// Recognize explicit empty-suite outcomes. A successful exit alone does not prove tests executed.
/// Whole-run aggregation: a runner reports testless packages one row each (`go test` prints
/// `? pkg [no test files]`), so the run is empty only when no package executed a test.
pub(super) fn zero_tests(output: &str) -> bool {
    if output.contains("Ran 0 tests")
        || output.contains("no tests ran")
        || output.lines().any(|line| line.trim() == "No tests found")
    {
        return true;
    }
    let testless = output
        .lines()
        .any(|line| line.trim().ends_with("[no test files]"));
    if testless {
        let executed = output.lines().any(|line| {
            let line = line.trim();
            line.starts_with("ok ")
                && !line.ends_with("[no test files]")
                && !line.contains("[no tests to run]")
        });
        return !executed;
    }
    let mut counts = output
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("running ")
                .and_then(|v| v.strip_suffix(" tests"))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .peekable();
    counts.peek().is_some() && counts.all(|n| n == 0)
}

/// Only compact retained successful checks; preserve test counts and diagnostics
/// in the visible tail, and leave empty suites/failures untouched.
pub(super) fn compact_success(output: &str, artifact: &str) -> Option<String> {
    if output.len() < 4096 || exit_code(output) != Some(0) || zero_tests(output) {
        return None;
    }
    let mut tail = output.len().saturating_sub(2048);
    while !output.is_char_boundary(tail) {
        tail += 1;
    }
    Some(format!(
        "[Successful check; {} bytes retained at {artifact}; retrieve with artifact_read]\n{}",
        output.len(),
        &output[tail..]
    ))
}

#[cfg(test)]
mod compact_tests;

/// Select a project runner without installing packages or falling back to a different
/// interpreter when that runner fails. Commands still pass normal shell approval.
fn python_runner(root: &Path, fallback: &Path) -> String {
    if root.join("uv.lock").is_file() {
        "uv run --offline --no-sync python".into()
    } else if venv_python(root).is_file() {
        shell_invocation(&venv_python(root).to_string_lossy())
    } else if root.join("poetry.lock").is_file()
        || config_contains(root, "pyproject.toml", "[tool.poetry]")
    {
        "poetry run python".into()
    } else {
        shell_invocation(&fallback.to_string_lossy())
    }
}

/// Diagnose explicit setup failures, never reinterpret an assertion or syntax error
/// as success. Missing application modules remain code failures, not setup guesses.
pub(super) fn environment_problem(check: &FocusedCheck, output: &str) -> Option<&'static str> {
    // Match the selected runner, not arbitrary missing programs invoked by application code.
    let missing_executable = |program: &str| {
        output.lines().any(|line| {
            let line = line.trim();
            line == format!("{program}: command not found")
                || line.ends_with(&format!(": {program}: command not found"))
                || line.ends_with(&format!(": {program}: not found"))
                || line == format!("env: {program}: No such file or directory")
                || line == format!("/usr/bin/env: {program}: No such file or directory")
                || line == format!("env: '{program}': No such file or directory")
        })
    };
    let cargo = (check.name == "cargo check"
        || check.name.starts_with("cargo check: ")
        || check.name.starts_with("related Rust tests:"))
        && check.command.starts_with("cargo ");
    let go = check.name.starts_with("related Go package tests:")
        && check.command.starts_with("go test ");
    let javascript = (check.name.starts_with("related vitest test:")
        && check.command.contains("node_modules/.bin/vitest"))
        || (check.name.starts_with("related jest test:")
            && check.command.contains("node_modules/.bin/jest"));
    let offline_cache = cargo
        && check
            .command
            .split_whitespace()
            .any(|arg| arg == "--offline")
        && ((output.contains("error: no matching package named `")
            && output.contains("location searched: crates.io index")
            && output.contains("As a reminder, you're using offline mode (--offline)"))
            || (output.contains("error: failed to download `")
                && output
                    .contains("attempting to make an HTTP request, but --offline was specified")));
    if (cargo
        && (missing_executable("cargo")
            || output.contains("error: rustup could not choose a version of cargo to run, because one wasn't specified explicitly, and no default is configured.")))
        || (go && missing_executable("go"))
        || (javascript && missing_executable("node"))
        || offline_cache
    {
        return Some(
            "Verification environment unavailable (not a confirmed code failure). Restore the selected toolchain or existing offline dependency cache, then rerun this check. Installation/network setup still require approval.",
        );
    }
    if !(check.name.starts_with("python AST:") || check.name.starts_with("related Python tests:")) {
        return None;
    }
    let setup = [
        "uv: command not found",
        "poetry: command not found",
        "No module named pytest",
        "No module named 'pytest'",
        "No interpreter found",
        "does not satisfy the project's Python requirement",
        "is not allowed by the project",
    ];
    setup.iter().any(|message| output.contains(message)).then_some(
        "Verification environment unavailable or incompatible (not a confirmed code failure). Use the project's runner/interpreter and existing environment; do not edit application code to satisfy the wrong Python. Correct the environment or configure a matching verification command, then rerun this check. Packages/network setup still require approval."
    )
}

#[cfg(test)]
pub fn plan(root: &Path, python: &Path, changed_paths: &[PathBuf]) -> Vec<FocusedCheck> {
    plan_with_runner(root, python, changed_paths, &[])
}

pub fn plan_with_runner(
    root: &Path,
    python: &Path,
    changed_paths: &[PathBuf],
    override_argv: &[String],
) -> Vec<FocusedCheck> {
    let rust_paths = changed_paths
        .iter()
        .filter(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                || path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
        })
        .collect::<Vec<_>>();

    let mut checks = Vec::new();
    if !rust_paths.is_empty() && root.join("Cargo.toml").is_file() {
        checks.push(FocusedCheck {
            name: "cargo check".to_owned(),
            command: "cargo check --offline --all-targets --quiet".to_owned(),
            timeout_secs: 0,
        });
    } else {
        // Without a root manifest, check each nested crate that owns a changed file instead of
        // silently skipping Rust verification.
        let mut owners = Vec::new();
        for path in rust_paths {
            if let Some(manifest) = cargo_owner(root, path)
                && !owners.contains(&manifest)
            {
                owners.push(manifest);
            }
        }
        for manifest in owners {
            let crate_dir = manifest.parent().unwrap_or(Path::new(""));
            checks.push(FocusedCheck {
                name: format!("cargo check: {}", crate_dir.display()),
                command: format!(
                    "cargo check --offline --all-targets --quiet --manifest-path {}",
                    shell_quote(&manifest.to_string_lossy())
                ),
                timeout_secs: 0,
            });
        }
    }

    let mut seen_python_paths = HashSet::new();
    for path in changed_paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("py")
            || !seen_python_paths.insert(path)
        {
            continue;
        }
        // A renamed or deleted file has no post-change content to parse, so it gets no syntax obligation.
        // Only a confirmed absence counts: an unreadable path keeps its check so the failure stays explicit.
        if removed_from(root, path) {
            continue;
        }

        let owner = python_owner(root, path);
        let relative = root
            .join(path)
            .strip_prefix(&owner)
            .unwrap_or(path)
            .to_path_buf();
        let path = path.to_string_lossy();
        let python_code = "import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding='utf-8'), filename=p)";
        checks.push(FocusedCheck {
            name: format!("python AST: {path}"),
            command: [
                scoped_python(root, &owner, python, override_argv),
                shell_quote("-I"),
                shell_quote("-c"),
                shell_quote(python_code),
                shell_quote(&relative.to_string_lossy()),
            ]
            .join(" "),
            timeout_secs: 0,
        });
    }

    checks
}

/// Whether `path` is confirmed absent from the post-change workspace (renamed away or deleted).
/// Any other lookup failure, such as a permission error, is not absence and keeps its obligation.
fn removed_from(root: &Path, path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(root.join(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// A retained Python AST obligation whose file is gone is obsolete: nothing remains to parse.
pub fn obsolete_check(root: &Path, check: &FocusedCheck) -> bool {
    check
        .name
        .strip_prefix("python AST: ")
        .is_some_and(|path| removed_from(root, Path::new(path)))
}

/// Workspace-relative manifest of the nearest crate below `root` that owns `changed`.
fn cargo_owner(root: &Path, changed: &Path) -> Option<PathBuf> {
    if changed.is_absolute()
        || changed
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let mut dir = changed.parent();
    while let Some(current) = dir {
        if current.as_os_str().is_empty() {
            return None;
        }
        let manifest = current.join("Cargo.toml");
        if root.join(&manifest).is_file() {
            return Some(manifest);
        }
        dir = current.parent();
    }
    None
}

/// Changed paths beyond this count are not mapped to related tests.
pub const RELATED_TEST_PATH_LIMIT: usize = 32;

/// Explicit gaps in automatic test selection, so a passing run is not read as full coverage.
pub fn coverage_limits(changed: &[PathBuf]) -> Vec<String> {
    let mut limits = Vec::new();
    if changed.len() > RELATED_TEST_PATH_LIMIT {
        limits.push(format!(
            "related-test selection covered the first {RELATED_TEST_PATH_LIMIT} of {} changed paths; run broader tests for the rest",
            changed.len()
        ));
    }
    limits
}

fn config_contains(root: &Path, file: &str, marker: &str) -> bool {
    let path = root.join(file);
    std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= 1024 * 1024)
        && std::fs::read_to_string(path).is_ok_and(|text| text.contains(marker))
}

fn pytest_configured(root: &Path) -> bool {
    root.join("pytest.ini").is_file()
        || config_contains(root, "pyproject.toml", "[tool.pytest.")
        || config_contains(root, "setup.cfg", "[tool:pytest]")
}

fn rust_test_filter(root: &Path, path: &Path) -> Option<String> {
    if path.extension()?.to_str()? != "rs" {
        return None;
    }
    let source_path = root.join(path);
    if std::fs::metadata(&source_path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    let source = std::fs::read_to_string(source_path).ok()?;
    if !source.contains("#[test]") || !source.contains("mod tests") {
        return None;
    }
    let src_relative = path.strip_prefix("src").ok()?;
    let mut modules = src_relative
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    let file = src_relative.file_name()?.to_str()?;
    if file != "mod.rs" {
        let stem = Path::new(file).file_stem()?.to_str()?;
        if matches!(stem, "lib" | "main") {
            return None;
        }
        modules.push(stem);
    }
    if modules.is_empty()
        || modules.iter().any(|part| {
            part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    {
        return None;
    }
    Some(format!("{}::tests::", modules.join("::")))
}

fn has_go_package_tests(root: &Path, dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with("_test.go"))
    })
}

fn javascript_test_runner(root: &Path) -> Option<(&'static str, String)> {
    let package_path = root.join("package.json");
    if std::fs::metadata(&package_path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    let package: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package_path).ok()?).ok()?;
    let script = package["scripts"]["test"].as_str()?;
    let runner = if script.contains("vitest") {
        "vitest"
    } else if script.contains("jest") {
        "jest"
    } else {
        return None;
    };
    let executable_name = if cfg!(windows) {
        format!("{runner}.cmd")
    } else {
        runner.to_owned()
    };
    let executable = root.join("node_modules/.bin").join(executable_name);
    if !executable.is_file() {
        return None;
    }
    let invocation = shell_invocation(&executable.to_string_lossy());
    Some((runner, invocation))
}

fn javascript_test_command(
    root: &Path,
    changed: &Path,
    (runner, invocation): &(&str, String),
) -> Option<FocusedCheck> {
    let stem = changed.file_stem()?.to_str()?;
    if !stem
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let parent = changed.parent().unwrap_or(Path::new(""));
    let tests_dir = parent.join("__tests__");
    let extensions = ["js", "jsx", "ts", "tsx", "mjs", "cjs"];
    let mut test = None;
    for suffix in ["test", "spec"] {
        for extension in extensions {
            for candidate in [
                parent.join(format!("{stem}.{suffix}.{extension}")),
                tests_dir.join(format!("{stem}.{suffix}.{extension}")),
            ] {
                if root.join(&candidate).is_file() {
                    test = Some(candidate);
                    break;
                }
            }
            if test.is_some() {
                break;
            }
        }
        if test.is_some() {
            break;
        }
    }
    let test = test?;
    let mut command = format!("{invocation} ");
    if *runner == "vitest" {
        command.push_str("run ");
    } else {
        command.push_str("--runInBand ");
    }
    command.push_str(&shell_quote(&test.to_string_lossy()));
    Some(FocusedCheck {
        name: format!("related {runner} test: {}", test.display()),
        command,
        timeout_secs: 0,
    })
}

/// Conservative test mapping; ambiguous ownership leaves broader configured checks intact.
/// Selected tests supplement syntax/type checks, never independent behavioral evidence.
#[cfg(test)]
pub fn related_tests(root: &Path, python: &Path, changed: &[PathBuf]) -> Vec<FocusedCheck> {
    related_tests_with_runner(root, python, changed, &[])
}

pub fn related_tests_with_runner(
    root: &Path,
    python: &Path,
    changed: &[PathBuf],
    override_argv: &[String],
) -> Vec<FocusedCheck> {
    related_tests_with_metadata(
        root,
        python,
        changed,
        override_argv,
        || javascript_test_runner(root),
        |directory| has_go_package_tests(root, directory),
    )
}

// Keep filesystem metadata resolution injectable for deterministic per-invocation work counts.
fn related_tests_with_metadata(
    root: &Path,
    python: &Path,
    changed: &[PathBuf],
    override_argv: &[String],
    mut javascript_metadata: impl FnMut() -> Option<(&'static str, String)>,
    mut go_has_tests: impl FnMut(&Path) -> bool,
) -> Vec<FocusedCheck> {
    let mut selected = Vec::new();
    let cargo_project = root.join("Cargo.toml").is_file() && root.join("src/lib.rs").is_file();
    let go_project = root.join("go.mod").is_file();
    // Invocation-local metadata only: later edits must be visible on the next selection pass.
    let mut javascript_runner = None;
    let mut go_packages = HashMap::new();
    for path in changed.iter().take(RELATED_TEST_PATH_LIMIT) {
        if path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        match path.extension().and_then(|s| s.to_str()) {
            Some("py") => {
                let owner = python_owner(root, path);
                let pytest = pytest_configured(&owner);
                let test = owner.join("tests").join(format!("test_{stem}.py"));
                if test.is_file() {
                    let relative = test.strip_prefix(&owner).unwrap_or(&test);
                    let command = if pytest {
                        format!(
                            "{} -m pytest -q {}",
                            scoped_python(root, &owner, python, override_argv),
                            shell_quote(&relative.to_string_lossy())
                        )
                    } else {
                        format!(
                            "{} -m unittest discover -s tests -p {}",
                            scoped_python(root, &owner, python, override_argv),
                            shell_quote(&format!("test_{stem}.py"))
                        )
                    };
                    selected.push(FocusedCheck {
                        name: format!("related Python tests: {stem}"),
                        command,
                        timeout_secs: 0,
                    });
                }
            }
            Some("rs") if cargo_project => {
                if let Some(filter) = rust_test_filter(root, path) {
                    selected.push(FocusedCheck {
                        name: format!("related Rust tests: {filter}"),
                        command: format!(
                            "cargo test --offline --lib {} --quiet -j1",
                            shell_quote(&filter)
                        ),
                        timeout_secs: 0,
                    });
                }
            }
            Some("go") if go_project => {
                let directory = path.parent().unwrap_or(Path::new(""));
                if *go_packages
                    .entry(directory)
                    .or_insert_with(|| go_has_tests(directory))
                {
                    let package = if directory.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        format!("./{}", directory.to_string_lossy())
                    };
                    selected.push(FocusedCheck {
                        name: format!("related Go package tests: {package}"),
                        command: format!("go test {}", shell_quote(&package)),
                        timeout_secs: 0,
                    });
                }
            }
            Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs") => {
                if let Some(runner) = javascript_runner
                    .get_or_insert_with(&mut javascript_metadata)
                    .as_ref()
                    && let Some(check) = javascript_test_command(root, path, runner)
                {
                    selected.push(check);
                }
            }
            _ => {}
        }
    }
    let mut seen = HashSet::new();
    selected.retain(|check| seen.insert(check.command.clone()));
    selected
}

#[cfg(windows)]
fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "''"))
}

#[cfg(not(windows))]
fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

fn shell_invocation(executable: &str) -> String {
    #[cfg(windows)]
    {
        format!("& {}", shell_quote(executable))
    }
    #[cfg(not(windows))]
    {
        shell_quote(executable)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod selection_tests;

#[cfg(test)]
mod project_tests;

#[cfg(test)]
mod infrastructure_tests;
