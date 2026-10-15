//! `/cleanse` agent side: checker discovery, repair dispatch, and verification.
//!
//! Every checker's output is normalized into [`Diagnostic`]s (`parsers`), balanced by file weight,
//! and repaired by `worker` subagents one batch at a time (`repair`). The suite is rerun once to verify.

mod checkers;
mod parsers;
mod prompts;
mod repair;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::agent::{Agent, Ui};

pub use checkers::Suite;
pub use repair::{LoopOutcome, LoopStatus};

/// Diagnostic severity. Repair weight: error 5, warning 3, info 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }

    fn weight(self) -> u64 {
        match self {
            Self::Error => 5,
            Self::Warning => 3,
            Self::Info => 1,
        }
    }
}

/// One normalized checker finding. `file` is project-relative with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub checker: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub end_line: Option<u64>,
    pub end_column: Option<u64>,
    pub code: Option<String>,
    pub severity: Severity,
    pub message: String,
    pub suggestion: Option<String>,
}

impl Diagnostic {
    /// Identity used for dedupe. The checker label is excluded.
    fn key(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}\0{}",
            self.file.as_deref().unwrap_or(""),
            opt(self.line),
            opt(self.column),
            self.code.as_deref().unwrap_or(""),
            self.message
        )
    }

    /// Repair weight: severity plus penalties for a missing line, code, and suggestion.
    fn weight(&self) -> u64 {
        let mut weight = self.severity.weight();
        if self.line.is_none() {
            weight += 2;
        }
        if self.code.is_none() {
            weight += 1;
        }
        if self.suggestion.is_none() {
            weight += 1;
        }
        weight
    }
}

fn opt(value: Option<u64>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}

/// A checker that could not run during discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub label: String,
    pub reason: String,
}

/// One executed checker. `diagnostics` include failure diagnostics when the checker failed.
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub diagnostics: Vec<Diagnostic>,
}

/// Result of running the whole suite once.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub diagnostics: Vec<Diagnostic>,
}

/// Diagnostics for one file (`""` = project-level), with their total repair weight.
#[derive(Clone, Debug)]
pub struct FileGroup {
    pub file: String,
    pub diagnostics: Vec<Diagnostic>,
    pub weight: u64,
}

/// Groups diagnostics by file, heaviest group first.
pub fn group_by_file(diagnostics: &[Diagnostic]) -> Vec<FileGroup> {
    let mut by_file: BTreeMap<String, Vec<Diagnostic>> = BTreeMap::new();
    for diagnostic in diagnostics {
        by_file
            .entry(diagnostic.file.clone().unwrap_or_default())
            .or_default()
            .push(diagnostic.clone());
    }
    let mut groups: Vec<FileGroup> = by_file
        .into_iter()
        .map(|(file, mut diagnostics)| {
            diagnostics.sort_by(compare_diagnostics);
            let weight: u64 = diagnostics.iter().map(Diagnostic::weight).sum();
            FileGroup {
                file,
                diagnostics,
                weight,
            }
        })
        .collect();
    groups.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.file.cmp(&b.file)));
    groups
}

fn compare_diagnostics(a: &Diagnostic, b: &Diagnostic) -> Ordering {
    a.line
        .unwrap_or(0)
        .cmp(&b.line.unwrap_or(0))
        .then_with(|| a.column.unwrap_or(0).cmp(&b.column.unwrap_or(0)))
        .then_with(|| a.checker.cmp(&b.checker))
        .then_with(|| a.message.cmp(&b.message))
}

/// Takes groups from the front until their weight reaches `budget`; always takes at least one.
pub fn take_batch(groups: &mut Vec<FileGroup>, budget: u64) -> Vec<FileGroup> {
    let mut batch = Vec::new();
    let mut weight = 0;
    while !groups.is_empty() && (batch.is_empty() || weight < budget) {
        let group = groups.remove(0);
        weight += group.weight;
        batch.push(group);
    }
    batch
}

/// Keeps the first diagnostic for each key.
fn dedupe_keep_first(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = HashSet::new();
    diagnostics
        .into_iter()
        .filter(|diagnostic| seen.insert(diagnostic.key()))
        .collect()
}

