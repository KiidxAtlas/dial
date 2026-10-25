use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A fixed, focused verification command selected for changed source files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusedCheck {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
}

/// Focused checks for changed paths and the explicit gaps the selection leaves unverified.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RelatedTestPlan {
    pub checks: Vec<FocusedCheck>,
    pub limits: Vec<String>,
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
    pub(in crate::agent) fn repair_exit_code(
        self,
        output: &crate::runtime::tool_output::ToolOutput,
    ) -> Option<i32> {
        let code = output.process_outcome()?.exit_code?;
        match self {
            Self::Passed if code == 0 => Some(code),
            Self::CodeFailed if code != 0 => Some(code),
            _ => None,
        }
    }
}

#[cfg(test)]
pub fn classify(
    check: &FocusedCheck,
    output: &crate::runtime::tool_output::ToolOutput,
    cancelled: bool,
) -> CheckStatus {
    classify_output(Some(check), output, cancelled)
}

/// The one verdict for a finished run, shared by cache, assistance and repair bookkeeping.
/// `check` supplies the configured runner for setup diagnosis; undeclared commands pass `None`.
/// Success requires observed process completion, not printed prose such as `[exit code: 0]`.
pub(in crate::agent) fn classify_output(
    check: Option<&FocusedCheck>,
    output: &crate::runtime::tool_output::ToolOutput,
    cancelled: bool,
) -> CheckStatus {
    classify_with_runner(check, None, output, cancelled)
}

/// Explicit configured runner facts are separate from command names and application imports.
pub(in crate::agent) fn classify_with_runner(
    check: Option<&FocusedCheck>,
    runner: Option<(&str, Option<&str>)>,
    output: &crate::runtime::tool_output::ToolOutput,
    cancelled: bool,
) -> CheckStatus {
    classify_with_artifacts(check, runner, output, cancelled, None)
}

pub(in crate::agent) fn classify_with_artifacts(
    check: Option<&FocusedCheck>,
    runner: Option<(&str, Option<&str>)>,
    output: &crate::runtime::tool_output::ToolOutput,
    cancelled: bool,
    artifacts: Option<&crate::storage::artifact::ArtifactStore>,
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
    let Some(text) = classification_text(output, artifacts) else {
        return CheckStatus::Unverified;
    };
    let empty = zero_tests(&text);
    let complete = process
        .as_ref()
        .is_some_and(|process| process.started && process.output_complete)
        && output.execution.effects != crate::tools::surface::outcome::Effects::Unknown;
    if complete
        && !output.is_error
        && process
            .as_ref()
            .is_some_and(|process| process.exit_code == Some(0))
        && !empty
    {
        return CheckStatus::Passed;
    }
    let runner_launch_observed = runner.is_none()
        || (complete
            && process
                .as_ref()
                .is_some_and(|p| matches!(p.exit_code, Some(1 | 126 | 127))));
    if output.is_error
        && runner_launch_observed
        && check.is_some_and(|check| runner_problem(check, runner, &text).is_some())
    {
        return CheckStatus::EnvironmentUnavailable;
    }
    let Some(process) = process.filter(|_| complete) else {
        return CheckStatus::Unverified;
    };
    if empty || process.exit_code.is_none() || process.exit_code == Some(0) {
        return CheckStatus::Unverified;
    }
    CheckStatus::CodeFailed
}

