//! Checker discovery and execution.
//!
//! Discovery is static: it inspects the project's manifests and source files and resolves executables
//! from project-local bin directories and `PATH`. Nothing runs until [`Suite::run`].

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::super::loop_runner::{collect, read_in_background};
use super::parsers::{self, Ctx, Kind};
use super::repair::Checker;
use super::{CheckResult, Diagnostic, Report, Severity, Skipped, dedupe_project, rel_string};
use crate::tools::shell::ProcessGroup;

/// Directories that are never scanned for checker roots.
const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".terraform",
    ".venv",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "vendor",
];

/// Project-local bin directories searched before `PATH`.
const LOCAL_BIN_DIRS: &[&str] = &["node_modules/.bin", ".venv/bin", "venv/bin", "vendor/bin"];

/// ShellCheck receives at most this many files per invocation.
const SHELLCHECK_BATCH_SIZE: usize = 200;

/// Output kept from a failed checker's stdout/stderr.
const FAILURE_DETAIL_LIMIT: usize = 12000;

/// One runnable checker command.
#[derive(Clone, Debug)]
pub struct Plan {
    pub id: String,
    pub label: String,
    /// Shown to the user; the binary is shown by name, not by its resolved path.
    pub display: String,
    cwd: PathBuf,
    argv: Vec<String>,
    parser: Kind,
    mutates: bool,
}

/// Discovered or requested checkers for one project, plus the ones that could not run.
#[derive(Clone, Debug)]
pub struct Suite {
    root: PathBuf,
    plans: Vec<Plan>,
    skipped: Vec<Skipped>,
}

/// A checker spec from a free-form request (scout output).
#[derive(Debug, Deserialize)]
pub struct CustomSpec {
    pub label: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub parser: Option<String>,
}

impl Suite {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            plans: Vec::new(),
            skipped: Vec::new(),
        }
    }

    pub fn plans(&self) -> &[Plan] {
        &self.plans
    }

    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// Keeps only the plans `keep` accepts; used by the checker picker.
    pub fn retain(&mut self, keep: impl Fn(&Plan) -> bool) {
        self.plans.retain(|plan| keep(plan));
    }

    /// `label: command` per plan, for repair prompts.
    pub fn commands(&self) -> Vec<String> {
        self.plans
            .iter()
            .map(|plan| format!("{}: {}", plan.label, plan.display))
            .collect()
    }

    /// Runs mutating checkers one at a time, then the rest in parallel. Results keep plan order.
    pub fn run(&self, cancelled: &(dyn Fn() -> bool + Sync)) -> Report {
        let mut results: Vec<Option<CheckResult>> = (0..self.plans.len()).map(|_| None).collect();
        for (index, plan) in self
            .plans
            .iter()
            .enumerate()
            .filter(|(_, plan)| plan.mutates)
        {
            results[index] = Some(run_plan(plan, &self.root, cancelled));
        }
        thread::scope(|scope| {
            let handles: Vec<(usize, thread::ScopedJoinHandle<'_, CheckResult>)> = self
                .plans
                .iter()
                .enumerate()
                .filter(|(_, plan)| !plan.mutates)
                .map(|(index, plan)| {
                    let root = self.root.as_path();
                    (index, scope.spawn(move || run_plan(plan, root, cancelled)))
                })
                .collect();
            for (index, handle) in handles {
                results[index] = Some(handle.join().unwrap_or_else(|_| CheckResult {
                    diagnostics: vec![failure(
                        &self.plans[index].label,
                        None,
                        "",
                        "checker thread panicked",
                    )],
                }));
            }
        });
        let checks: Vec<CheckResult> = results.into_iter().flatten().collect();
        let diagnostics = dedupe_project(
            checks
                .iter()
                .flat_map(|check| check.diagnostics.iter().cloned())
                .collect(),
        );
        Report { diagnostics }
    }
}

impl Checker for Suite {
    fn check(&self, cancelled: &(dyn Fn() -> bool + Sync)) -> Report {
        self.run(cancelled)
    }
}

