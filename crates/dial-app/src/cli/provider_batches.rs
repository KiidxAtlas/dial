//! Explicit batch workflow; all network and paid work requires a named command.
use crate::config::paths::Layout;
use crate::llm::batch::{Client, MAX_BYTES, Plan, Provider};
use crate::storage::{
    archive::Archive,
    batch_jobs::{Ledger, archive_summaries},
};
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand};
use std::{io::Read, path::PathBuf};

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}
#[derive(Debug, Subcommand)]
pub enum Action {
    /// Prepare a tool-free JSON plan locally. No network or payment.
    Prepare { file: PathBuf },
    /// Snapshot archive metadata, prompts and existing recaps for independent summaries (not full transcripts).
    ArchiveSummaries {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        #[arg(required = true)]
        sessions: Vec<String>,
    },
    /// Submit a prepared job. This explicitly authorizes paid third-party processing.
    Submit {
        id: String,
        #[arg(long, required = true)]
        yes_paid: bool,
    },
    /// Inspect the private local record, without network.
    Inspect { id: String },
    /// Fetch remote status using the current paid API credential.
    Status { id: String },
    /// Collect bounded results. Never applies generated text to memory or files.
    Results { id: String },
    /// Request remote cancellation (nonterminal; completed items may still be charged).
    Cancel { id: String },
}
pub fn run(layout: &Layout, args: &Args) -> Result<()> {
    let ledger = Ledger::open(&layout.user_dir)?;
    match &args.action {
        Action::Prepare { file } => {
            let mut bytes = Vec::new();
            std::fs::File::open(file)?
                .take((MAX_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_BYTES, "batch plan exceeds byte bound");
            let plan: Plan = serde_json::from_slice(&bytes).context("invalid batch plan")?;
            println!("{}", ledger.prepare(plan)?.id);
        }
        Action::ArchiveSummaries {
            provider,
            model,
            sessions,
        } => {
            let provider = match provider.as_str() {
                "openai" => Provider::Openai,
                "anthropic" => Provider::Anthropic,
                _ => anyhow::bail!("only native paid openai or anthropic batches are supported"),
            };
            let plan =
                archive_summaries(&Archive::new(&layout.user_dir), provider, model, sessions)?;
            println!("{}", ledger.prepare(plan)?.id);
        }
        Action::Inspect { id } => {
            println!("{}", serde_json::to_string_pretty(&ledger.load(id)?)?);
        }
        Action::Submit { id, yes_paid } => {
            ensure!(*yes_paid, "paid submission requires explicit consent");
            let job = ledger.load(id)?;
            let client = Client::resolve(job.plan.provider, &layout.user_dir)?;
            let job = ledger.submit(id, &client)?;
            println!("{}", serde_json::to_string_pretty(&job)?);
        }
        Action::Status { id } | Action::Results { id } | Action::Cancel { id } => {
            let job = ledger.load(id)?;
            let client = Client::resolve(job.plan.provider, &layout.user_dir)?;
            match &args.action {
                Action::Status { .. } => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &client.status(
                            job.remote_id
                                .as_deref()
                                .context("no remote receipt; inspect provider console")?
                        )?
                    )?
                ),
                Action::Results { .. } => println!(
                    "{}",
                    serde_json::to_string_pretty(&ledger.collect(id, &client)?.results)?
                ),
                Action::Cancel { .. } => println!(
                    "{}",
                    serde_json::to_string_pretty(&ledger.cancel(id, &client)?)?
                ),
                _ => unreachable!("matched only remote actions"),
            }
        }
    }
    Ok(())
}