/// Presentation is not exhaustive evidence. Recover complete retained bytes through the bounded
/// artifact API, never a display link or payload-supplied fact. Oversized/unavailable evidence
/// remains unverified rather than earning a pass or triggering speculative code repair.
fn classification_text<'a>(
    output: &'a crate::runtime::tool_output::ToolOutput,
    artifacts: Option<&crate::storage::artifact::ArtifactStore>,
) -> Option<std::borrow::Cow<'a, str>> {
    use crate::runtime::execution::Completeness;
    use std::borrow::Cow;
    let Some(integrity) = &output.execution.output else {
        // Legacy controller-created results have no presentation transform metadata.
        return Some(Cow::Borrowed(&output.text));
    };
    if integrity.capture != Completeness::Complete {
        return None;
    }
    if integrity.presentation == Completeness::Complete {
        return Some(Cow::Borrowed(&output.text));
    }
    if integrity.retention != Completeness::Complete {
        return None;
    }
    const MAX_CLASSIFICATION_BYTES: u64 = 1024 * 1024;
    let size = integrity.original_bytes?;
    if size > MAX_CLASSIFICATION_BYTES || integrity.retained_bytes != Some(size) {
        return None;
    }
    let store = artifacts?;
    let uri = integrity.artifact.as_deref()?;
    let mut bytes = Vec::with_capacity(size as usize);
    // Even empty artifacts must be opened: metadata alone is not evidence of availability.
    loop {
        let page: serde_json::Value =
            serde_json::from_str(&store.retrieve_bytes(uri, bytes.len() as u64, 16384).ok()?)
                .ok()?;
        if page["total_bytes"].as_u64()? != size {
            return None;
        }
        use base64::Engine;
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(page["data"].as_str()?)
            .ok()?;
        if chunk.len() as u64 > size.saturating_sub(bytes.len() as u64) {
            return None;
        }
        let done = bytes.len() + chunk.len() == size as usize;
        if chunk.is_empty() && !done {
            return None;
        }
        bytes.extend(chunk);
        if done {
            return String::from_utf8(bytes).ok().map(Cow::Owned);
        }
    }
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
pub(in crate::agent) fn zero_tests(output: &str) -> bool {
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
                .and_then(|v| v.strip_suffix(" tests").or_else(|| v.strip_suffix(" test")))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .peekable();
    counts.peek().is_some() && counts.all(|n| n == 0)
}

/// Only compact retained successful checks; preserve test counts and diagnostics
/// in the visible tail, and leave empty suites/failures untouched.
pub(in crate::agent) fn compact_success(output: &str, artifact: &str) -> Option<String> {
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

/// Metadata is useful only for a simple literal launch boundary. Complex shell commands retain
/// ordinary classification rather than treating an application's missing child/import as setup.
pub(in crate::agent) fn runner_problem(
    check: &FocusedCheck,
    runner: Option<(&str, Option<&str>)>,
    output: &str,
) -> Option<&'static str> {
    let Some((program, module)) = runner else {
        return environment_problem(check, output);
    };
    let words = crate::agent::safety::approval::shell_words(&check.command)?;
    if words.first().map(String::as_str) != Some(program) {
        return None;
    }
    // A launch failure has one diagnostic, not an application traceback/assertion plus a
    // missing dependency somewhere in its log. Ambiguous mixed output remains actionable.
    let diagnostics: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("[exit code: "))
        .collect();
    if diagnostics.len() != 1 {
        return None;
    }
    let missing_program = output.lines().any(|line| {
        let line = line.trim();
        line == format!("{program}: command not found")
            || line.ends_with(&format!(": {program}: command not found"))
            || line.ends_with(&format!(": {program}: not found"))
            || line == format!("env: {program}: No such file or directory")
            || line == format!("/usr/bin/env: {program}: No such file or directory")
    });
    let missing_module = module.is_some_and(|module| {
        words.get(1).map(String::as_str) == Some("-m")
            && words.get(2).map(String::as_str) == Some(module)
            && output.lines().any(|line| {
                let line = line.trim();
                // Python's launch error, not a traceback's ModuleNotFoundError in application code.
                let Some((executable, missing)) = line.split_once(": No module named ") else {
                    return false;
                };
                let selected = executable == program
                    || (!program.contains(['/', '\\'])
                        && Path::new(executable).is_absolute()
                        && Path::new(executable)
                            .file_name()
                            .and_then(|name| name.to_str())
                            == Some(program));
                selected && (missing == module || missing == format!("'{module}'"))
            })
    });
    (missing_program || missing_module).then_some(
        "Configured verification runner unavailable (not a confirmed application failure). Restore the declared runner/environment and rerun; installation and network access still require approval.",
    )
}