/// Runs one checker in its own process group. Nonzero exit with no parsed diagnostics becomes a
/// failure diagnostic. A cancelled checker is killed and reports no diagnostics.
fn run_plan(plan: &Plan, root: &Path, cancelled: &(dyn Fn() -> bool + Sync)) -> CheckResult {
    if cancelled() {
        return CheckResult {
            diagnostics: Vec::new(),
        };
    }
    let mut command = Command::new(&plan.argv[0]);
    command
        .args(&plan.argv[1..])
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = match crate::platform::process::spawn_group(&mut command) {
        Ok(child) => ProcessGroup::new(child),
        Err(error) => {
            return CheckResult {
                diagnostics: vec![failure(&plan.label, None, "", &error.to_string())],
            };
        }
    };
    let stdout = group.child.stdout.take().map(read_in_background);
    let stderr = group.child.stderr.take().map(read_in_background);
    let status = loop {
        match group.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                group.terminate();
                return CheckResult {
                    diagnostics: vec![failure(&plan.label, None, "", &error.to_string())],
                };
            }
        }
        if cancelled() {
            group.terminate();
            return CheckResult {
                diagnostics: Vec::new(),
            };
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = collect(stdout);
    let stderr = collect(stderr);
    let ctx = Ctx {
        checker: &plan.label,
        cwd: &plan.cwd,
        root,
    };
    let mut diagnostics = parsers::parse(plan.parser, &ctx, &stdout, &stderr);
    if !status.success() && diagnostics.is_empty() {
        diagnostics.push(failure(&plan.label, status.code(), &stdout, &stderr));
    }
    CheckResult { diagnostics }
}

fn failure(label: &str, exit: Option<i32>, stdout: &str, stderr: &str) -> Diagnostic {
    let code = exit
        .map(|code| format!(" with exit code {code}"))
        .unwrap_or_default();
    let details = [stderr, stdout]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let details: String = parsers::sanitize(&details)
        .trim()
        .chars()
        .take(FAILURE_DETAIL_LIMIT)
        .collect();
    let message = if details.is_empty() {
        format!("{label} failed{code}")
    } else {
        format!("{label} failed{code}: {details}")
    };
    Diagnostic {
        checker: label.to_owned(),
        file: None,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        code: None,
        severity: Severity::Error,
        message,
        suggestion: None,
    }
}

/// Static discovery of the project's checkers for the languages Dial supports.
pub fn discover(root: &Path, include_tests: bool) -> Suite {
    let files = project_files(root);
    let mut suite = Suite::new(root);
    rust(&mut suite, root, &files, include_tests);
    go(&mut suite, root, &files, include_tests);
    python(&mut suite, root, &files, include_tests);
    javascript(&mut suite, root, &files, include_tests);
    shell(&mut suite, root, &files);
    suite
}

/// Checkers from a free-form request. Unknown parsers and missing executables are skipped.
pub fn custom(root: &Path, specs: Vec<CustomSpec>) -> Suite {
    let mut suite = Suite::new(root);
    for (index, spec) in specs.into_iter().enumerate() {
        let label = spec.label.trim().to_owned();
        let cwd_rel = spec.cwd.as_deref().unwrap_or(".").trim().to_owned();
        let cwd = root.join(&cwd_rel);
        if !cwd.is_dir() {
            skip(&mut suite, label, format!("cwd not found: {cwd_rel}"));
            continue;
        }
        let Some(program) = spec.command.first().cloned() else {
            skip(&mut suite, label, "empty command".to_owned());
            continue;
        };
        let parser = match spec.parser.as_deref() {
            None => Kind::Generic,
            Some(id) => match Kind::from_id(id) {
                Some(kind) => kind,
                None => {
                    skip(&mut suite, label, format!("unsupported parser: {id}"));
                    continue;
                }
            },
        };
        let program_path = if program.contains('/') {
            let candidate = cwd.join(&program);
            candidate
                .is_file()
                .then(|| candidate.to_string_lossy().into_owned())
        } else {
            path_binary(&program).then(|| program.clone())
        };
        let Some(program_path) = program_path else {
            skip(
                &mut suite,
                label,
                format!("executable not found: {program}"),
            );
            continue;
        };
        let display = format_command(&spec.command);
        let mut argv = vec![program_path];
        argv.extend(spec.command.into_iter().skip(1));
        suite.plans.push(Plan {
            id: format!("custom-{index}"),
            display,
            label,
            cwd,
            argv,
            parser,
            mutates: false,
        });
    }
    suite
}

