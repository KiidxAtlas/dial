//! Non-gating observations of runtime journaling, encoding and long-history workloads.
mod workloads;

use anyhow::{Result, ensure};
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Serialize)]
pub struct Baseline {
    pub schema_version: u32,
    pub samples: usize,
    pub raw_append_ns: u128,
    pub journaled_append_ns: u128,
    pub journal_overhead_ratio: f64,
    pub render_encode_p95_ns: u128,
    pub workloads: Vec<Workload>,
}

/// Warm-cache observations of one deterministic workload, not a model-quality score.
#[derive(Debug, Serialize)]
pub struct Workload {
    pub name: &'static str,
    pub samples: usize,
    pub fixture_items: usize,
    pub fixture_bytes: usize,
    pub median_ns: u128,
    pub p95_ns: u128,
    pub total_ns: u128,
}

pub fn measure(samples: usize) -> Result<Baseline> {
    ensure!(
        (100..=10_000).contains(&samples),
        "runtime baseline needs 100..10000 samples"
    );
    let chunk = "streamed evidence α with a `symbol`\n";
    let mut raw = String::new();
    let start = Instant::now();
    for _ in 0..samples {
        raw.push_str(std::hint::black_box(chunk));
    }
    let raw_append_ns = start.elapsed().as_nanos();
    ensure!(
        raw_append_ns > 0,
        "clock resolution cannot measure the baseline"
    );
    let scratch = tempfile::tempdir()?;
    let tree = crate::runtime::Tree::default();
    tree.journal(crate::runtime::stream_journal::Journal::open(
        &scratch.path().join("session.jsonl"),
    )?);
    let body = tree.start("assistant", "baseline", Some("fixture".into()));
    let start = Instant::now();
    for _ in 0..samples {
        body.append(std::hint::black_box(chunk));
    }
    let journaled_append_ns = start.elapsed().as_nanos();
    tree.check_persistence().map_err(anyhow::Error::msg)?;
    ensure!(
        body.snapshot().result.text == raw,
        "journal recorder changed streaming content"
    );
    let theme = crate::ui::theme::Theme::dark();
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        let rows = crate::ui::markdown::render(chunk, 100, &theme, false);
        std::hint::black_box(crate::ui::term::rows_to_ansi(&rows, 100));
        timings.push(start.elapsed().as_nanos());
    }
    timings.sort_unstable();
    Ok(Baseline {
        schema_version: 2,
        samples,
        raw_append_ns,
        journaled_append_ns,
        journal_overhead_ratio: journaled_append_ns as f64 / raw_append_ns as f64,
        render_encode_p95_ns: timings[(samples * 95).div_ceil(100).saturating_sub(1)],
        workloads: workloads::measure(samples)?,
    })
}

#[cfg(test)]
mod tests;
