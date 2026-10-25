//! `dial edit-bench`: the deterministic, model-free edit-grammar benchmark.
//!
//! Generates seeded mutation cases from a source corpus, applies each in every edit grammar
//! through the real tool entry points in temporary workspaces, and prints exact / refused / wrong
//! counts and payload sizes. Exits non-zero when any grammar produced a wrong (unsafe) result.
//! The engine lives in `tools::edit::grammar_bench`; this module only parses flags and prints.

use anyhow::{Context, Result, bail, ensure};
use std::path::PathBuf;

use crate::tools::edit::grammar_bench::{Grammar, Options, Report, run as run_benchmark};

/// Upper bound on `--seeds`; each seed is a few dozen temporary-workspace edits per grammar.
const MAX_SEEDS: u64 = 10_000;

/// Arguments for `dial edit-bench [flags]`.
#[derive(Debug, Clone, clap::Args)]
pub struct EditBenchArgs {
    /// Number of seeds to run, counted from `--first-seed`.
    #[arg(long, default_value_t = 20, value_name = "N")]
    pub seeds: u64,
    /// First seed of the run.
    #[arg(long, default_value_t = 0, value_name = "SEED")]
    pub first_seed: u64,
    /// Grammar to run (repeatable): hashline, replace, apply_patch, patch, sloppy, ast_edit. Default: all.
    #[arg(long = "grammar", value_name = "NAME")]
    pub grammars: Vec<String>,
    /// Directory of corpus files (default: the committed examples/benchmarks/edit-grammar-corpus).
    #[arg(long, value_name = "DIR")]
    pub corpus: Option<PathBuf>,
    /// Print the report as JSON instead of tables.
    #[arg(long)]
    pub json: bool,
    /// Also write the JSON report to this file.
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
}

fn options(args: &EditBenchArgs) -> Result<Options> {
    ensure!(
        (1..=MAX_SEEDS).contains(&args.seeds),
        "--seeds must be between 1 and {MAX_SEEDS}"
    );
    let last = args
        .first_seed
        .checked_add(args.seeds)
        .context("--first-seed plus --seeds overflows")?;
    let mut grammars = Vec::new();
    for name in &args.grammars {
        let Some(grammar) = Grammar::parse(name) else {
            let known: Vec<_> = Grammar::ALL.iter().map(|grammar| grammar.name()).collect();
            bail!("unknown grammar {name:?}; choose from {}", known.join(", "));
        };
        if !grammars.contains(&grammar) {
            grammars.push(grammar);
        }
    }
    if grammars.is_empty() {
        grammars = Grammar::ALL.to_vec();
    }
    Ok(Options {
        seeds: (args.first_seed..last).collect(),
        grammars,
        corpus: args.corpus.clone(),
    })
}

/// Run the benchmark; the exit code is non-zero when any result was wrong.
pub fn run(args: &EditBenchArgs) -> Result<i32> {
    let report = run_benchmark(&options(args)?)?;
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = &args.report {
        std::fs::write(path, format!("{json}\n"))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if args.json {
        println!("{json}");
    } else {
        print!("{}", report.render());
    }
    Ok(exit_code(&report))
}

fn exit_code(report: &Report) -> i32 {
    i32::from(report.wrong().next().is_some())
}

#[cfg(test)]
mod tests;