/// Declarative description of one discovered checker.
struct Spec {
    id: String,
    label: String,
    binaries: &'static [&'static str],
    args: Vec<String>,
    parser: Kind,
    /// Project-relative directory the checker runs in.
    dir: String,
    mutates: bool,
    /// When true, a missing executable skips the checker silently.
    optional: bool,
}

fn push(suite: &mut Suite, root: &Path, spec: Spec) {
    let cwd = root.join(&spec.dir);
    let Some((name, program)) = resolve_binary(spec.binaries, &cwd, root) else {
        if !spec.optional {
            skip(
                suite,
                spec.label,
                format!("executable not found: {}", spec.binaries.join(" or ")),
            );
        }
        return;
    };
    let mut argv = vec![program];
    argv.extend(spec.args.iter().cloned());
    let mut shown = vec![name];
    shown.extend(spec.args);
    suite.plans.push(Plan {
        id: spec.id,
        label: spec.label,
        display: format_command(&shown),
        cwd,
        argv,
        parser: spec.parser,
        mutates: spec.mutates,
    });
}

fn skip(suite: &mut Suite, label: String, reason: String) {
    suite.skipped.push(Skipped { label, reason });
}

fn rust(suite: &mut Suite, root: &Path, files: &[String], include_tests: bool) {
    for dir in roots_named(files, &["Cargo.toml"]) {
        let label = format!("cargo clippy ({dir})");
        let cwd = root.join(&dir);
        let Some((_, cargo)) = resolve_binary(&["cargo"], &cwd, root) else {
            skip(suite, label, "executable not found: cargo".to_owned());
            continue;
        };
        let packages = match cargo_packages(&cargo, &cwd, root, files) {
            Ok(packages) => packages,
            Err(error) => {
                skip(suite, label, format!("cargo metadata failed: {error}"));
                continue;
            }
        };
        if packages.is_empty() {
            skip(
                suite,
                label,
                "no first-party workspace packages found".to_owned(),
            );
            continue;
        }
        let package_args: Vec<String> = packages
            .iter()
            .flat_map(|name| ["--package".to_owned(), name.clone()])
            .collect();

        let mut clippy: Vec<String> = [
            "clippy",
            "--fix",
            "--allow-dirty",
            "--all-targets",
            "--no-deps",
            "--allow-staged",
            "--broken-code",
            "--allow-no-vcs",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        clippy.extend(package_args.iter().cloned());
        clippy.extend(
            ["--message-format=json", "--", "-D", "warnings"]
                .into_iter()
                .map(str::to_owned),
        );
        push(
            suite,
            root,
            Spec {
                id: format!("clippy:{dir}"),
                label,
                binaries: &["cargo"],
                args: clippy,
                parser: Kind::Rust,
                dir: dir.clone(),
                mutates: true,
                optional: false,
            },
        );

        if include_tests {
            let mut test: Vec<String> = ["test", "--no-fail-fast", "--all-targets"]
                .into_iter()
                .map(str::to_owned)
                .collect();
            test.extend(package_args);
            test.push("--message-format=json".to_owned());
            push(
                suite,
                root,
                Spec {
                    id: format!("cargo-test:{dir}"),
                    label: format!("cargo test ({dir})"),
                    binaries: &["cargo"],
                    args: test,
                    parser: Kind::RustTest,
                    dir,
                    mutates: false,
                    optional: false,
                },
            );
        }
    }
}

/// Names of first-party packages in the workspace rooted at `cwd` whose manifests are tracked.
fn cargo_packages(
    cargo: &str,
    cwd: &Path,
    root: &Path,
    files: &[String],
) -> Result<Vec<String>, String> {
    let manifest = cwd.join("Cargo.toml");
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .current_dir(cwd)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
    let members: HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let tracked: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut names = BTreeSet::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let (Some(id), Some(name), Some(manifest_path)) = (
            package["id"].as_str(),
            package["name"].as_str(),
            package["manifest_path"].as_str(),
        ) else {
            continue;
        };
        if !members.contains(id) {
            continue;
        }
        let Ok(relative) = Path::new(manifest_path).strip_prefix(root) else {
            continue;
        };
        if tracked.contains(rel_string(relative).as_str()) {
            names.insert(name.to_owned());
        }
    }
    Ok(names.into_iter().collect())
}

