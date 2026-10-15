use std::collections::HashSet;
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
mod authority_tests {
    use super::*;
    #[test]
    fn printed_success_cannot_replace_observed_process_termination() {
        let check = FocusedCheck {
            name: "fixture".into(),
            command: "cargo check".into(),
            timeout_secs: 1,
        };
        let prose = crate::tools::ToolOutput::ok("checks passed\n[exit code: 0]\n".into());
        assert_eq!(classify(&check, &prose, false), CheckStatus::Unverified);
        let failed = prose.with_process(crate::runtime::execution::ProcessOutcome {
            started: true,
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        });
        assert_eq!(classify(&check, &failed, false), CheckStatus::CodeFailed);
    }
}

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
pub(super) fn zero_tests(output: &str) -> bool {
    if output.contains("Ran 0 tests")
        || output.contains("no tests ran")
        || output
            .lines()
            .any(|line| line.trim().ends_with("[no test files]") || line.trim() == "No tests found")
    {
        return true;
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
mod compact_tests {
    use super::*;
    #[test]
    fn compaction_is_bounded_retrievable_and_preserves_verdicts() {
        let text = format!(
            "{}\nrunning 12 tests\ntest result: ok. 12 passed\n[exit code: 0]",
            "build progress\n".repeat(1000)
        );
        let compact = compact_success(&text, "artifact://check").unwrap();
        assert!(
            compact.len() < 2300
                && compact.contains("12 passed")
                && compact.contains("artifact://check")
        );
        assert!(
            compact_success(
                &text.replace("[exit code: 0]", "[exit code: 1]"),
                "artifact://check"
            )
            .is_none()
        );
        assert!(
            compact_success(
                &text.replace("running 12 tests", "running 0 tests"),
                "artifact://check"
            )
            .is_none()
        );
    }
}

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
    let cargo = (check.name == "cargo check" || check.name.starts_with("related Rust tests:"))
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
    let has_rust_changes = changed_paths.iter().any(|path| {
        path.extension().and_then(|extension| extension.to_str()) == Some("rs")
            || path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
    });

    let mut checks = Vec::new();
    if has_rust_changes && root.join("Cargo.toml").is_file() {
        checks.push(FocusedCheck {
            name: "cargo check".to_owned(),
            command: "cargo check --offline --all-targets --quiet".to_owned(),
            timeout_secs: 0,
        });
    }

    let mut seen_python_paths = HashSet::new();
    for path in changed_paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("py")
            || !seen_python_paths.insert(path)
        {
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

fn javascript_test_command(root: &Path, changed: &Path) -> Option<FocusedCheck> {
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
    let executable_name = if cfg!(windows) {
        format!("{runner}.cmd")
    } else {
        runner.to_owned()
    };
    let executable = root.join("node_modules/.bin").join(executable_name);
    if !executable.is_file() {
        return None;
    }
    let mut command = format!("{} ", shell_invocation(&executable.to_string_lossy()));
    if runner == "vitest" {
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
    let mut selected = Vec::new();
    let cargo_project = root.join("Cargo.toml").is_file() && root.join("src/lib.rs").is_file();
    let go_project = root.join("go.mod").is_file();
    for path in changed.iter().take(32) {
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
                if has_go_package_tests(root, directory) {
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
                if let Some(check) = javascript_test_command(root, path) {
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
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn exit_codes_survive_artifact_footers() {
        assert_eq!(
            exit_code("ok\n[exit code: 0]\n[complete output: artifact://abc]\n"),
            Some(0)
        );
        assert_eq!(exit_code("failed\n[exit code: 3]\n"), Some(3));
        assert_eq!(exit_code("[exit code: nope]\n"), None);
    }

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("focused-check-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn routes_and_deduplicates_supported_languages_in_stable_order() {
        let root = TempDir::new();
        fs::write(root.0.join("Cargo.toml"), "").unwrap();
        let changed = [
            PathBuf::from("src/main.rs"),
            PathBuf::from("scripts/check.py"),
            PathBuf::from("Cargo.toml"),
            PathBuf::from("scripts/check.py"),
            PathBuf::from("notes.txt"),
            PathBuf::from("config/custom.toml"),
            PathBuf::from("lib/module.rs"),
        ];

        let checks = plan(&root.0, Path::new("/usr/bin/python3"), &changed);

        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "cargo check");
        assert_eq!(
            checks[0].command,
            "cargo check --offline --all-targets --quiet"
        );
        assert_eq!(checks[0].timeout_secs, 0);
        assert_eq!(checks[1].name, "python AST: scripts/check.py");
        assert!(checks[1].command.contains("'scripts/check.py'"));
        assert_eq!(checks[1].timeout_secs, 0);
    }

    #[test]
    fn shell_quotes_executable_and_path_with_spaces_and_single_quotes() {
        let root = TempDir::new();
        let changed = [PathBuf::from("dir with space/a'b.py")];

        #[cfg(unix)]
        let python = Path::new("/opt/python's bin/python");
        #[cfg(windows)]
        let python = Path::new(r"C:\opt\python's bin\python.exe");
        let checks = plan(&root.0, python, &changed);
        assert_eq!(checks.len(), 1);
        #[cfg(unix)]
        assert_eq!(
            checks[0].command,
            "'/opt/python'\"'\"'s bin/python' '-I' '-c' 'import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding='\"'\"'utf-8'\"'\"'), filename=p)' 'dir with space/a'\"'\"'b.py'"
        );
        #[cfg(windows)]
        assert_eq!(
            checks[0].command,
            "& 'C:\\opt\\python''s bin\\python.exe' '-I' '-c' 'import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding=''utf-8''), filename=p)' 'dir with space\\a''b.py'"
        );
    }

    #[test]
    fn unrelated_paths_produce_no_checks() {
        let root = TempDir::new();
        let changed = [PathBuf::from("README.md"), PathBuf::from("image.png")];

        assert!(plan(&root.0, Path::new("python3"), &changed).is_empty());
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn python_checks_share_project_runner_without_sync_or_fallback() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("tests/test_app.py"), "").unwrap();
        std::fs::write(root.path().join("pytest.ini"), "").unwrap();
        std::fs::write(root.path().join("uv.lock"), "version = 1").unwrap();
        let static_check =
            plan(root.path(), Path::new("wrong-python"), &["app.py".into()]).remove(0);
        let test_check =
            related_tests(root.path(), Path::new("wrong-python"), &["app.py".into()]).remove(0);
        for check in [&static_check, &test_check] {
            assert!(
                check
                    .command
                    .starts_with("uv run --offline --no-sync python")
            );
            assert!(!check.command.contains("wrong-python"));
        }
        assert!(environment_problem(&test_check, "No module named pytest").is_some());
        assert!(environment_problem(&test_check, "AssertionError: wrong answer").is_none());
        assert!(environment_problem(&static_check, "SyntaxError: invalid syntax").is_none());
        std::fs::remove_file(root.path().join("uv.lock")).unwrap();
        std::fs::write(root.path().join("poetry.lock"), "").unwrap();
        assert_eq!(
            python_runner(root.path(), Path::new("fallback")),
            "poetry run python"
        );
        let venv = venv_python(root.path());
        std::fs::create_dir_all(venv.parent().unwrap()).unwrap();
        std::fs::write(&venv, "").unwrap();
        assert!(
            python_runner(root.path(), Path::new("fallback"))
                .contains(&venv.to_string_lossy().to_string())
        );
    }

    #[test]
    fn project_venv_executes_ast_and_tests_instead_of_invalid_fallback() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let created = std::process::Command::new("python")
            .args(["-m", "venv", "--without-pip"])
            .arg(root.path().join(".venv"))
            .status()
            .unwrap();
        #[cfg(not(windows))]
        let created = std::process::Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(root.path().join(".venv"))
            .status()
            .unwrap();
        assert!(created.success());
        std::fs::create_dir(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("app.py"), "answer = 42\n").unwrap();
        std::fs::write(root.path().join("tests/test_app.py"), "import unittest, sys\nclass Test(unittest.TestCase):\n def test_project_env(self):\n  self.assertNotEqual(sys.prefix, sys.base_prefix)\n").unwrap();
        let mut checks = plan(
            root.path(),
            Path::new("missing-interpreter"),
            &["app.py".into()],
        );
        checks.extend(related_tests(
            root.path(),
            Path::new("missing-interpreter"),
            &["app.py".into()],
        ));
        assert_eq!(checks.len(), 2);
        for check in checks {
            #[cfg(windows)]
            let output = {
                let command = format!(
                    "{}; if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}",
                    check.command
                );
                std::process::Command::new(
                    std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                        .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                )
                .args(["-NoProfile", "-NonInteractive", "-Command", &command])
                .current_dir(root.path())
                .output()
                .unwrap()
            };
            #[cfg(not(windows))]
            let output = std::process::Command::new("sh")
                .args(["-c", &check.command])
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}: {}",
                check.command,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn related_tests_are_direct_bounded_and_quoted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("tests/test_math.py"), "").unwrap();
        let checks = related_tests(
            root.path(),
            Path::new("/a path/python"),
            &[
                "math.py".into(),
                "math.py".into(),
                "other.py".into(),
                "bad;cmd.py".into(),
            ],
        );
        assert_eq!(checks.len(), 1);
        #[cfg(windows)]
        assert!(checks[0].command.starts_with("& '/a path/python'"));
        #[cfg(not(windows))]
        assert!(checks[0].command.starts_with("'/a path/python'"));
        assert!(checks[0].command.contains("test_math.py"));
    }

    #[test]
    fn related_checks_follow_project_metadata_and_source_layout() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        for dir in ["src/agent", "tests", "app", "src", "node_modules/.bin"] {
            std::fs::create_dir_all(path.join(dir)).unwrap();
        }
        std::fs::write(path.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
        std::fs::write(path.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            path.join("src/agent/parser.rs"),
            "#[cfg(test)] mod tests { #[test] fn works() {} }\n",
        )
        .unwrap();
        std::fs::write(path.join("pyproject.toml"), "[tool.pytest.ini_options]\n").unwrap();
        std::fs::write(
            path.join("tests/test_config.py"),
            "def test_config(): pass\n",
        )
        .unwrap();
        std::fs::write(path.join("go.mod"), "module fixture\n").unwrap();
        std::fs::write(path.join("app/main.go"), "package app\n").unwrap();
        std::fs::write(path.join("app/main_test.go"), "package app\n").unwrap();
        std::fs::write(
            path.join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(path.join("src/ui.ts"), "export const ui = true;\n").unwrap();
        std::fs::write(
            path.join("src/ui.test.ts"),
            "import { test } from 'vitest';\n",
        )
        .unwrap();
        let vitest = path.join(if cfg!(windows) {
            "node_modules/.bin/vitest.cmd"
        } else {
            "node_modules/.bin/vitest"
        });
        std::fs::write(&vitest, "").unwrap();

        let checks = related_tests(
            path,
            Path::new("python3"),
            &[
                "src/agent/parser.rs".into(),
                "config.py".into(),
                "app/main.go".into(),
                "src/ui.ts".into(),
            ],
        );
        assert_eq!(checks.len(), 4);
        assert!(checks[0].command.contains("'agent::parser::tests::'"));
        assert!(
            checks[1]
                .command
                .contains("-m pytest -q 'tests/test_config.py'")
        );
        assert_eq!(checks[2].command, "go test './app'");
        assert!(
            checks[3].command.contains("vitest")
                && checks[3].command.contains("run")
                && checks[3].command.contains("src/ui.test.ts")
        );
    }
    #[test]
    fn empty_suites_are_not_behavioral_evidence() {
        assert!(zero_tests("running 0 tests\n"));
        assert!(zero_tests("Ran 0 tests in 0.000s"));
        assert!(!zero_tests("running 4 tests\nrunning 0 tests\n"));
        assert!(zero_tests("? example/pkg [no test files]\n"));
        assert!(zero_tests("No tests found"));
        assert!(!zero_tests("build complete"));
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;

    #[test]
    fn nested_project_checks_execute_in_their_own_environment() {
        let root = tempfile::tempdir().unwrap();
        let service = root.path().join("services/my api");
        std::fs::create_dir_all(service.join("src")).unwrap();
        std::fs::create_dir(service.join("tests")).unwrap();
        std::fs::write(
            service.join("pyproject.toml"),
            "[project]\nname='api'\nversion='1.0'\n",
        )
        .unwrap();
        std::fs::write(service.join("src/app.py"), "answer = 42\n").unwrap();
        std::fs::write(service.join("tests/test_app.py"), "import unittest, sys, pathlib\nclass Test(unittest.TestCase):\n def test_owner(self):\n  self.assertNotEqual(sys.prefix, sys.base_prefix)\n  self.assertTrue(pathlib.Path('src/app.py').is_file())\n").unwrap();
        assert!(
            std::process::Command::new("python3")
                .args(["-m", "venv", "--without-pip"])
                .arg(service.join(".venv"))
                .status()
                .unwrap()
                .success()
        );
        let changed = [PathBuf::from("services/my api/src/app.py")];
        let mut checks = plan(root.path(), Path::new("/missing/root/python"), &changed);
        checks.extend(related_tests(
            root.path(),
            Path::new("/missing/root/python"),
            &changed,
        ));
        assert_eq!(checks.len(), 2);
        for check in checks {
            assert!(check.command.contains("services/my api"));
            let output = std::process::Command::new("sh")
                .args(["-c", &check.command])
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}: {}",
                check.command,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let override_check = plan_with_runner(
            root.path(),
            Path::new("missing"),
            &changed,
            &["python3".into(), "-I".into()],
        )
        .remove(0);
        assert!(override_check.command.contains("'python3' '-I'"));
        assert!(!override_check.command.contains(".venv"));
        assert_eq!(
            python_owner(root.path(), Path::new("../escape.py")),
            root.path()
        );
    }

    #[test]
    fn structured_outcomes_never_convert_setup_or_empty_checks_into_success() {
        let check = FocusedCheck {
            name: "related Python tests: app".into(),
            command: "python -m pytest".into(),
            timeout_secs: 1,
        };
        for (text, error, cancelled, expected) in [
            (
                "1 passed\n[exit code: 0]",
                false,
                false,
                CheckStatus::Passed,
            ),
            (
                "AssertionError\n[exit code: 1]",
                true,
                false,
                CheckStatus::CodeFailed,
            ),
            (
                "No module named pytest\n[exit code: 1]",
                true,
                false,
                CheckStatus::EnvironmentUnavailable,
            ),
            (
                "approval denied",
                true,
                false,
                CheckStatus::PermissionDenied,
            ),
            ("[timed out after 1s]", true, false, CheckStatus::TimedOut),
            ("", true, true, CheckStatus::Cancelled),
            (
                "no tests ran\n[exit code: 0]",
                false,
                false,
                CheckStatus::Unverified,
            ),
        ] {
            let output = crate::tools::ToolOutput {
                execution: Default::default(),
                text: text.into(),
                is_error: error,
                data: None,
            };
            let mut output = output.with_process(crate::runtime::execution::ProcessOutcome {
                started: !matches!(
                    expected,
                    CheckStatus::PermissionDenied | CheckStatus::Cancelled
                ),
                exit_code: match expected {
                    CheckStatus::Passed | CheckStatus::Unverified => Some(0),
                    CheckStatus::CodeFailed | CheckStatus::EnvironmentUnavailable => Some(1),
                    _ => None,
                },
                signal: None,
                timed_out: expected == CheckStatus::TimedOut,
                cancelled,
                output_complete: !matches!(
                    expected,
                    CheckStatus::TimedOut | CheckStatus::Cancelled
                ),
            });
            if expected == CheckStatus::PermissionDenied {
                output = output.with_status(crate::tools::ToolStatus::Blocked);
            }
            assert_eq!(classify(&check, &output, cancelled), expected, "{text}");
        }
    }
}

#[cfg(test)]
mod infrastructure_tests {
    use super::*;

    fn verdict(name: &str, command: &str, diagnostic: &str) -> CheckStatus {
        let check = FocusedCheck {
            name: name.into(),
            command: command.into(),
            timeout_secs: 0,
        };
        let output = crate::tools::ToolOutput {
            execution: Default::default(),
            text: format!("{diagnostic}\n[exit code: 1]"),
            is_error: true,
            data: None,
        };
        let output = output.with_process(crate::runtime::execution::ProcessOutcome {
            started: true,
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        });
        classify(&check, &output, false)
    }

    #[test]
    fn missing_selected_toolchains_are_unavailable() {
        for (name, command, diagnostic) in [
            (
                "cargo check",
                "cargo check --offline",
                "bash: cargo: command not found",
            ),
            (
                "related Rust tests: app::tests::",
                "cargo test --offline --lib",
                "sh: 1: cargo: not found",
            ),
            (
                "cargo check",
                "cargo check --offline",
                "error: rustup could not choose a version of cargo to run, because one wasn't specified explicitly, and no default is configured.",
            ),
            (
                "related Go package tests: ./app",
                "go test './app'",
                "bash: go: command not found",
            ),
            (
                "related vitest test: app.test.ts",
                "'/project/node_modules/.bin/vitest' run 'app.test.ts'",
                "/usr/bin/env: node: No such file or directory",
            ),
            (
                "related jest test: app.test.js",
                "'/project/node_modules/.bin/jest' --runInBand 'app.test.js'",
                "env: node: No such file or directory",
            ),
        ] {
            assert_eq!(
                verdict(name, command, diagnostic),
                CheckStatus::EnvironmentUnavailable,
                "{diagnostic}"
            );
        }
    }

    #[test]
    fn offline_cargo_cache_absence_is_unavailable() {
        for diagnostic in [
            "error: no matching package named `serde` found\nlocation searched: crates.io index\nrequired by package `app v0.1.0`\nAs a reminder, you're using offline mode (--offline) which can sometimes cause surprising resolution failures",
            "error: failed to download `serde v1.0.0`\nCaused by:\n  attempting to make an HTTP request, but --offline was specified",
        ] {
            assert_eq!(
                verdict(
                    "cargo check",
                    "cargo check --offline --all-targets",
                    diagnostic
                ),
                CheckStatus::EnvironmentUnavailable,
                "{diagnostic}"
            );
            assert_eq!(
                verdict(
                    "related Rust tests: app::tests::",
                    "cargo test --offline --lib",
                    diagnostic
                ),
                CheckStatus::EnvironmentUnavailable
            );
            assert_eq!(
                verdict("cargo check", "cargo check", diagnostic),
                CheckStatus::CodeFailed
            );
        }
    }

    #[test]
    fn project_defects_are_not_setup_failures() {
        for (name, command, diagnostic) in [
            (
                "cargo check",
                "cargo check --offline",
                "error[E0308]: mismatched types",
            ),
            (
                "cargo check",
                "cargo check --offline",
                "error: expected `;`, found `}`",
            ),
            (
                "cargo check",
                "cargo check --offline",
                "error: failed to select a version for the requirement `serde = \"^99\"`",
            ),
            (
                "cargo check",
                "cargo check --offline",
                "error: no matching package named `app` found\nlocation searched: /project/app",
            ),
            (
                "related Rust tests: app::tests::",
                "cargo test --offline --lib",
                "thread 'app::tests::answer' panicked: assertion `left == right` failed",
            ),
            (
                "related Go package tests: ./app",
                "go test './app'",
                "app/main.go:3: undefined: answer",
            ),
            (
                "related vitest test: app.test.ts",
                "'/project/node_modules/.bin/vitest' run",
                "SyntaxError: Unexpected token",
            ),
            (
                "related jest test: app.test.js",
                "'/project/node_modules/.bin/jest' --runInBand",
                "Cannot find module './app' from 'app.test.js'",
            ),
            (
                "related Python tests: app",
                "python -m pytest",
                "ModuleNotFoundError: No module named 'app'",
            ),
            (
                "related Python tests: app",
                "python -m pytest",
                "AssertionError: wrong answer",
            ),
            (
                "python AST: app.py",
                "python -c 'import ast'",
                "SyntaxError: invalid syntax",
            ),
            (
                "related Python tests: app",
                "python -m pytest",
                "bash: cargo: command not found",
            ),
            (
                "cargo check",
                "echo cargo",
                "bash: cargo: command not found",
            ),
        ] {
            assert_eq!(
                verdict(name, command, diagnostic),
                CheckStatus::CodeFailed,
                "{diagnostic}"
            );
        }
    }
}
