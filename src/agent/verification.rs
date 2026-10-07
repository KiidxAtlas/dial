use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A fixed, focused verification command selected for changed source files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusedCheck {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
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

/// Plans safe syntax/type checks for supported changed files without running them.
pub fn plan(root: &Path, python: &Path, changed_paths: &[PathBuf]) -> Vec<FocusedCheck> {
    let has_rust_changes = changed_paths.iter().any(|path| {
        path.extension().and_then(|extension| extension.to_str()) == Some("rs")
            || path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
    });

    let mut checks = Vec::new();
    if has_rust_changes && root.join("Cargo.toml").is_file() {
        checks.push(FocusedCheck {
            name: "cargo check".to_owned(),
            command: "cargo check --offline --all-targets --quiet".to_owned(),
            timeout_secs: 120,
        });
    }

    let mut seen_python_paths = HashSet::new();
    for path in changed_paths {
        if path.extension().and_then(|extension| extension.to_str()) != Some("py")
            || !seen_python_paths.insert(path)
        {
            continue;
        }

        let path = path.to_string_lossy();
        let python_code = "import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding='utf-8'), filename=p)";
        checks.push(FocusedCheck {
            name: format!("python AST: {path}"),
            command: [
                shell_quote(&python.to_string_lossy()),
                shell_quote("-I"),
                shell_quote("-c"),
                shell_quote(python_code),
                shell_quote(&path),
            ]
            .join(" "),
            timeout_secs: 30,
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
    let executable = root.join("node_modules/.bin").join(runner);
    if !executable.is_file() {
        return None;
    }
    let mut command = format!("{} ", shell_quote(&executable.to_string_lossy()));
    if runner == "vitest" {
        command.push_str("run ");
    } else {
        command.push_str("--runInBand ");
    }
    command.push_str(&shell_quote(&test.to_string_lossy()));
    Some(FocusedCheck {
        name: format!("related {runner} test: {}", test.display()),
        command,
        timeout_secs: 120,
    })
}

/// Conservative test mapping; ambiguous ownership leaves broader configured checks intact.
/// Selected tests supplement syntax/type checks, never independent behavioral evidence.
pub fn related_tests(root: &Path, python: &Path, changed: &[PathBuf]) -> Vec<FocusedCheck> {
    let mut selected = Vec::new();
    let pytest = pytest_configured(root);
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
                let test = root.join("tests").join(format!("test_{stem}.py"));
                if test.is_file() {
                    let relative = test.strip_prefix(root).unwrap_or(&test);
                    let command = if pytest {
                        format!(
                            "{} -m pytest -q {}",
                            shell_quote(&python.to_string_lossy()),
                            shell_quote(&relative.to_string_lossy())
                        )
                    } else {
                        format!(
                            "{} -m unittest discover -s tests -p {}",
                            shell_quote(&python.to_string_lossy()),
                            shell_quote(&format!("test_{stem}.py"))
                        )
                    };
                    selected.push(FocusedCheck {
                        name: format!("related Python tests: {stem}"),
                        command,
                        timeout_secs: 60,
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
                        timeout_secs: 120,
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
                        timeout_secs: 120,
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

fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\"'\"'"))
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
        assert_eq!(checks[0].timeout_secs, 120);
        assert_eq!(checks[1].name, "python AST: scripts/check.py");
        assert!(checks[1].command.contains("'scripts/check.py'"));
        assert_eq!(checks[1].timeout_secs, 30);
    }

    #[test]
    fn shell_quotes_executable_and_path_with_spaces_and_single_quotes() {
        let root = TempDir::new();
        let changed = [PathBuf::from("dir with space/a'b.py")];

        let checks = plan(&root.0, Path::new("/opt/python's bin/python"), &changed);

        assert_eq!(checks.len(), 1);
        assert_eq!(
            checks[0].command,
            "'/opt/python'\"'\"'s bin/python' '-I' '-c' 'import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding='\"'\"'utf-8'\"'\"'), filename=p)' 'dir with space/a'\"'\"'b.py'"
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
        assert!(checks[0].command.starts_with("'/a path/python'"));
        assert!(checks[0].command.contains("'test_math.py'"));
    }

    #[test]
    fn related_checks_follow_project_metadata_and_source_layout() {
        use std::os::unix::fs::PermissionsExt;
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
        let vitest = path.join("node_modules/.bin/vitest");
        std::fs::write(&vitest, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&vitest).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&vitest, permissions).unwrap();

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
        assert!(checks[3].command.contains("vitest' run 'src/ui.test.ts'"));
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