fn go(suite: &mut Suite, root: &Path, files: &[String], include_tests: bool) {
    let mut roots = roots_named(files, &["go.work"]);
    if roots.is_empty() {
        roots = roots_named(files, &["go.mod"]);
    }
    for dir in roots {
        push(
            suite,
            root,
            Spec {
                id: format!("go-vet:{dir}"),
                label: format!("go vet ({dir})"),
                binaries: &["go"],
                args: strings(&["vet", "-json", "./..."]),
                parser: Kind::Go,
                dir: dir.clone(),
                mutates: false,
                optional: false,
            },
        );
        push(
            suite,
            root,
            Spec {
                id: format!("staticcheck:{dir}"),
                label: format!("staticcheck ({dir})"),
                binaries: &["staticcheck"],
                args: strings(&["-f", "json", "./..."]),
                parser: Kind::Staticcheck,
                dir: dir.clone(),
                mutates: false,
                optional: !has_file(files, &join_rel(&dir, "staticcheck.conf")),
            },
        );
        if include_tests {
            push(
                suite,
                root,
                Spec {
                    id: format!("go-test:{dir}"),
                    label: format!("go test ({dir})"),
                    binaries: &["go"],
                    args: strings(&["test", "-json", "./..."]),
                    parser: Kind::GoTest,
                    dir: dir.clone(),
                    mutates: false,
                    optional: false,
                },
            );
        }
    }
    let golangci = regex(r"(?:^|/)\.golangci\.(?:ya?ml|toml|json)$");
    for dir in roots_matching(files, &golangci) {
        push(
            suite,
            root,
            Spec {
                id: format!("golangci:{dir}"),
                label: format!("golangci-lint ({dir})"),
                binaries: &["golangci-lint"],
                args: strings(&["run"]),
                parser: Kind::Golangci,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }
}

fn python(suite: &mut Suite, root: &Path, files: &[String], include_tests: bool) {
    if !files
        .iter()
        .any(|file| file.ends_with(".py") || file.ends_with(".pyi"))
    {
        return;
    }
    let mut ruff_roots = roots_named(
        files,
        &[
            ".ruff.toml",
            "pyproject.toml",
            "ruff.toml",
            "setup.cfg",
            "tox.ini",
        ],
    );
    if ruff_roots.is_empty() {
        ruff_roots.push(".".to_owned());
    }
    for dir in &ruff_roots {
        push(
            suite,
            root,
            Spec {
                id: format!("ruff:{dir}"),
                label: format!("ruff ({dir})"),
                binaries: &["ruff"],
                args: strings(&["check", "--output-format=json", "."]),
                parser: Kind::Ruff,
                dir: dir.clone(),
                mutates: false,
                optional: false,
            },
        );
        if include_tests {
            push(
                suite,
                root,
                Spec {
                    id: format!("pytest:{dir}"),
                    label: format!("pytest ({dir})"),
                    binaries: &["pytest"],
                    args: strings(&["-q"]),
                    parser: Kind::Generic,
                    dir: dir.clone(),
                    mutates: false,
                    optional: false,
                },
            );
        }
    }

    /// `(name, config files, pyproject section, binaries, args, parser)`.
    type PythonTool<'a> = (
        &'a str,
        &'a [&'a str],
        Option<&'a str>,
        &'static [&'static str],
        Vec<&'a str>,
        Kind,
    );
    let tools: [PythonTool<'_>; 5] = [
        (
            "pyright",
            &["pyrightconfig.json"],
            None,
            &["pyright", "basedpyright"],
            vec!["--outputjson"],
            Kind::Pyright,
        ),
        (
            "mypy",
            &["mypy.ini", ".mypy.ini"],
            Some("[tool.mypy"),
            &["mypy"],
            vec!["--no-error-summary", "--no-pretty", "."],
            Kind::Generic,
        ),
        (
            "pylint",
            &[".pylintrc", "pylintrc"],
            Some("[tool.pylint"),
            &["pylint"],
            vec!["--output-format=json", "--recursive=y", "."],
            Kind::Pylint,
        ),
        (
            "flake8",
            &[".flake8"],
            None,
            &["flake8"],
            vec!["."],
            Kind::Flake8,
        ),
        (
            "ty",
            &["ty.toml"],
            Some("[tool.ty"),
            &["ty"],
            vec!["check", "--output-format", "concise"],
            Kind::Ty,
        ),
    ];
    for (name, names, needle, binaries, args, parser) in tools {
        for dir in config_roots(root, files, names, needle) {
            push(
                suite,
                root,
                Spec {
                    id: format!("{name}:{dir}"),
                    label: format!("{name} ({dir})"),
                    binaries,
                    args: strings(&args),
                    parser,
                    dir,
                    mutates: false,
                    optional: false,
                },
            );
        }
    }
}

