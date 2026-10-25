//! Deterministic, model-free benchmark of Dial's edit grammars.
//!
//! A seeded generator derives mutation cases (rename, statement replace, insertion, block delete,
//! body change, edit next to duplicate lines, and edits after the file shifted since the read) from
//! a corpus of source files. Each case is expressed as a correct call in every grammar and applied
//! through the real tool entry points in a temporary workspace. A result is scored `exact` (the
//! file equals the intended result), `refused` (the tool errored and left the file untouched: a
//! safe failure) or `wrong` (any other file state: an unsafe failure). Payload size is the
//! serialized call arguments in bytes and in estimated tokens (4 bytes per token).
//!
//! Wrong results are the number to drive to zero. Refusals and payload size are what to compare
//! when tuning a grammar; combine with the opt-in `edit.blackbox` log for real-world failures.
mod calls;
mod cases;
#[cfg(test)]
mod tests;

pub use calls::{Call, Grammar, render};
pub use cases::{Case, Hunk, Kind, SourceFile, embedded_corpus, generate, load_corpus};

use crate::config::types::{Config, EditMode};
use crate::tools::integrations::ext::ToolContext;
use crate::tools::{Workspace, execute};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{collections::BTreeMap, fmt::Write as _, fs, path::PathBuf};

/// Longest tool message kept per non-exact result.
const MESSAGE_LIMIT: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The file equals the intended result.
    Exact,
    /// The tool refused and the file is unchanged.
    Refused,
    /// The file differs from the intended result, or the tool reported an error after changing it.
    Wrong,
    /// The grammar cannot express this case.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub case: String,
    pub scenario: String,
    pub grammar: Grammar,
    pub outcome: Outcome,
    pub call_bytes: usize,
    pub call_tokens: usize,
    /// First line of the tool message for refused and wrong results.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Tally {
    pub exact: usize,
    pub refused: usize,
    pub wrong: usize,
    pub unsupported: usize,
    pub call_bytes: usize,
    pub call_tokens: usize,
}

impl Tally {
    fn add(&mut self, result: &CaseResult) {
        match result.outcome {
            Outcome::Exact => self.exact += 1,
            Outcome::Refused => self.refused += 1,
            Outcome::Wrong => self.wrong += 1,
            Outcome::Unsupported => {
                self.unsupported += 1;
                return;
            }
        }
        self.call_bytes += result.call_bytes;
        self.call_tokens += result.call_tokens;
    }

    /// Cases that ran, excluding unsupported ones.
    pub fn attempted(&self) -> usize {
        self.exact + self.refused + self.wrong
    }

    /// Mean call size over the cases that ran, in bytes.
    pub fn mean_bytes(&self) -> usize {
        self.call_bytes.checked_div(self.attempted()).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub seeds: Vec<u64>,
    pub results: Vec<CaseResult>,
}

impl Report {
    pub fn by_grammar(&self) -> BTreeMap<Grammar, Tally> {
        let mut map: BTreeMap<Grammar, Tally> = BTreeMap::new();
        for result in &self.results {
            map.entry(result.grammar).or_default().add(result);
        }
        map
    }

    /// Per scenario and grammar tallies, scenarios sorted by name.
    pub fn by_scenario(&self) -> BTreeMap<(String, Grammar), Tally> {
        let mut map: BTreeMap<(String, Grammar), Tally> = BTreeMap::new();
        for result in &self.results {
            map.entry((result.scenario.clone(), result.grammar))
                .or_default()
                .add(result);
        }
        map
    }

    pub fn wrong(&self) -> impl Iterator<Item = &CaseResult> {
        self.results
            .iter()
            .filter(|result| result.outcome == Outcome::Wrong)
    }

