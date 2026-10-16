# Core harness review: simplify work, not guarantees

This review compares the current Dial implementation with primary descriptions and source code
from Codex, Pi, and Anthropic. It covers the model/tool loop, context preparation, scheduling,
recovery, verification, and their storage costs. It is not an exhaustive security audit of every
provider or tool, and it is not a real-model leaderboard.

## Executive assessment

Dial does not need another agent framework. Its strongest components already follow the common
successful design: a model/tool loop, an append-only execution history, stable prompt prefixes,
just-in-time retrieval, bounded recovery, and explicit boundaries between tools and orchestration.
The opportunity is to remove redundant work and inconsistent admission paths inside that design.

Optimize in this order:

1. Correct task completion and enforce the same permissions on every execution path.
2. Remove avoidable failed tool calls and the model round trips needed to recover from them.
3. Reuse immutable planning inputs within an invocation; remove unused database/materialization work.
4. Measure context and inference costs on representative tasks before changing model-facing defaults.
5. Extract code only when responsibility and tests move together, not to meet a line-count target.

“Fewer lines,” “fewer tokens,” and “faster” are different objectives. Removing a safety check may
improve a microbenchmark while producing worse software. A smaller prompt can also add discovery
round trips, hide useful evidence, or break a valuable cached prefix.

## How Dial currently works

### One user turn, several model requests

`src/agent/turn_loop.rs` owns user-turn admission and final settlement. It retains the original
step budget across execution, focused verification repairs, and stop-hook continuations. Candidate
completion is not accepted merely because the model stopped generating tools.

`src/agent/turn.rs` is the foreground adapter. At a request boundary it processes live steering and
worker results, prepares the current tool offer, projects history, applies recovery guidance,
estimates context pressure, performs necessary context maintenance, and dispatches inference.
It then routes tool calls through schema checks, hooks, authorization, and execution, records the
results, and returns to the same loop. Parent-specific session/UI/learning policy stays here.

`src/agent/child.rs` performs the corresponding role adaptation for workers. The shared
`src/agent/engine.rs` drives both adapters and owns uncertain-effect stopping, repetition recovery,
unchanged-read reuse, output-length recovery, and committed-content edit-loop detection.
`src/tools/execution.rs` owns native execution boundaries rather than a second agent loop.

### Durable history is not the same as a model request

`src/storage/session.rs` retains the execution history. `src/agent/projection.rs`, `evidence.rs`,
and `task_packet.rs` construct a bounded model-facing view without rewriting original evidence.
Artifacts retain full results when a projection substitutes a pointer. Errors, mutations, user
instructions, and historical tool contracts cannot be treated as disposable summaries.

`context_meter.rs` distinguishes serialized size estimates from reported usage. `compaction.rs`
and `compaction_runtime.rs` provide model-aware context planning and compaction. Provider cache
identity belongs to the actual outbound payload, not just a convenient Rust-side estimate.

### Verification is a separate settlement phase

`src/agent/verification.rs` selects focused checks and classifies observed executions. `checks.rs`
publishes their status and drives completion prerequisites. Only confirmed code failures should
request automatic repairs; missing infrastructure, denied execution, cancellation, and incomplete
capture are not passing tests or evidence that application code needs changing.

`repair_evidence.rs` records a narrower claim: the same executable objective failed, source content
changed, and the objective subsequently passed. Even that is advisory historical evidence, not proof
that the current user task is complete. Source freshness and dependency claims remain separate in
`src/storage/codebase/architecture.rs`.

## Comparison with other harnesses

| Concern | Primary-source comparison | Dial assessment and decision |
| --- | --- | --- |
| Loop structure | Codex describes repeated inference, tools, appended observations, then an assistant response. Pi exposes a small shared loop with preparation, execution, steering, and finish hooks. | Dial already has a shared engine and role adapters. Preserve the separation; do not replace it with another generic event framework. |
| Prompt caching | Codex emphasizes exact prefixes, stable tool ordering, and appending changes rather than rewriting earlier items. | Dial has stable prompt bands, projected evidence, and outbound fingerprinting. Prefer eliminating needless local work over rewriting historical messages every step. |
| Tool discovery | Anthropic recommends deferred tool discovery for large catalogs, but explicitly notes the extra search-step latency. | Dial already supports discovery and bounded automatic activation. Do not shrink every model's default offer without measuring tool-selection failures and extra requests. |
| Programmatic tools | Anthropic keeps intermediate data in a code runtime, returning only relevant results to the model. | Dial already has eval/kernel callbacks and batching. Reuse those authorization boundaries; do not introduce another orchestration language. |
| Parallel execution | Codex's inspected runtime uses parallel eligibility plus a shared read/exclusive-write admission lock and cancellable readiness. | Dial's scheduler must retain dynamic authorization, recovery restrictions, and effect handling on every admitted call. An optimization is invalid if it bypasses any of these. |
| Long context | Pi checks projected context at request boundaries, preserves complete tool groups, and carries a structured summary plus recent history. Anthropic stresses high-recall compaction and just-in-time retrieval. | Dial already implements these principles with additional model-aware and speculative policies. Keep recent evidence and authoritative user corrections; evaluate speculative compaction cost before changing defaults. |
| Completion | Codex's article describes a final assistant message as the normal termination signal. Pi supports explicit finish/continuation hooks. | Dial deliberately adds focused verification, blockers, and effect certainty. Preserve those stronger contracts; reduce duplicate preparation, not the acceptance requirements. |
| Subagents | Anthropic recommends isolated deep investigation with concise reports when parallel work pays for itself. | Dial's shared worker engine and bounded reports fit this model. Do not make delegation unconditional: shared edits, local model serialization, and coordination consume time and tokens. |

