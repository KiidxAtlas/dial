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
    if output.contains("Ran 0 tests") || output.contains("no tests ran") {
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

/// Conservative direct-test mapping. Ambiguous changes retain configured full checks.
/// A selected command is supplementary, never an independent training label.
pub fn related_tests(root: &Path, python: &Path, changed: &[PathBuf]) -> Vec<FocusedCheck> {
    let mut selected = Vec::new();
    for path in changed.iter().take(32) {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("py") {
            let test = root.join("tests").join(format!("test_{stem}.py"));
            if test.is_file() {
                selected.push(FocusedCheck {
                    name: format!("related tests: {stem}"),
                    command: format!(
                        "{} -m unittest discover -s tests -p {}",
                        shell_quote(&python.to_string_lossy()),
                        shell_quote(&format!("test_{stem}.py"))
                    ),
                    timeout_secs: 60,
                });
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs")
            && path.parent() == Some(Path::new("src"))
            && !["lib", "main", "mod"].contains(&stem)
            && root.join("src/lib.rs").is_file()
        {
            let source = std::fs::metadata(root.join(path))
                .ok()
                .filter(|m| m.len() <= 1024 * 1024)
                .and_then(|_| std::fs::read_to_string(root.join(path)).ok());
            if source.is_some_and(|s| s.contains("#[test]") && s.contains("mod tests")) {
                selected.push(FocusedCheck {
                    name: format!("related tests: {stem}"),
                    command: format!(
                        "cargo test --offline --lib {} --quiet -j1",
                        shell_quote(&format!("{stem}::tests::"))
                    ),
                    timeout_secs: 120,
                });
            }
        }
    }
    let mut seen = HashSet::new();
    selected.retain(|c| seen.insert(c.command.clone()));
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
    fn empty_suites_are_not_behavioral_evidence() {
        assert!(zero_tests("running 0 tests\n"));
        assert!(zero_tests("Ran 0 tests in 0.000s"));
        assert!(!zero_tests("running 4 tests\nrunning 0 tests\n"));
        assert!(!zero_tests("build complete"));
    }
}