fn javascript(suite: &mut Suite, root: &Path, files: &[String], include_tests: bool) {
    const EXTENSIONS: &[&str] = &[
        ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
    ];
    if !files
        .iter()
        .any(|file| EXTENSIONS.iter().any(|ext| file.ends_with(ext)))
    {
        return;
    }

    for dir in roots_named(files, &["biome.json", "biome.jsonc"]) {
        push(
            suite,
            root,
            Spec {
                id: format!("biome:{dir}"),
                label: format!("biome check ({dir})"),
                binaries: &["biome"],
                args: strings(&["check", ".", "--reporter=json"]),
                parser: Kind::Biome,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    let eslint = regex(
        r"(?:^|/)(eslint\.config\.(?:js|mjs|cjs|ts|mts|cts)|\.eslintrc(?:\.(?:js|cjs|json|yaml|yml))?)$",
    );
    for dir in roots_matching(files, &eslint) {
        push(
            suite,
            root,
            Spec {
                id: format!("eslint:{dir}"),
                label: format!("eslint ({dir})"),
                binaries: &["eslint"],
                args: strings(&[".", "--format=json"]),
                parser: Kind::Eslint,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    for dir in roots_named(files, &["tsconfig.json"]) {
        let prefix = if dir == "." {
            String::new()
        } else {
            format!("{dir}/")
        };
        let vue = files
            .iter()
            .any(|file| file.starts_with(&prefix) && file.ends_with(".vue"));
        let (label, binaries): (String, &'static [&'static str]) = if vue {
            (
                format!("Vue TypeScript ({dir})"),
                &["vue-tsc", "tsgo", "tsc"],
            )
        } else {
            (format!("TypeScript ({dir})"), &["tsgo", "tsc"])
        };
        push(
            suite,
            root,
            Spec {
                id: format!("typescript:{dir}"),
                label,
                binaries,
                args: strings(&["--noEmit", "--pretty", "false"]),
                parser: Kind::Generic,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    for dir in roots_named(files, &[".oxlintrc.json"]) {
        push(
            suite,
            root,
            Spec {
                id: format!("oxlint:{dir}"),
                label: format!("oxlint ({dir})"),
                binaries: &["oxlint"],
                args: strings(&["--format=unix", "."]),
                parser: Kind::Oxlint,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    for dir in roots_named(files, &["deno.json", "deno.jsonc"]) {
        push(
            suite,
            root,
            Spec {
                id: format!("deno-lint:{dir}"),
                label: format!("deno lint ({dir})"),
                binaries: &["deno"],
                args: strings(&["lint", "--json"]),
                parser: Kind::DenoLint,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    let stylelint = regex(
        r"(?:^|/)(?:\.stylelintrc(?:\.(?:json|yaml|yml|js|cjs|mjs))?|stylelint\.config\.(?:js|cjs|mjs|ts))$",
    );
    for dir in roots_matching(files, &stylelint) {
        push(
            suite,
            root,
            Spec {
                id: format!("stylelint:{dir}"),
                label: format!("stylelint ({dir})"),
                binaries: &["stylelint"],
                args: strings(&[
                    "**/*.{css,scss,sass,less}",
                    "--formatter",
                    "json",
                    "--allow-empty-input",
                ]),
                parser: Kind::Stylelint,
                dir,
                mutates: false,
                optional: false,
            },
        );
    }

    if include_tests {
        for dir in topmost_roots(package_test_roots(root, files)) {
            let manager = package_manager(root, files, &dir);
            push(
                suite,
                root,
                Spec {
                    id: format!("{manager}-test:{dir}"),
                    label: format!("{manager} test ({dir})"),
                    binaries: manager_binary(manager),
                    args: strings(&["run", "test"]),
                    parser: Kind::Generic,
                    dir,
                    mutates: false,
                    optional: false,
                },
            );
        }
    }
}

/// Directories whose `package.json` defines a real `scripts.test`.
fn package_test_roots(root: &Path, files: &[String]) -> Vec<String> {
    roots_named(files, &["package.json"])
        .into_iter()
        .filter(|dir| {
            let path = root.join(join_rel(dir, "package.json"));
            let Ok(text) = std::fs::read_to_string(path) else {
                return false;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            value["scripts"]["test"].as_str().is_some_and(|script| {
                !script.trim().is_empty() && !script.contains("no test specified")
            })
        })
        .collect()
}

/// Package manager named by `packageManager`, else by lockfile. Defaults to npm.
fn package_manager(root: &Path, files: &[String], dir: &str) -> &'static str {
    let declared = std::fs::read_to_string(root.join(join_rel(dir, "package.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["packageManager"].as_str().map(str::to_owned));
    if let Some(declared) = declared {
        let name = declared.split('@').next().unwrap_or_default();
        match name {
            "bun" => return "bun",
            "npm" => return "npm",
            "pnpm" => return "pnpm",
            "yarn" => return "yarn",
            _ => {}
        }
    }
    if has_file(files, &join_rel(dir, "bun.lock")) || has_file(files, &join_rel(dir, "bun.lockb")) {
        "bun"
    } else if has_file(files, &join_rel(dir, "pnpm-lock.yaml")) {
        "pnpm"
    } else if has_file(files, &join_rel(dir, "yarn.lock")) {
        "yarn"
    } else {
        "npm"
    }
}

fn manager_binary(manager: &str) -> &'static [&'static str] {
    match manager {
        "bun" => &["bun"],
        "pnpm" => &["pnpm"],
        "yarn" => &["yarn"],
        _ => &["npm"],
    }
}

fn shell(suite: &mut Suite, root: &Path, files: &[String]) {
    let scripts: Vec<&String> = files
        .iter()
        .filter(|file| {
            [".sh", ".bash", ".ksh", ".zsh"]
                .iter()
                .any(|ext| file.ends_with(ext))
        })
        .collect();
    for (index, batch) in scripts.chunks(SHELLCHECK_BATCH_SIZE).enumerate() {
        let mut args = vec!["--format=json1".to_owned()];
        args.extend(batch.iter().map(|file| (*file).clone()));
        push(
            suite,
            root,
            Spec {
                id: format!("shellcheck-{}", index + 1),
                label: "ShellCheck".to_owned(),
                binaries: &["shellcheck"],
                args,
                parser: Kind::Shellcheck,
                dir: ".".to_owned(),
                mutates: false,
                optional: false,
            },
        );
    }
}

/// Project files relative to the root, with `/` separators, excluding ignored directories.
fn project_files(root: &Path) -> Vec<String> {
    let listed = crate::tools::git::run_checked(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        false,
    );
    let raw: Vec<String> = match listed {
        Ok(stdout) => stdout.split('\0').map(str::to_owned).collect(),
        Err(_) => WalkBuilder::new(root)
            .hidden(false)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter_map(|entry| entry.path().strip_prefix(root).ok().map(rel_string))
            .collect(),
    };
    let files: BTreeSet<String> = raw
        .into_iter()
        .map(|path| path.replace('\\', "/").trim_start_matches("./").to_owned())
        .filter(|path| {
            let segments: Vec<&str> = path.split('/').collect();
            !path.is_empty()
                && segments[..segments.len() - 1]
                    .iter()
                    .all(|segment| !is_ignored_directory(segment))
        })
        .collect();
    files.into_iter().collect()
}

fn is_ignored_directory(segment: &str) -> bool {
    IGNORED_DIRECTORIES.contains(&segment) || segment.starts_with("bazel-")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dirname(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_owned())
        .unwrap_or_else(|| ".".to_owned())
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir == "." {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn has_file(files: &[String], rel: &str) -> bool {
    files.iter().any(|file| file == rel)
}

/// Directories containing a file whose basename is one of `names`.
fn roots_named(files: &[String], names: &[&str]) -> Vec<String> {
    files
        .iter()
        .filter(|file| names.contains(&basename(file)))
        .map(|file| dirname(file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Directories containing a file whose path matches `pattern`.
fn roots_matching(files: &[String], pattern: &Regex) -> Vec<String> {
    files
        .iter()
        .filter(|file| pattern.is_match(file))
        .map(|file| dirname(file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Roots named by `names`, plus roots whose `pyproject.toml` contains `pyproject_needle`.
fn config_roots(
    root: &Path,
    files: &[String],
    names: &[&str],
    pyproject_needle: Option<&str>,
) -> Vec<String> {
    let mut set: BTreeSet<String> = roots_named(files, names).into_iter().collect();
    if let Some(needle) = pyproject_needle {
        for dir in roots_named(files, &["pyproject.toml"]) {
            let path = root.join(join_rel(&dir, "pyproject.toml"));
            if std::fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
                set.insert(dir);
            }
        }
    }
    set.into_iter().collect()
}

fn depth(dir: &str) -> usize {
    if dir == "." {
        0
    } else {
        dir.split('/').count()
    }
}

fn is_within(child: &str, ancestor: &str) -> bool {
    ancestor == "." || child == ancestor || child.starts_with(&format!("{ancestor}/"))
}

/// Drops roots nested inside another root, keeping the shallowest.
fn topmost_roots(mut roots: Vec<String>) -> Vec<String> {
    roots.sort_by(|a, b| depth(a).cmp(&depth(b)).then_with(|| a.cmp(b)));
    let mut kept: Vec<String> = Vec::new();
    for root in roots {
        if !kept.iter().any(|ancestor| is_within(&root, ancestor)) {
            kept.push(root);
        }
    }
    kept.sort();
    kept
}

/// Resolves a checker executable. Project-local bin directories win over `PATH`, and any
/// name matches before `PATH` is consulted. Returns `(name, program)`, where `program` is a path or the name.
fn resolve_binary(names: &[&str], cwd: &Path, root: &Path) -> Option<(String, String)> {
    for name in names {
        if let Some(path) = local_binary(name, cwd, root) {
            return Some(((*name).to_owned(), path));
        }
    }
    for name in names {
        if path_binary(name) {
            return Some(((*name).to_owned(), (*name).to_owned()));
        }
    }
    None
}

fn local_binary(name: &str, cwd: &Path, root: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        for bin_dir in LOCAL_BIN_DIRS {
            let candidate = current.join(bin_dir).join(name);
            if crate::platform::fs::is_executable(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        if current == root {
            break;
        }
        dir = current.parent();
    }
    None
}

fn path_binary(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| crate::platform::fs::is_executable(&dir.join(name)))
    })
}

/// Shell-quotes arguments that contain anything beyond a safe character set.
fn format_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let safe = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./:=@+-".contains(c));
            if safe {
                arg.clone()
            } else {
                serde_json::to_string(arg).unwrap_or_else(|_| arg.clone())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static cleanse checker regex")
}

#[cfg(test)]
mod tests;
