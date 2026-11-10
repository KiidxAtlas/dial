//! `experimental.changed_line_coverage`: which lines this turn added or changed a passing
//! behavioral check never executed.
//!
//! Collection is injected through the environment only, so the recorded command is unchanged:
//! Python loads a `sitecustomize` collector from `PYTHONPATH` (`sys.monitoring` on 3.12+, else
//! `sys.settrace`, restricted to the changed files), and Node writes V8 block coverage through
//! `NODE_V8_COVERAGE`. Interpreters started with `-I`/`-S`, processes ending with `os._exit` and
//! other languages report nothing; a file the check never loaded is named as such. Evidence only:
//! check status and repair credit never change.

use crate::storage::checkpoint::TurnVersion;
use crate::tools::Workspace;
use crate::tools::exec::shell::{EnvEntry, environment_prefix};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const COLLECTOR: &str = include_str!("coverage/sitecustomize.py");
/// Changed files instrumented per check.
const MAX_FILES: usize = 32;
/// Coverage reports read per check; a test suite spawning more processes is summarized from these.
const MAX_REPORTS: usize = 256;
const MAX_REPORT_BYTES: u64 = 32 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Language {
    Python,
    JavaScript,
}

impl Language {
    fn of(path: &str) -> Option<Self> {
        match Path::new(path).extension()?.to_str()? {
            "py" => Some(Self::Python),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Target {
    path: String,
    absolute: PathBuf,
    language: Language,
    /// 1-based lines added or changed this turn, excluding blank and comment-only lines.
    changed: BTreeSet<usize>,
    source: String,
}

/// One instrumented check run. The scratch directory (and `.tmp`, when this created it) is
/// removed on drop, so declared check inputs see the same tree before and after.
#[derive(Debug)]
pub(in crate::agent) struct Probe {
    dir: PathBuf,
    created_scratch: Option<PathBuf>,
    targets: Vec<Target>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        if let Some(scratch) = &self.created_scratch {
            let _ = std::fs::remove_dir(scratch);
        }
    }
}

fn comment_or_blank(line: &str) -> bool {
    let line = line.trim();
    line.is_empty()
        || ["#", "//", "/*", "*", "*/"]
            .iter()
            .any(|m| line.starts_with(m))
}

/// 1-based lines of `after` that are not present anywhere in `before`.
pub(in crate::agent) fn changed_lines(before: Option<&str>, after: &str) -> BTreeSet<usize> {
    let old: BTreeSet<&str> = before
        .map(|text| text.lines().collect())
        .unwrap_or_default();
    after
        .lines()
        .enumerate()
        .filter(|(_, line)| !old.contains(line) && !comment_or_blank(line))
        .map(|(index, _)| index + 1)
        .collect()
}

/// `path` as interpreters report it: without the Windows verbatim prefix (`\\?\`) that
/// `canonicalize` adds.
fn interpreter_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// Comparison key for a path or `file://` URL: separators unified and, on Windows, a leading
/// slash before a drive letter dropped and case folded.
fn path_key(text: &str) -> String {
    let text = text.strip_prefix("file://").unwrap_or(text);
    let unified = text.replace('\\', "/");
    if !cfg!(windows) {
        return unified;
    }
    let bytes = unified.as_bytes();
    let trimmed = if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
        &unified[1..]
    } else {
        unified.as_str()
    };
    trimmed.to_lowercase()
}

/// Instrument the turn's changed Python and JavaScript files, or `None` when there are none.
pub(in crate::agent) fn prepare(root: &Path, versions: &[TurnVersion]) -> Option<Probe> {
    let targets: Vec<Target> = versions
        .iter()
        .filter(|version| version.in_project)
        .filter_map(|version| {
            let language = Language::of(&version.path)?;
            let source = String::from_utf8(version.after.clone()?).ok()?;
            let before = version
                .before
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok());
            let changed = changed_lines(before, &source);
            if changed.is_empty() {
                return None;
            }
            let absolute = std::fs::canonicalize(root.join(&version.path)).ok()?;
            Some(Target {
                path: version.path.clone(),
                absolute,
                language,
                changed,
                source,
            })
        })
        .take(MAX_FILES)
        .collect();
    if targets.is_empty() {
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let scratch = root.join(crate::workspace_copy::SCRATCH_DIR);
    let created_scratch = (!scratch.exists()).then(|| scratch.clone());
    let dir = scratch.join(format!("coverage-{stamp}-{}", std::process::id()));
    let probe = Probe {
        dir,
        created_scratch,
        targets,
    };
    std::fs::create_dir_all(probe.dir.join("site")).ok()?;
    if probe.has(Language::Python) {
        std::fs::write(probe.dir.join("site/sitecustomize.py"), COLLECTOR).ok()?;
    }
    Some(probe)
}

impl Probe {
    fn has(&self, language: Language) -> bool {
        self.targets
            .iter()
            .any(|target| target.language == language)
    }

    /// The collectors' environment.
    fn environment(&self) -> Vec<EnvEntry> {
        let dir = interpreter_path(&self.dir);
        let separator = if cfg!(windows) { ";" } else { ":" };
        let join = |leaf: &str| Path::new(&dir).join(leaf).to_string_lossy().into_owned();
        let mut entries = Vec::new();
        if self.has(Language::Python) {
            let files: Vec<String> = self
                .targets
                .iter()
                .filter(|target| target.language == Language::Python)
                .map(|target| interpreter_path(&target.absolute))
                .collect();
            entries.push(EnvEntry::Set("DIAL_COVERAGE_DIR".into(), join("py")));
            entries.push(EnvEntry::Set(
                "DIAL_COVERAGE_FILES".into(),
                files.join(separator),
            ));
            entries.push(EnvEntry::Prepend("PYTHONPATH".into(), join("site")));
        }
        if self.has(Language::JavaScript) {
            entries.push(EnvEntry::Set("NODE_V8_COVERAGE".into(), join("node")));
        }
        entries
    }