/// Diagnose explicit setup failures, never reinterpret an assertion or syntax error
/// as success. Missing application modules remain code failures, not setup guesses.
pub(in crate::agent) fn environment_problem(
    check: &FocusedCheck,
    output: &str,
) -> Option<&'static str> {
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
                || matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("Cargo.toml" | "Cargo.lock")
                )
        })
        .collect::<Vec<_>>();

    let mut checks = Vec::new();
    let root_manifest = root.join("Cargo.toml").is_file();
    let workspace = config_contains(root, "Cargo.toml", "[workspace]");
    // Manifest/lock/shared root changes can affect dependents beyond a file's owning package.
    let broaden = root_manifest
        && rust_paths.iter().any(|path| {
            matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("Cargo.toml" | "Cargo.lock")
            ) || cargo_owner(root, path).is_none()
        });
    if !rust_paths.is_empty() && broaden {
        checks.push(FocusedCheck {
            name: "cargo check".to_owned(),
            command: format!(
                "cargo check --offline --all-targets --quiet{}",
                if workspace { " --workspace" } else { "" }
            ),
            timeout_secs: 0,
        });
    } else {
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
                shell_quote("-S"),
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

/// Only the exact generated isolated AST command has automatically known project inputs.
/// Runner wrappers can read arbitrary configuration or execute hooks, so never infer their inputs.
pub(in crate::agent) fn automatic_python_source(
    root: &Path,
    python: &Path,
    override_argv: &[String],
    check: &FocusedCheck,
) -> Option<PathBuf> {
    if !override_argv.is_empty() {
        return None;
    }
    let source = PathBuf::from(check.name.strip_prefix("python AST: ")?);
    if source
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let owner = python_owner(root, &source);
    if scoped_python(root, &owner, python, &[])
        != scoped_python(
            root,
            &owner,
            python,
            &[python.to_string_lossy().into_owned()],
        )
    {
        return None;
    }
    let generated = plan_with_runner(root, python, std::slice::from_ref(&source), &[]);
    (generated.as_slice() == std::slice::from_ref(check)).then_some(source)
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
/// Path-only gaps here; root-aware gaps come from `related_test_plan_with_runner`.
pub fn coverage_limits(changed: &[PathBuf]) -> Vec<String> {
    let mut limits = Vec::new();
    if changed.len() > RELATED_TEST_PATH_LIMIT {
        limits.push(format!(
            "related-test selection covered the first {RELATED_TEST_PATH_LIMIT} of {} changed paths; run broader tests for the rest",
            changed.len()
        ));
    }
    limits.extend(changed.iter().filter_map(|path| static_gap(path)));
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

/// Outcome of mapping one changed Rust path to Cargo tests.
enum RustSelection {
    Check(FocusedCheck),
    /// A gap the caller reports; the path gets compile coverage only.
    Gap(String),
    /// Already reported by `coverage_limits` from the path alone.
    Static,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RustKind {
    Lib,
    Bin,
    Test,
    Example,
}

/// One Cargo target of a package. `root` is workspace-relative. `module_dir` is the directory whose
/// files belong to the target's module tree; `None` for a file-form root that owns only itself.
struct RustTarget {
    kind: RustKind,
    name: String,
    root: PathBuf,
    module_dir: Option<PathBuf>,
}

struct RustPackage {
    dir: PathBuf,
    targets: Vec<RustTarget>,
}

fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|part| !matches!(part, std::path::Component::CurDir))
        .collect()
}

fn read_manifest(root: &Path, manifest: &Path) -> Option<toml::Table> {
    let path = root.join(manifest);
    if std::fs::metadata(&path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn target_at(kind: RustKind, name: String, dir: &Path, rel: &str) -> RustTarget {
    let root = normalize(&dir.join(rel));
    let crate_root = matches!(
        root.file_stem().and_then(|s| s.to_str()),
        Some("lib" | "main")
    );
    let module_dir = crate_root.then(|| root.parent().unwrap_or(Path::new("")).to_path_buf());
    RustTarget {
        kind,
        name,
        root,
        module_dir,
    }
}

fn push_target(targets: &mut Vec<RustTarget>, target: RustTarget) {
    if !targets.iter().any(|existing| existing.root == target.root) {
        targets.push(target);
    }
}

/// Cargo auto-discovers `<sub>/*.rs` and `<sub>/*/main.rs` unless the manifest disables it.
fn discover_targets(
    root: &Path,
    dir: &Path,
    sub: &str,
    kind: RustKind,
    targets: &mut Vec<RustTarget>,
) {
    let base = root.join(dir).join(sub);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    let mut found: Vec<(String, String)> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let (Ok(name), Ok(file_type)) = (entry.file_name().into_string(), entry.file_type()) else {
            continue;
        };
        if file_type.is_file() {
            if let Some(stem) = name.strip_suffix(".rs") {
                found.push((stem.to_owned(), format!("{sub}/{name}")));
            }
        } else if file_type.is_dir() && base.join(&name).join("main.rs").is_file() {
            found.push((name.clone(), format!("{sub}/{name}/main.rs")));
        }
    }
    found.sort();
    for (name, rel) in found {
        push_target(targets, target_at(kind, name, dir, &rel));
    }
}

/// `(name, path)` of each `[[key]]` table in the manifest.
fn declared_targets(manifest: &toml::Table, key: &str) -> Vec<(Option<String>, Option<String>)> {
    manifest
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .map(|table| {
            let text = |field: &str| {
                table
                    .get(field)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            };
            (text("name"), text("path"))
        })
        .collect()
}

/// Reads a package's Cargo targets from its manifest. `None` for a virtual or unreadable manifest.
fn load_package(root: &Path, dir: &Path) -> Option<RustPackage> {
    let manifest = read_manifest(root, &dir.join("Cargo.toml"))?;
    let package = manifest.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?.to_owned();
    let auto = |key: &str| {
        package
            .get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
    };
    let base = root.join(dir);
    let mut targets = Vec::new();

    let lib = manifest.get("lib").and_then(toml::Value::as_table);
    let lib_field = |field: &str| {
        lib.and_then(|table| table.get(field))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    if let Some(rel) = lib_field("path").or_else(|| {
        base.join("src/lib.rs")
            .is_file()
            .then(|| "src/lib.rs".to_owned())
    }) {
        let lib_name = lib_field("name").unwrap_or_else(|| name.replace('-', "_"));
        push_target(&mut targets, target_at(RustKind::Lib, lib_name, dir, &rel));
    }

    // Declared targets come first so their paths win over discovered duplicates.
    for (bin, path) in declared_targets(&manifest, "bin") {
        let Some(bin) = bin else {
            continue;
        };
        let rel = path.unwrap_or_else(|| format!("src/bin/{bin}.rs"));
        push_target(&mut targets, target_at(RustKind::Bin, bin, dir, &rel));
    }
    if auto("autobins") {
        if base.join("src/main.rs").is_file() {
            push_target(
                &mut targets,
                target_at(RustKind::Bin, name.clone(), dir, "src/main.rs"),
            );
        }
        discover_targets(root, dir, "src/bin", RustKind::Bin, &mut targets);
    }
    for (test, path) in declared_targets(&manifest, "test") {
        let Some(test) = test else {
            continue;
        };
        let rel = path.unwrap_or_else(|| format!("tests/{test}.rs"));
        push_target(&mut targets, target_at(RustKind::Test, test, dir, &rel));
    }
    if auto("autotests") {
        discover_targets(root, dir, "tests", RustKind::Test, &mut targets);
    }
    for (example, path) in declared_targets(&manifest, "example") {
        let Some(example) = example else {
            continue;
        };
        let rel = path.unwrap_or_else(|| format!("examples/{example}.rs"));
        push_target(
            &mut targets,
            target_at(RustKind::Example, example, dir, &rel),
        );
    }
    if auto("autoexamples") {
        discover_targets(root, dir, "examples", RustKind::Example, &mut targets);
    }
    Some(RustPackage {
        dir: dir.to_path_buf(),
        targets,
    })
}

/// The nearest manifest at or above the file's directory must be a readable package.
fn owning_package(root: &Path, file: &Path) -> Result<RustPackage, &'static str> {
    let mut dir = file.parent().map(normalize).unwrap_or_default();
    loop {
        if root.join(&dir).join("Cargo.toml").is_file() {
            return load_package(root, &dir).ok_or("manifest is unreadable or not a package");
        }
        if dir.as_os_str().is_empty() {
            return Err("no owning Cargo package");
        }
        dir = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    }
}

/// Deepest directory-owning target that contains `file`; the lib wins ties with the bin at `src/`.
fn owning_target<'a>(package: &'a RustPackage, file: &Path) -> Option<(&'a RustTarget, &'a Path)> {
    let mut best: Option<(&'a RustTarget, &'a Path)> = None;
    for target in &package.targets {
        let Some(module_dir) = target.module_dir.as_deref() else {
            continue;
        };
        if module_dir.as_os_str().is_empty() || !file.starts_with(module_dir) {
            continue;
        }
        if best.is_none_or(|(_, current)| {
            module_dir.components().count() > current.components().count()
        }) {
            best = Some((target, module_dir));
        }
    }
    best
}

/// Maps a changed Rust path to one focused `cargo test`, or to an explicit gap.
fn rust_selection(root: &Path, changed: &Path) -> RustSelection {
    if changed.is_absolute()
        || changed.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return RustSelection::Gap("path is absolute or leaves the workspace".into());
    }
    let file = normalize(changed);
    if static_gap(&file).is_some() {
        return RustSelection::Static;
    }
    if removed_from(root, &file) {
        return RustSelection::Gap("deleted or renamed away; no test target to run".into());
    }
    let package = match owning_package(root, &file) {
        Ok(package) => package,
        Err(reason) => return RustSelection::Gap(reason.into()),
    };
    let source_path = root.join(&file);
    let Ok(metadata) = std::fs::metadata(&source_path) else {
        return RustSelection::Gap("source is unreadable".into());
    };
    if metadata.len() > 1024 * 1024 {
        return RustSelection::Gap("source is too large to inspect".into());
    }
    let Ok(source) = std::fs::read_to_string(&source_path) else {
        return RustSelection::Gap("source is unreadable".into());
    };

    if let Some(target) = package.targets.iter().find(|target| target.root == file) {
        if target.kind == RustKind::Test {
            return RustSelection::Check(cargo_check(&package, target, None));
        }
        if declares_tests_module(&source) {
            return RustSelection::Check(cargo_check(&package, target, Some("tests::".into())));
        }
        if has_test_attribute(&source) {
            return RustSelection::Check(cargo_check(&package, target, None));
        }
        return RustSelection::Gap("crate root declares no tests of its own".into());
    }

    // Files under `src/bin` belong to a registered binary only, never to the library's tree.
    let bin_dir = package.dir.join("src/bin");
    let Some((target, module_dir)) = owning_target(&package, &file)
        .filter(|(_, dir)| !file.starts_with(&bin_dir) || dir.starts_with(&bin_dir))
    else {
        return RustSelection::Gap("not inside any registered target's source tree".into());
    };
    let Ok(relative) = file.strip_prefix(module_dir) else {
        return RustSelection::Gap("not inside any registered target's source tree".into());
    };
    let mut module: Vec<String> = relative
        .with_extension("")
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if module.last().is_some_and(|last| last.as_str() == "mod") {
        module.pop();
    }
    if module.is_empty()
        || module
            .iter()
            .any(|part| !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        return RustSelection::Gap("module path is not a plain Rust identifier path".into());
    }
    let module_path = module.join("::");
    let filter = match target.kind {
        RustKind::Test => has_test_attribute(&source).then(|| format!("{module_path}::")),
        _ => match module.iter().position(|part| part.as_str() == "tests") {
            Some(index) => Some(format!("{}::", module[..=index].join("::"))),
            None if declares_tests_module(&source) => Some(format!("{module_path}::tests::")),
            None if has_test_attribute(&source) => Some(format!("{module_path}::")),
            None => {
                return RustSelection::Gap(
                    "no tests in its module; only compilation covers it".into(),
                );
            }
        },
    };
    RustSelection::Check(cargo_check(&package, target, filter))
}

fn cargo_check(package: &RustPackage, target: &RustTarget, filter: Option<String>) -> FocusedCheck {
    let (plain, quoted) = match target.kind {
        RustKind::Lib => ("--lib".to_owned(), "--lib".to_owned()),
        RustKind::Bin => (
            format!("--bin {}", target.name),
            format!("--bin {}", shell_quote(&target.name)),
        ),
        RustKind::Test => (
            format!("--test {}", target.name),
            format!("--test {}", shell_quote(&target.name)),
        ),
        RustKind::Example => (
            format!("--example {}", target.name),
            format!("--example {}", shell_quote(&target.name)),
        ),
    };
    let label = if package.dir.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        package.dir.to_string_lossy().into_owned()
    };
    let filter_arg = filter
        .as_deref()
        .map(|text| format!(" {}", shell_quote(text)))
        .unwrap_or_default();
    let filter_name = filter
        .as_deref()
        .map(|text| format!(" {text}"))
        .unwrap_or_default();
    FocusedCheck {
        name: format!("related Rust tests: {label} {plain}{filter_name}"),
        command: format!(
            "cargo test --offline --manifest-path {} {quoted}{filter_arg} --quiet -j1",
            shell_quote(&package.dir.join("Cargo.toml").to_string_lossy())
        ),
        timeout_secs: 0,
    }
}

