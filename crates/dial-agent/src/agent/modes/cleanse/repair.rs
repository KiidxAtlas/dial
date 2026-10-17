//! Repair loop: check, batch diagnostics by file weight, dispatch one worker
//! per batch, and recheck. Generic over the checker and the worker so tests can use fakes.

use std::collections::HashSet;

use anyhow::Result;

use super::{Diagnostic, FileGroup, Report, format_diagnostics, group_by_file, take_batch};

/// Runs the checker suite once.
pub trait Checker {
    fn check(&self, cancelled: &(dyn Fn() -> bool + Sync)) -> Report;
}

/// What a worker reports after its assignment ends.
pub enum Dispatch {
    Done,
    Cancelled,
}

/// One batch of diagnostics for one worker.
#[derive(Clone, Debug)]
pub struct Assignment {
    /// 1-based worker number across the whole run.
    pub index: usize,
    /// Project-relative files this worker owns.
    pub write_scope: Vec<String>,
    /// Peer ownership lines. Empty for sequential dispatch.
    pub peers: Vec<String>,
    /// Rendered diagnostics for this batch.
    pub diagnostics: String,
}

/// Executes one assignment to completion.
pub trait Worker {
    fn repair(&mut self, assignment: &Assignment) -> Result<Dispatch>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopStatus {
    /// No diagnostics remain.
    Clean,
    /// The last round introduced no new diagnostics, but some remain.
    Stalled,
    /// Interrupted between workers.
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct LoopOutcome {
    pub status: LoopStatus,
    /// Workers dispatched.
    pub workers: usize,
    /// Diagnostics still present after the last check.
    pub unresolved: Vec<Diagnostic>,
}

/// Runs the repair loop. `cancelled` is polled between workers.
pub fn run(
    checker: &impl Checker,
    worker: &mut impl Worker,
    max_agents: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<LoopOutcome> {
    let mut report = checker.check(cancelled);
    if cancelled() {
        return Ok(outcome(LoopStatus::Cancelled, 0, &report));
    }
    if report.diagnostics.is_empty() {
        return Ok(LoopOutcome {
            status: LoopStatus::Clean,
            workers: 0,
            unresolved: Vec::new(),
        });
    }

    let mut seen: HashSet<String> = report.diagnostics.iter().map(Diagnostic::key).collect();
    let mut workers = 0;
    loop {
        let total: u64 = report.diagnostics.iter().map(Diagnostic::weight).sum();
        let budget = total.div_ceil(max_agents.max(1) as u64).max(1);
        let mut groups = group_by_file(&report.diagnostics);
        while !groups.is_empty() {
            if cancelled() {
                return Ok(outcome(LoopStatus::Cancelled, workers, &report));
            }
            let batch = take_batch(&mut groups, budget);
            workers += 1;
            let assignment = assignment(workers, &batch);
            match worker.repair(&assignment)? {
                Dispatch::Done => {}
                Dispatch::Cancelled => {
                    return Ok(outcome(LoopStatus::Cancelled, workers, &report));
                }
            }
        }

        report = checker.check(cancelled);
        if cancelled() {
            return Ok(outcome(LoopStatus::Cancelled, workers, &report));
        }
        if report.diagnostics.is_empty() {
            return Ok(outcome(LoopStatus::Clean, workers, &report));
        }
        let fresh = report
            .diagnostics
            .iter()
            .any(|diagnostic| seen.insert(diagnostic.key()));
        if !fresh {
            return Ok(outcome(LoopStatus::Stalled, workers, &report));
        }
        if cancelled() {
            return Ok(outcome(LoopStatus::Cancelled, workers, &report));
        }
    }
}

fn outcome(status: LoopStatus, workers: usize, report: &Report) -> LoopOutcome {
    LoopOutcome {
        status,
        workers,
        unresolved: report.diagnostics.clone(),
    }
}

fn assignment(index: usize, batch: &[FileGroup]) -> Assignment {
    let diagnostics: Vec<Diagnostic> = batch
        .iter()
        .flat_map(|group| group.diagnostics.iter().cloned())
        .collect();
    let write_scope = batch
        .iter()
        .filter(|group| !group.file.is_empty())
        .map(|group| group.file.clone())
        .collect();
    Assignment {
        index,
        write_scope,
        peers: Vec::new(),
        diagnostics: format_diagnostics(&diagnostics),
    }
}