These are mechanism comparisons, not assertions that any harness is universally better. Anthropic's
published token/accuracy improvements apply to its experiments, not to Dial. Claude Code's complete
current internal loop was not inspected; claims about it are limited to Anthropic's published material.
The Pi and Codex source links below track moving branches, so recheck them before relying on exact
implementation details in a later review.

## Changes selected for this pass

### 1. Consistent admission instead of failed parallel work

A pre-tool hook can rewrite a call. The foreground parallel executor previously rejected rewritten
arguments even though the ordinary serial path supports them. That turns a valid operator policy
into a tool failure and can force another model request. When pre-tool hooks are present, calls
use the serial admission path; hook-free eligible reads retain concurrency. Post-tool presentation
hooks alone do not justify disabling safe parallel execution.

After a hook rewrite, schema validity and authorization must be checked against the rewritten
call. Validation before the hook cannot establish that the hook's new path or arguments are valid.
Likewise, a later member of a prefetched batch must satisfy recovery restrictions, not just the
first member. These changes reduce an avoidable failure mechanism while strengthening admission.

Expected benefit: fewer unexecuted hook-rewrite calls and less recovery churn. Actual end-to-end
latency and token savings depend on the configured hooks and workload; no percentage is assumed.

### 2. Invocation-local focused-check planning

The related-test planner previously recomputed JavaScript package/runner metadata for each changed
JavaScript/TypeScript file and rescanned a Go package directory for each changed Go file.

The planner now resolves JavaScript metadata lazily once per invocation and memoizes the presence of
Go tests by directory for that invocation. No-check results are reusable too. Selection order and
command deduplication stay intact. A later invocation reads fresh metadata, so changes to manifests,
lockfiles, runners, or test files are not hidden by a persistent cache.

For N changed JS/TS paths in one planning call, runner/package resolution changes from N evaluations
to at most one. For Go, directory scans scale with distinct touched directories rather than files.
These are local-work reductions, not reductions in the number of required verification executions.

### 3. One execution authority for repair evidence

The main verification verdict already used typed process outcomes, but executable repair capture
still took its exit status from rendered output. A displayed marker is not authoritative execution
state, and exit zero alone does not establish that a nonempty check actually ran.

Repair capture now combines the classified verdict with the typed process outcome. Only accepted
passes and confirmed nonzero code failures receive usable exit evidence. Incomplete, unavailable,
cancelled, timed-out, and otherwise unverified observations invalidate a matching repair baseline
instead of silently allowing an older failure to pair with a later pass. Diagnostics remain visible.

This is principally an effectiveness/correctness improvement: future advice must not be trained or
retrieved as an executable repair on the strength of a forged or incomplete observation.

### 4. Source-only behavioral provenance

Recording a behavioral check used a freshness snapshot that also loaded every indexed dependency
outline from SQLite. The record only uses source hashes, environment identity, and completeness.
The unused query and outline materialization now belong only to callers doing dependency analysis.

The source walk, content hashing, branch/environment identity, bounds, and partial-scan handling are
unchanged. A fresh snapshot is still made for each check; external writers are not assumed to update
Dial's mutation counters. This is deliberately not a cache of passing verification results.

## What not to change without stronger evidence

- **Do not skip final checks because a tool once printed “passed.”** Reuse requires an exact check,
  environment, and source identity, including uncheckpointed writes. Current counters alone are
  insufficient for a general pass cache.
- **Do not cache authorization or recovery eligibility for a whole turn.** Hooks, steering,
  source changes, and prior results can change the answer before the next call executes.
- **Do not disable all parallelism when any hook exists.** Hook event matters. This pass conservatively
  serializes pre-tool-hook batches; finer matcher-aware scheduling needs its own admission tests.
  Never force rewritten calls through a parallel path whose admission contract assumes old args.
- **Do not add model judges to deterministic boundaries.** Schema validation, process termination,
  unchanged source identity, and test execution do not need another inference request.
- **Do not shorten the system prompt or aggressively prune results by intuition.** Stable prefixes
  can be valuable even when long, and lost evidence can cost more than it saves.
- **Do not use faster microbenchmarks as evidence of better agent answers.** Request preparation,
  provider prefill/decode, tools, approvals, compaction, and verification have different costs.
- **Do not split the large parent/worker adapters merely to reduce file sizes.** Extract the next
  coherent stage only with its callers and invariant tests. Preserve explicit role differences.

## Measurement and remaining priorities