    /// Plain-text tables: totals per grammar, then per scenario and grammar.
    pub fn render(&self) -> String {
        let mut rows: Vec<[String; 8]> = Vec::new();
        for (grammar, tally) in self.by_grammar() {
            rows.push(row(grammar.name(), &tally));
        }
        let mut out = format!("edit grammars, seeds {:?}\n", self.seeds);
        out.push_str(&table(
            [
                "grammar",
                "exact",
                "refused",
                "WRONG",
                "n/a",
                "bytes/call",
                "tokens/call",
                "ran",
            ],
            &rows,
        ));
        let mut rows = Vec::new();
        for ((scenario, grammar), tally) in self.by_scenario() {
            rows.push(row(&format!("{scenario} / {}", grammar.name()), &tally));
        }
        out.push('\n');
        out.push_str(&table(
            [
                "scenario / grammar",
                "exact",
                "refused",
                "WRONG",
                "n/a",
                "bytes/call",
                "tokens/call",
                "ran",
            ],
            &rows,
        ));
        for result in self.wrong() {
            let _ = writeln!(
                out,
                "\nWRONG {} {}: {}",
                result.grammar.name(),
                result.case,
                result.message.as_deref().unwrap_or("")
            );
        }
        out
    }
}

fn row(label: &str, tally: &Tally) -> [String; 8] {
    let attempted = tally.attempted();
    [
        label.to_owned(),
        tally.exact.to_string(),
        tally.refused.to_string(),
        tally.wrong.to_string(),
        tally.unsupported.to_string(),
        tally.mean_bytes().to_string(),
        tally
            .call_tokens
            .checked_div(attempted)
            .unwrap_or(0)
            .to_string(),
        attempted.to_string(),
    ]
}

fn table(header: [&str; 8], rows: &[[String; 8]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    for line in std::iter::once(header.map(str::to_owned)).chain(rows.iter().cloned()) {
        for (index, cell) in line.iter().enumerate() {
            if index == 0 {
                let _ = write!(out, "{cell:<width$}", width = widths[0]);
            } else {
                let _ = write!(out, "  {cell:>width$}", width = widths[index]);
            }
        }
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone)]
pub struct Options {
    pub seeds: Vec<u64>,
    pub grammars: Vec<Grammar>,
    /// `None` uses the committed corpus embedded in the binary.
    pub corpus: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            seeds: vec![0],
            grammars: Grammar::ALL.to_vec(),
            corpus: None,
        }
    }
}

/// Generate every case for the options and apply each in every requested grammar.
pub fn run(options: &Options) -> Result<Report> {
    let corpus = match &options.corpus {
        Some(dir) => load_corpus(dir)?,
        None => embedded_corpus(),
    };
    let mut report = Report {
        seeds: options.seeds.clone(),
        results: Vec::new(),
    };
    for &seed in &options.seeds {
        for file in &corpus {
            for case in generate(file, seed) {
                for &grammar in &options.grammars {
                    report.results.push(apply_case(&case, grammar)?);
                }
            }
        }
    }
    Ok(report)
}

/// Score a finished call: only the intended file is `Exact`, an error that left the file as it
/// was is a safe `Refused`, and every other file state is `Wrong`, whatever the tool reported.
pub fn classify(is_error: bool, after: &str, before: &str, expected: &str) -> Outcome {
    if !is_error && after == expected {
        Outcome::Exact
    } else if is_error && after == before {
        Outcome::Refused
    } else {
        Outcome::Wrong
    }
}

fn short(message: &str) -> String {
    let line = message.lines().next().unwrap_or("");
    line.chars().take(MESSAGE_LIMIT).collect()
}

/// Apply one case in one grammar in a fresh temporary workspace: the file is read, optionally
/// shifted on disk, edited through the real tool, and compared with the expected result.
pub fn apply_case(case: &Case, grammar: Grammar) -> Result<CaseResult> {
    let dir = tempfile::tempdir().context("creating benchmark workspace")?;
    let path = dir.path().join(&case.file);
    fs::write(&path, case.original())?;
    let workspace = Workspace::new(dir.path())?
        .with_edit_mode(grammar.edit_mode().unwrap_or(EditMode::Replace));
    let read = execute(
        &workspace,
        "read",
        &serde_json::json!({"path": case.file}),
        20,
    );
    if read.is_error {
        anyhow::bail!("benchmark read of {} failed: {}", case.file, read.text);
    }
    if case.shift > 0 {
        fs::write(&path, case.before())?;
    }
    let mut result = CaseResult {
        case: case.id.clone(),
        scenario: case.scenario(),
        grammar,
        outcome: Outcome::Unsupported,
        call_bytes: 0,
        call_tokens: 0,
        message: None,
    };
    let Some(call) = render(case, grammar, &read.text) else {
        return Ok(result);
    };
    result.call_bytes = call.bytes();
    result.call_tokens = call.bytes().div_ceil(4);
    let output = if call.tool == "ast_edit" {
        ast_edit(&workspace, &call)?
    } else {
        execute(&workspace, call.tool, &call.args, 20)
    };
    let after = fs::read_to_string(&path).unwrap_or_default();
    result.outcome = classify(output.is_error, &after, &case.before(), &case.expected());
    if result.outcome != Outcome::Exact {
        result.message = Some(short(&output.text));
    }
    Ok(result)
}

fn ast_edit(workspace: &Workspace, call: &Call) -> Result<crate::tools::ToolOutput> {
    let tool = super::ast::tools(&Config::default())
        .into_iter()
        .find(|tool| tool.name() == call.tool)
        .with_context(|| format!("tool {} is not registered", call.tool))?;
    let root = workspace.root().to_path_buf();
    let authorize = move |name: &str, paths: &[PathBuf], _unbounded: bool| {
        if name == "ast_edit" && paths.iter().all(|path| path.starts_with(&root)) {
            Ok(())
        } else {
            Err("benchmark authority covers only the benchmark workspace".to_owned())
        }
    };
    let config = Config::default();
    Ok(tool.call(
        &ToolContext {
            workspace,
            config: &config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &call.args,
    ))
}