/// `mod tests` declared in the file, inline or external (`mod tests;`).
fn declares_tests_module(source: &str) -> bool {
    source.match_indices("mod tests").any(|(index, _)| {
        let before = source[..index].chars().next_back();
        let after = source[index + "mod tests".len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && matches!(after, Some(' ' | '{' | ';'))
    })
}

fn has_test_attribute(source: &str) -> bool {
    source.contains("#[test]") || source.contains("::test]")
}

/// Path-only gaps that hold whatever the package layout is.
fn static_gap(path: &Path) -> Option<String> {
    let is_rust = path.extension().and_then(|e| e.to_str()) == Some("rs");
    if is_rust && path.file_name().and_then(|n| n.to_str()) == Some("build.rs") {
        return Some(format!(
            "build script {} is compile-only; run broader tests for it",
            path.display()
        ));
    }
    if is_rust
        && path
            .components()
            .any(|part| part.as_os_str().to_str() == Some("benches"))
    {
        return Some(format!(
            "benchmark {} is not run by related-test selection; run broader tests for it",
            path.display()
        ));
    }
    (path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml")).then(|| {
        format!(
            "manifest {} can change target layout; run broader tests",
            path.display()
        )
    })
}

fn rust_gap(path: &Path, reason: &str) -> String {
    format!(
        "Rust path {}: {reason}; no related test was selected",
        path.display()
    )
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
    related_test_plan_with_runner(root, python, changed, override_argv).checks
}