    /// `command` with the collectors' environment set first, in the syntax of the workspace's
    /// shell; `None` when no shell resolves (the check then runs uninstrumented).
    pub(in crate::agent) fn wrap(&self, ws: &Workspace, command: &str) -> Option<String> {
        environment_prefix(ws, &self.environment(), command).ok()
    }

    /// A line for the check output naming changed lines that never ran, or `None` when every
    /// measured changed line ran or nothing was measured.
    pub(in crate::agent) fn report(&self, check: &str) -> Option<String> {
        let python = read_reports(&self.dir.join("py"), "py-");
        let node = read_reports(&self.dir.join("node"), "coverage-");
        let mut findings = Vec::new();
        let mut measured = 0;
        for target in &self.targets {
            let (ran, outcome) = match target.language {
                Language::Python => (!python.is_empty(), python_unexecuted(target, &python)),
                Language::JavaScript => (!node.is_empty(), node_unexecuted(target, &node)),
            };
            if !ran {
                continue;
            }
            measured += 1;
            match outcome {
                None => findings.push(format!("{}: never loaded by this check", target.path)),
                Some(lines) if !lines.is_empty() => findings.push(format!(
                    "{}: changed line{} {} never executed",
                    target.path,
                    if lines.len() == 1 { "" } else { "s" },
                    ranges(&lines)
                )),
                Some(_) => {}
            }
        }
        if findings.is_empty() {
            return (measured > 0).then(|| {
                format!("[coverage: {check} executed every changed line it measured in {measured} file(s)]")
            });
        }
        Some(format!(
            "[coverage: {check} passed without exercising some of this turn's changes; untested code can still be wrong:\n  {}]",
            findings.join("\n  ")
        ))
    }
}

fn read_reports(dir: &Path, prefix: &str) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(prefix)
                && entry
                    .metadata()
                    .is_ok_and(|meta| meta.len() <= MAX_REPORT_BYTES)
        })
        .take(MAX_REPORTS)
        .filter_map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok())
        .collect()
}

/// Changed executable lines no Python process executed; `None` when no process loaded the file.
fn python_unexecuted(target: &Target, reports: &[serde_json::Value]) -> Option<Vec<usize>> {
    let key = path_key(&interpreter_path(&target.absolute));
    let numbers = |value: &serde_json::Value| -> BTreeSet<usize> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| usize::try_from(n.as_u64()?).ok())
            .collect()
    };
    let mut executed = BTreeSet::new();
    let mut executable = None;
    for report in reports {
        let Some(entry) = report
            .as_object()
            .and_then(|files| files.iter().find(|(name, _)| path_key(name) == key))
            .map(|(_, entry)| entry)
        else {
            continue;
        };
        executed.extend(numbers(&entry["executed"]));
        if executable.is_none() && entry["executable"].is_array() {
            executable = Some(numbers(&entry["executable"]));
        }
    }
    if executed.is_empty() {
        return None;
    }
    Some(
        target
            .changed
            .iter()
            .copied()
            .filter(|line| executable.as_ref().is_none_or(|lines| lines.contains(line)))
            .filter(|line| !executed.contains(line))
            .collect(),
    )
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Changed lines whose code V8 counted zero times in every process; `None` when no process
/// loaded the file. Offsets are UTF-16 code units of the source.
fn node_unexecuted(target: &Target, reports: &[serde_json::Value]) -> Option<Vec<usize>> {
    let url = path_key(&interpreter_path(&target.absolute));
    // UTF-16 offset of each line start, and the code (non-whitespace) offsets on each line.
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut offset = 0;
    for line in target.source.split('\n') {
        let mut code = Vec::new();
        for c in line.chars() {
            if !c.is_whitespace() {
                code.push(offset);
            }
            offset += c.len_utf16();
        }
        offset += 1;
        lines.push(code);
    }
    let mut ran: BTreeMap<usize, bool> = BTreeMap::new();
    let mut loaded = false;
    for report in reports {
        for script in report["result"].as_array().into_iter().flatten() {
            if script["url"]
                .as_str()
                .map(|url| path_key(&percent_decode(url)))
                != Some(url.clone())
            {
                continue;
            }
            loaded = true;
            let mut ranges: Vec<(usize, usize, u64)> = script["functions"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|function| function["ranges"].as_array().into_iter().flatten())
                .filter_map(|range| {
                    Some((
                        usize::try_from(range["startOffset"].as_u64()?).ok()?,
                        usize::try_from(range["endOffset"].as_u64()?).ok()?,
                        range["count"].as_u64()?,
                    ))
                })
                .collect();
            // Innermost range wins: paint outer ranges first.
            ranges.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
            let mut counts = vec![None; offset];
            for (start, end, count) in ranges {
                for slot in counts.iter_mut().take(end.min(offset)).skip(start) {
                    *slot = Some(count);
                }
            }
            for (index, code) in lines.iter().enumerate() {
                let line = index + 1;
                if !target.changed.contains(&line) || code.is_empty() {
                    continue;
                }
                let executed = code.iter().any(|at| {
                    counts
                        .get(*at)
                        .copied()
                        .flatten()
                        .is_none_or(|count| count > 0)
                });
                *ran.entry(line).or_default() |= executed;
            }
        }
    }
    loaded.then(|| {
        ran.into_iter()
            .filter(|(_, executed)| !executed)
            .map(|(line, _)| line)
            .collect()
    })
}

/// `[3, 4, 5, 9]` → `3-5, 9`.
fn ranges(lines: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = lines.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap_or(end);
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests;
