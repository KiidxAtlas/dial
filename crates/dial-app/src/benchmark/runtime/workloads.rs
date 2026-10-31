//! Deterministic, warm-cache workload observations. Setup and outcome checks are not timed.
use super::Workload;
use crate::{
    agent::{compaction, workers::child_transcript},
    config::{
        compaction::{CompactionConfig, CompactionIdentity},
        paths::Layout,
    },
    llm::{Chat, wire},
    runtime::{Phase, Tree},
    storage::session::Session,
    tools::{Workspace, knowledge::codebase::Codebase},
    ui::subagent_model::{self, HubCache},
};
use anyhow::{Result, ensure};
use serde_json::json;
use std::{fs, hint::black_box, io::Write, sync::atomic::AtomicBool, time::Instant};

/// Keep CI observations bounded even when the streaming microbenchmark requests 10,000 samples.
pub(super) fn measure(samples: usize) -> Result<Vec<Workload>> {
    let samples = samples.min(32);
    let mut out = Vec::new();
    runtime_flush(samples, &mut out)?;
    request_accounting(samples, &mut out)?;
    let scratch = tempfile::tempdir()?;
    let project = scratch.path().join("project");
    fs::create_dir(&project)?;
    let layout = Layout::with_user_dir(&project, scratch.path().join("state"))?;
    worker_transcripts(samples, &layout, &mut out)?;
    freshness(samples, &layout, &mut out)?;
    Ok(out)
}

fn observation(
    name: &'static str,
    fixture_items: usize,
    fixture_bytes: usize,
    mut timings: Vec<u128>,
) -> Workload {
    let total_ns = timings.iter().sum();
    timings.sort_unstable();
    let percentile = |percent: usize| timings[(timings.len() * percent).div_ceil(100) - 1];
    Workload {
        name,
        samples: timings.len(),
        fixture_items,
        fixture_bytes,
        median_ns: percentile(50),
        p95_ns: percentile(95),
        total_ns,
    }
}

fn runtime_flush(samples: usize, out: &mut Vec<Workload>) -> Result<()> {
    for history in [0, 256, 2048] {
        // Memory-only session isolates runtime bookkeeping from disk latency. The existing
        // journaled-append observation separately measures real filesystem writes.
        let mut session = Session::ephemeral();
        let tree = session.runtime();
        for index in 0..history {
            tree.start("tool", &format!("settled {index}"), None)
                .settle("fixture".into(), None, Phase::Completed);
        }
        session.flush_runtime()?;
        let live = tree.start("tool", "changing", None);
        session.flush_runtime()?;
        let mut timings = Vec::with_capacity(samples);
        for step in 0..samples {
            live.progress("benchmark", step as u32);
            let started = Instant::now();
            session.flush_runtime()?;
            timings.push(started.elapsed().as_nanos());
            ensure!(tree.pending().is_empty(), "runtime flush left dirty state");
        }
        out.push(observation(
            "memory_session_flush_one_dirty",
            history + 1,
            0,
            timings,
        ));
    }
    Ok(())
}