/// Focused checks for `changed` plus the explicit gaps the selection could not cover.
pub fn related_test_plan_with_runner(
    root: &Path,
    python: &Path,
    changed: &[PathBuf],
    override_argv: &[String],
) -> RelatedTestPlan {
    related_test_plan_with_metadata(
        root,
        python,
        changed,
        override_argv,
        || javascript_test_runner(root),
        |directory| has_go_package_tests(root, directory),
    )
}

#[cfg(test)]
// Keep filesystem metadata resolution injectable for deterministic per-invocation work counts.
fn related_tests_with_metadata(
    root: &Path,
    python: &Path,
    changed: &[PathBuf],
    override_argv: &[String],
    javascript_metadata: impl FnMut() -> Option<(&'static str, String)>,
    go_has_tests: impl FnMut(&Path) -> bool,
) -> Vec<FocusedCheck> {
    related_test_plan_with_metadata(
        root,
        python,
        changed,
        override_argv,
        javascript_metadata,
        go_has_tests,
    )
    .checks
}

fn related_test_plan_with_metadata(
    root: &Path,
    python: &Path,
    changed: &[PathBuf],
    override_argv: &[String],
    mut javascript_metadata: impl FnMut() -> Option<(&'static str, String)>,
    mut go_has_tests: impl FnMut(&Path) -> bool,
) -> RelatedTestPlan {
    let mut selected = Vec::new();
    let mut gaps = Vec::new();
    let go_project = root.join("go.mod").is_file();
    // Invocation-local metadata only: later edits must be visible on the next selection pass.
    let mut javascript_runner = None;
    let mut go_packages = HashMap::new();
    for path in changed.iter().take(RELATED_TEST_PATH_LIMIT) {
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            match rust_selection(root, path) {
                RustSelection::Check(check) => selected.push(check),
                RustSelection::Gap(reason) => gaps.push(rust_gap(path, &reason)),
                RustSelection::Static => {}
            }
            continue;
        }
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
    let mut limits = coverage_limits(changed);
    for gap in gaps {
        if !limits.contains(&gap) {
            limits.push(gap);
        }
    }
    RelatedTestPlan {
        checks: selected,
        limits,
    }
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

#[cfg(test)]
mod rust_selection_tests;
