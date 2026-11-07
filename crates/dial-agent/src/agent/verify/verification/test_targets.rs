//! `verify` targets: files or single tests the model names, mapped to the owning project's runner,
//! and the passed/failed counts a runner reports.
//!
//! An entry is `path` or `path::selector`. The selector is a pytest node (`test_div`,
//! `TestCalc::test_div`), a unittest `Class::method`, a Rust or Go test filter, or a jest/vitest
//! test name. Commands are built by the harness; the path and selector are shell-quoted model input.
use super::{
    FocusedCheck, RustSelection, cargo_check, javascript_test_command, javascript_test_runner,
    normalize, owning_package, owning_target, pytest_configured, python_owner, rust_selection,
    scoped_python, shell_quote,
};
use std::path::{Component, Path};

const MAX_ENTRY_BYTES: usize = 4096;
const JAVASCRIPT_EXTENSIONS: [&str; 6] = ["js", "jsx", "ts", "tsx", "mjs", "cjs"];
/// Every target check is named `verify: <entry>`; the controller generated its command.
const NAME_PREFIX: &str = "verify: ";
/// Marks a plain script whose exit status is its only evidence.
const EXIT_ONLY_SUFFIX: &str = " (exit status only)";

/// Test totals reported by a runner's summary lines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
}

/// A test runner selected for a `verify` entry: behavioral evidence like a configured test.
pub fn behavioral_target(name: &str) -> bool {
    name.starts_with(NAME_PREFIX) && !name.ends_with(EXIT_ONLY_SUFFIX)
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
    let (path_text, selector) = match entry.split_once("::") {
        Some((path, selector)) => (path.trim(), Some(selector.trim())),
        None => (entry, None),
    };
    if path_text.is_empty() || entry.len() > MAX_ENTRY_BYTES || entry.chars().any(char::is_control)
    {
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
    let path = workspace_relative(root, Path::new(path_text))?;
    let path = path.as_path();
    if !root.join(path).is_file() {
        return Err(format!(
            "`{path_text}` does not exist; write the test first or name an existing file"
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
        "go" if root.join("go.mod").is_file() => {
            let directory = path.parent().unwrap_or(Path::new(""));
            let package = if directory.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                format!("./{}", directory.to_string_lossy())
            };
            let mut command = format!("go test {}", shell_quote(&package));
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
    let Some(selector) = selector else {
        return match rust_selection(root, path) {
            RustSelection::Check(check) => Ok(check),
            RustSelection::Gap(reason) => Err(format!("`{}`: {reason}", path.display())),
            RustSelection::Static => Err(format!(
                "`{}` has no Rust tests to run; name a test file or `path::test`",
                path.display()
            )),
        };
    };
    let file = normalize(path);
    let package =
        owning_package(root, &file).map_err(|reason| format!("`{}`: {reason}", path.display()))?;
    let target = package
        .targets
        .iter()
        .find(|target| target.root == file)
        .or_else(|| owning_target(root, &package, &file).map(|(target, _)| target))
        .ok_or_else(|| {
            format!(
                "`{}` is not inside any registered target's source tree",
                path.display()
            )
        })?;
    Ok(cargo_check(&package, target, Some(selector.to_owned())))
}

fn javascript_check(root: &Path, path: &Path, selector: Option<&str>) -> Result<String, String> {
    let runner = javascript_test_runner(root).ok_or_else(|| {
        format!(
            "no installed jest or vitest `test` script covers `{}`; run it with bash instead",
            path.display()
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let is_test = name.contains(".test.")
        || name.contains(".spec.")
        || path
            .components()
            .any(|part| part.as_os_str() == "__tests__");
    let mut command = if is_test {
        let (kind, invocation) = &runner;
        let mode = if *kind == "vitest" {
            "run"
        } else {
            "--runInBand"
        };
        format!(
            "{invocation} {mode} {}",
            shell_quote(&path.to_string_lossy())
        )
    } else {
        javascript_test_command(root, path, &runner)
            .map(|check| check.command)
            .ok_or_else(|| format!("no test file was found for `{}`", path.display()))?
    };
    if let Some(selector) = selector {
        command.push_str(&format!(" -t {}", shell_quote(selector)));
    }
    Ok(command)
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