fn request_accounting(samples: usize, out: &mut Vec<Workload>) -> Result<()> {
    let request = Chat {
        messages: (0..256)
            .map(|index| json!({"role":if index % 2 == 0 {"user"} else {"assistant"},"content":"stream α evidence ".repeat(240)}))
            .collect(),
        tools: (0..32)
            .map(|index| json!({"type":"function","function":{"name":format!("tool_{index}"),"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}))
            .collect(),
        temperature: 0.0,
        max_tokens: 1024,
        seed: None,
    };
    let policy = CompactionConfig::default();
    let identity = CompactionIdentity {
        provider: "benchmark",
        model: "fixture",
        agent: None,
    };
    let fixture_bytes =
        serde_json::to_vec(&request.messages)?.len() + serde_json::to_vec(&request.tools)?.len();
    let expected = compaction::request_tokens_with_policy(&request, &policy, identity);
    let mut separate = Vec::with_capacity(samples);
    let mut combined = Vec::with_capacity(samples);
    for round in 0..=samples {
        // Alternate order; both paths are warm-cache observations, not provider inference.
        let mut run_separate = || {
            let started = Instant::now();
            let tokens =
                compaction::request_tokens_with_policy(black_box(&request), &policy, identity);
            let bytes = request
                .messages
                .iter()
                .map(|message| wire::wire_parts(message).total())
                .sum::<usize>()
                + request.tools.iter().map(wire::json_bytes).sum::<usize>();
            black_box(bytes);
            let elapsed = started.elapsed().as_nanos();
            ensure!(tokens == expected, "separate request estimate changed");
            if round > 0 {
                separate.push(elapsed);
            }
            Ok::<_, anyhow::Error>(())
        };
        let mut run_combined = || {
            let started = Instant::now();
            let accounting =
                compaction::request_accounting_with_policy(black_box(&request), &policy, identity);
            black_box(&accounting);
            let elapsed = started.elapsed().as_nanos();
            ensure!(
                accounting.tokens == expected,
                "combined request estimate changed"
            );
            if round > 0 {
                combined.push(elapsed);
            }
            Ok::<_, anyhow::Error>(())
        };
        if round % 2 == 0 {
            run_separate()?;
            run_combined()?;
        } else {
            run_combined()?;
            run_separate()?;
        }
    }
    out.push(observation(
        "request_tokens_plus_separate_wire_parts",
        request.messages.len(),
        fixture_bytes,
        separate,
    ));
    out.push(observation(
        "request_combined_accounting",
        request.messages.len(),
        fixture_bytes,
        combined,
    ));
    Ok(())
}

fn worker_transcripts(samples: usize, layout: &Layout, out: &mut Vec<Workload>) -> Result<()> {
    let directory = child_transcript::transcript_dir(layout, "benchmark");
    fs::create_dir_all(&directory)?;
    let path = directory.join("worker.jsonl");
    let mut bytes = serde_json::to_vec(
        &json!({"type":"header","id":"worker","task":"fixture","started_ms":1}),
    )?;
    bytes.push(b'\n');
    for index in 0..2048 {
        serde_json::to_writer(
            &mut bytes,
            &json!({"type":"message","message":{"role":"assistant","content":format!("entry {index}: {}", "evidence α ".repeat(80))}}),
        )?;
        bytes.push(b'\n');
    }
    fs::write(&path, &bytes)?;
    let tree = Tree::default();
    let mut cache = HubCache::default();
    let expected = subagent_model::snapshot(layout, "benchmark", &tree, None);
    let warm = cache.snapshot(layout, "benchmark", &tree, None);
    ensure!(
        warm == expected,
        "cached worker projection differs from uncached projection"
    );
    let mut uncached = Vec::with_capacity(samples);
    let mut cached = Vec::with_capacity(samples);
    for round in 0..samples {
        let mut run_uncached = || {
            let started = Instant::now();
            let value = subagent_model::snapshot(layout, "benchmark", &tree, None);
            let elapsed = started.elapsed().as_nanos();
            ensure!(value == expected, "uncached worker projection changed");
            uncached.push(elapsed);
            Ok::<_, anyhow::Error>(())
        };
        let mut run_cached = || {
            let started = Instant::now();
            let value = cache.snapshot(layout, "benchmark", &tree, None);
            let elapsed = started.elapsed().as_nanos();
            ensure!(value == expected, "cached worker projection changed");
            cached.push(elapsed);
            Ok::<_, anyhow::Error>(())
        };
        if round % 2 == 0 {
            run_uncached()?;
            run_cached()?;
        } else {
            run_cached()?;
            run_uncached()?;
        }
    }
    out.push(observation(
        "worker_projection_uncached",
        2048,
        bytes.len(),
        uncached,
    ));
    out.push(observation(
        "worker_projection_cached",
        2048,
        bytes.len(),
        cached,
    ));
    let mut appended = Vec::with_capacity(samples);
    for index in 0..samples {
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(
            file,
            "{}",
            json!({"type":"message","message":{"role":"assistant","content":format!("appended {index}")}})
        )?;
        let started = Instant::now();
        let value = cache.snapshot(layout, "benchmark", &tree, None);
        appended.push(started.elapsed().as_nanos());
        ensure!(
            value
                .activity
                .iter()
                .any(|event| event.entry == Some(2048 + index)),
            "incremental projection lost absolute transcript entry"
        );
    }
    out.push(observation(
        "worker_projection_appended",
        2048,
        bytes.len(),
        appended,
    ));
    Ok(())
}

fn freshness(samples: usize, layout: &Layout, out: &mut Vec<Workload>) -> Result<()> {
    let source = layout.project_root.join("src");
    fs::create_dir(&source)?;
    let unrelated = format!("// {}\npub fn unrelated() {{}}\n", "fixture ".repeat(128));
    for index in 0..512 {
        fs::write(source.join(format!("module_{index:04}.rs")), &unrelated)?;
    }
    let target = "pub fn route_needle() {}\n";
    fs::write(source.join("needle_router.rs"), target)?;
    let workspace = Workspace::new(&layout.project_root)?;
    let cache = Codebase::open(layout)?;
    let refreshed = cache.refresh(&workspace, &AtomicBool::new(false))?;
    ensure!(
        !refreshed.partial,
        "freshness workload setup did not index complete fixture"
    );
    let expected = cache.task_context("needle_router route_needle", 8000)?;
    ensure!(
        expected.contains("route_needle"),
        "freshness workload omitted target source"
    );
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let context = cache.task_context("needle_router route_needle", 8000)?;
        timings.push(started.elapsed().as_nanos());
        ensure!(
            context == expected,
            "ambient freshness retrieval changed without an edit"
        );
    }
    fs::write(
        source.join("needle_router.rs"),
        "pub fn newer_needle() {}\n",
    )?;
    ensure!(
        !cache
            .task_context("needle_router route_needle", 8000)?
            .contains("route_needle"),
        "ambient freshness reused externally changed source"
    );
    out.push(observation(
        "ambient_context_targeted_source",
        513,
        unrelated.len() * 512 + target.len(),
        timings,
    ));
    Ok(())
}