/// Project-wide dedupe, sorted by file, line, then message.
fn dedupe_project(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut unique = dedupe_keep_first(diagnostics);
    unique.sort_by(|a, b| {
        a.file
            .as_deref()
            .unwrap_or("")
            .cmp(b.file.as_deref().unwrap_or(""))
            .then_with(|| a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0)))
            .then_with(|| a.message.cmp(&b.message))
    });
    unique
}

/// Renders diagnostics the way repair prompts list them.
pub fn format_diagnostics(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| {
            let location = match (
                diagnostic.file.as_deref(),
                diagnostic.line,
                diagnostic.column,
            ) {
                (None, _, _) => "<project>".to_owned(),
                (Some(file), None, _) => file.to_owned(),
                (Some(file), Some(line), None) => format!("{file}:{line}"),
                (Some(file), Some(line), Some(column)) => format!("{file}:{line}:{column}"),
            };
            let code = diagnostic
                .code
                .as_deref()
                .map(|code| format!(" {code}"))
                .unwrap_or_default();
            let message: String = diagnostic.message.chars().take(4000).collect();
            let mut line = format!(
                "- [{}] {location} — {}{code}: {message}",
                diagnostic.severity.label(),
                diagnostic.checker
            );
            if let Some(suggestion) = &diagnostic.suggestion {
                line.push_str(&format!("\n  Suggested fix: {suggestion}"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Path components joined with `/`, without a root or `.` segments.
fn rel_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolves `.` and `..` lexically, without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl Agent<'_> {
    /// Static discovery of the project's checkers. No model calls.
    pub fn cleanse_discover(&self, include_tests: bool) -> Suite {
        checkers::discover(self.workspace().root(), include_tests)
    }

    /// Scout subagent: turns a free-form request into checker commands. `None` when interrupted.
    pub fn cleanse_request(
        &mut self,
        request: &str,
        ui: &mut dyn Ui,
        interrupt: &AtomicBool,
    ) -> Result<Option<Suite>> {
        let root = self.workspace().root().to_path_buf();
        let started = self.start_subagents(
            &json!({
                "agent": "scout",
                "name": "cleanse-discovery",
                "task": prompts::render_discovery(request),
                "solution_space": "discover checks for a given request: the scope is given, the checks are open",
                "output_schema": prompts::discovery_schema(),
            }),
            ui,
        )?;
        let id = started_id(&started)?;
        let Some(job) = self.await_subagent(&id, interrupt, ui)? else {
            return Ok(None);
        };
        let data = settled_data(&job, "checker discovery")?;
        let specs: Vec<checkers::CustomSpec> = serde_json::from_value(data["checkers"].clone())
            .map_err(|error| anyhow!("checker discovery returned invalid checkers: {error}"))?;
        Ok(Some(checkers::custom(&root, specs)))
    }

    /// Repairs the suite's diagnostics batch by batch, then verifies once.
    pub fn cleanse_repair(
        &mut self,
        suite: &Suite,
        max_agents: usize,
        interrupt: &Arc<AtomicBool>,
        ui: &mut dyn Ui,
    ) -> Result<LoopOutcome> {
        let mut worker = RepairWorker {
            agent: self,
            ui,
            interrupt,
            commands: suite.commands(),
        };
        repair::run(suite, &mut worker, max_agents, &|| {
            interrupt.load(AtomicOrdering::SeqCst)
        })
    }

    /// Waits for a subagent to settle. `None` when the interrupt flag fired (the subagent is cancelled).
    fn await_subagent(
        &mut self,
        id: &str,
        interrupt: &AtomicBool,
        ui: &mut dyn Ui,
    ) -> Result<Option<Value>> {
        loop {
            if interrupt.load(AtomicOrdering::SeqCst) {
                self.cancel_subagent(id)?;
                return Ok(None);
            }
            let status = self.subagent_status(Some(id), ui)?;
            let job = match status {
                Value::Array(jobs) => jobs
                    .into_iter()
                    .find(|job| job["id"].as_str() == Some(id))
                    .unwrap_or(Value::Null),
                other => other,
            };
            match job["status"].as_str() {
                Some("running") => thread::sleep(Duration::from_millis(100)),
                Some(_) => return Ok(Some(job)),
                None => bail!("subagent {id} reported no status"),
            }
        }
    }
}

/// Repair dispatch: one `worker` subagent per assignment, run to completion before the next.
struct RepairWorker<'a, 'g> {
    agent: &'a mut Agent<'g>,
    ui: &'a mut dyn Ui,
    interrupt: &'a Arc<AtomicBool>,
    commands: Vec<String>,
}

impl repair::Worker for RepairWorker<'_, '_> {
    fn repair(&mut self, assignment: &repair::Assignment) -> Result<repair::Dispatch> {
        let prompt = prompts::render_assignment(assignment, &self.commands);
        let started = self.agent.start_subagents(
            &json!({
                "agent": "worker",
                "name": format!("cleanse-repair-{}", assignment.index),
                "task": prompt,
                "solution_space": "one repair: the failing diagnostic is given, the fix is open",
            }),
            self.ui,
        )?;
        let id = started_id(&started)?;
        let Some(job) = self.agent.await_subagent(&id, self.interrupt, self.ui)? else {
            return Ok(repair::Dispatch::Cancelled);
        };
        match job["status"].as_str() {
            Some("done") => Ok(repair::Dispatch::Done),
            Some("interrupted") => Ok(repair::Dispatch::Cancelled),
            status => bail!(
                "repair worker {} {}: {}",
                assignment.index,
                status.unwrap_or("ended without a status"),
                job["result"].as_str().unwrap_or("no details")
            ),
        }
    }
}

/// Id of the first job in a `start_subagents` response.
fn started_id(started: &Value) -> Result<String> {
    let job = started
        .as_array()
        .and_then(|jobs| jobs.first())
        .unwrap_or(started);
    job["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("subagent did not start: {started}"))
}

/// Structured output of a finished subagent; errors when it did not finish cleanly.
fn settled_data(job: &Value, what: &str) -> Result<Value> {
    match job["status"].as_str() {
        Some("done") => job
            .get("data")
            .filter(|data| !data.is_null())
            .cloned()
            .ok_or_else(|| anyhow!("{what} finished without structured output")),
        status => bail!(
            "{what} {}: {}",
            status.unwrap_or("ended without a status"),
            job["result"].as_str().unwrap_or("no details")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(file: Option<&str>, line: Option<u64>, message: &str) -> Diagnostic {
        Diagnostic {
            checker: "tsc".into(),
            file: file.map(str::to_owned),
            line,
            column: None,
            end_line: None,
            end_column: None,
            code: Some("TS1".into()),
            severity: Severity::Error,
            message: message.into(),
            suggestion: Some("fix it".into()),
        }
    }

    #[test]
    fn groups_are_heaviest_first_and_batches_respect_budget() {
        let diagnostics = [
            diagnostic(Some("b.ts"), Some(1), "x"),
            diagnostic(Some("a.ts"), Some(1), "y"),
            diagnostic(Some("a.ts"), Some(2), "z"),
        ];
        let mut groups = group_by_file(&diagnostics);
        assert_eq!(groups[0].file, "a.ts");
        assert_eq!(groups[0].weight, 10);
        let batch = take_batch(&mut groups, 5);
        assert_eq!(batch.len(), 1);
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn dedupe_drops_repeats_regardless_of_checker() {
        let mut other = diagnostic(Some("a.ts"), Some(1), "x");
        other.checker = "other".into();
        let unique = dedupe_project(vec![diagnostic(Some("a.ts"), Some(1), "x"), other]);
        assert_eq!(unique.len(), 1);
    }

    #[test]
    fn formats_location_code_and_suggestion() {
        let text = format_diagnostics(&[diagnostic(Some("a.ts"), Some(3), "boom")]);
        assert_eq!(
            text,
            "- [error] a.ts:3 — tsc TS1: boom\n  Suggested fix: fix it"
        );
    }
}