Use the existing [quality/efficiency evaluation workflow](benchmarks.md#measuring-efficiency):
compare the same model, fixture hashes, settings, and repeated trials, with correctness first.
Measure successful-task latency alongside total requests, tool errors, prompt/output/cache usage,
compaction requests, and environment-unavailable checks. Include failed trials in reliability results.
Do not compare only the fastest successful examples or count unavailable usage as zero.

For the local source-provenance path, an ignored microbenchmark compares the prior full snapshot
operation with the source-only operation on the same generated workspace. It alternates execution
order, checks equal source identities/completeness, and reports 20 samples per path without asserting
a machine-specific speed threshold:

```sh
cargo test --locked --lib benchmark_behavior_source_snapshot -- --ignored --nocapture
```

This isolates removed local work. It does not measure model quality, provider latency, or total turn
speed. Regression tests should also exercise pre-hook rewrites, rewritten-call refusal, prefetched
recovery restrictions, fresh planner metadata on later invocations, and unknown repair evidence.

Next priorities, contingent on measured traces rather than automatic adoption:

1. Break down `request_prep.elapsed_ms` only if it is material: history projection, contract replay,
   hooks, and synchronous maintenance are different costs. The existing value is wall time, not CPU.
2. Evaluate tool-offer size and `tool_preview_bytes` on small and large models separately. Track the
   extra retrieval/discovery requests and lost-evidence failures, not only initial prompt bytes.
3. Evaluate speculative compaction by useful claims versus discarded preparations and total auxiliary
   model cost. A lower visible pause is not automatically lower total latency or spend.
4. Extract a shared prepared-call admission owner only if further parent/worker divergence is found.
   Do not expand the shared engine into a universal controller for persistence, UI, and role policy.

## Executed results and release limits

The broad affected-library run passed 595 tests, with two ignored tests and one failing new
concurrency fixture. That fixture incorrectly used the hermetic runner, which intentionally hides
extension tools. It was moved to the existing interactive integration fixture, where it passed.
The temporary missing test import found during compilation was also corrected.

The subsequent agent/architecture integration run passed 198 tests and failed 27. All three new
integration regressions passed: `parent_schedules_nohook_reads_concurrently`,
`batched_read_hook_rewrites_are_validated_and_authorized_without_a_model_retry`, and
`read_batch_lookahead_does_not_execute_a_later_withheld_read`. The library run also passed the new
child-rewrite, metadata-work-count, repair-authority, and source-provenance regressions. All four
architecture boundary tests passed. Scoped formatting and diff whitespace checks passed.

The remaining integration failures include changed recovery-stop expectations, dynamic tool
exposure/prefix expectations, compaction thresholds, learning/replay, silent-stream recovery, and
worker concurrency. Concurrent edits to those policies were visible during this review; a
workspace-copy ownership move also temporarily prevented compilation and then settled without
being reverted. No clean before/after baseline isolates all 27 failures. They are unresolved
integration findings, not proven regressions from this patch and not assumed harmless stale tests.
In particular, the silent-stream late-tool assertion needs investigation before release. This
work does **not** establish a green full suite or release readiness.

The source-snapshot microbenchmark passed with 258 files and 20 alternating samples per path:

| Measurement | Full snapshot | Source-only snapshot |
| --- | ---: | ---: |
| Median elapsed, debug build | 16,061 microseconds | 15,804 microseconds |
| Dependency outline bytes materialized | 435,009 | 0 |

The 257-microsecond difference is small and is not treated as a statistically established speedup.
The deterministic gain is one fewer unused index query/materialization and 435,009 avoided outline
bytes in this fixture; fresh filesystem hashing still dominates. The planner's counting regressions
establish one JS metadata resolution for three JS paths and one Go scan per directory for four Go
paths. The hook-rewrite fixture completes within two provider requests rather than requiring a
rewrite-retry request. These measurements do not establish better live-model task success or total
turn latency. No paid/provider-backed A/B benchmark or hardware inference test was run.

## Primary sources read

- OpenAI, [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/),
  January 23, 2026: prompt assembly, inference/tool iteration, exact-prefix caching, context compaction.
- Codex, [`tools/parallel.rs`](https://github.com/openai/codex/blob/master/codex-rs/core/src/tools/parallel.rs):
  parallel eligibility, read/exclusive admission, cancellation, and dispatch timing.
- Pi, [`agent-loop.ts`](https://github.com/earendil-works/pi/blob/main/packages/agent/src/agent-loop.ts):
  shared loop, steering/follow-up ordering, request preparation, and tool outcome lifecycle.
- Pi, [Compaction reference](https://pi.dev/docs/latest/compaction): projected context, complete tool
  groups, iterative summaries, retained recent history, and overflow recovery ordering.
- Anthropic, [Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents),
  September 29, 2025: minimal high-signal context, just-in-time retrieval, compaction, and scoped subagents.
- Anthropic, [Introducing advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use),
  November 24, 2025: deferred discovery, programmatic tool calls, and their workload-dependent tradeoffs.

All six sources were fetched successfully for this review. No live-model comparison was run as part
of the source research; design claims and local regression results must not be presented as one.
