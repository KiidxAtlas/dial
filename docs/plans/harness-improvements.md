# Harness improvement checklist

## Scope and decision

This checklist turns the harness source review into actionable work without treating every
suggestion as an established defect. The first batch was offline only. The operator then
authorized up to 30 minutes of local measurements and explicitly approved loading the already
downloaded `qwen3.8-27b-splash` into the existing LM Studio service. No downloads or hosted
inference were authorized. See section F for the follow-up findings and measurement limits.

Statuses: **done** means implemented or an existing invariant was inspected and checked;
**deferred** is not completed work. Source observations are not performance measurements.
Existing unrelated working-tree changes must remain untouched. No default, dependency,
permission policy, provider contract, or persisted session format is changed by this batch.

## A. Definite code improvements

### A1. Recurrence evidence correctness — implemented

- [x] Require an observed, non-null tool result before counting a recurring call.
- [x] Exclude synthetic duplicate/refusal results that did not execute the tool.
- [x] End the recurrence window at a new non-transient user request.
- [x] Keep transient harness notices from ending the window.
- [x] Compare full result values, not truncated presentation text.
- [x] Count a repeated call at most once per assistant batch.
- [x] Preserve the six-occurrences/ten-round threshold and advisory-only behavior.
- [x] Add regressions for missing results, user boundaries, synthetic results, large shared
  output prefixes, and duplicates inside one batch.

Owner: `crates/dial-agent/src/agent/safety/guard.rs`, `RecurringCall::detect`.
Tests: `guard/repeated_tool_call_tests.rs`. The missing-result regression was observed failing
before the fix and passing afterward. This change does not redefine the older consecutive
streak detector or its synthetic-result suppression contract.

### A2. Avoid recurrence history rescans and output copies — implemented

- [x] Collect borrowed tool-result bodies in the same reverse walk as the bounded turn window.
- [x] Remove the per-call search through the entire message history from recurrence detection.
- [x] Keep result bodies out of serialized identity keys; compare borrowed JSON values exactly.
- [x] Keep argument canonicalization and deterministic candidate selection unchanged.

This removes visible redundant work. No latency percentage, allocation benchmark result, or
live token-saving claim is made. The detector still compares full result values; this is not
an assertion of constant-time equality or a cap on the size of an individual tool batch.

### A3. One recovery selector for parent and workers — implemented

- [x] Reuse the existing stream-guard selection owner rather than introduce another engine.
- [x] Let the parent supply its already-detected streak, avoiding a second streak scan.
- [x] Retain that streak in the prepared request for duplicate-call suppression.
- [x] Keep redirect, escalation, short-cycle and irregular-recurrence priority and wording.
- [x] Test parity across no recovery, threshold redirect, escalation, two/three-step cycles,
  and irregular recurrence.

Owners: `safety/stream_guards.rs` and `run/turn/step/prepare.rs`.
This removes the duplicated recovery-rendering block from request preparation. No generic
middleware framework or new public API is needed.

## B. Definite architecture and regression work

### B1. Keep request preparation improvements grounded in demonstrated duplication

- [x] Extract the duplicated recovery decision into its existing owner (A3).
- [x] Inspect remaining preparation before proposing additional phase objects.
- [x] Preserve context measurement, model promotion, compaction claim/start/refresh/defer,
  request budgeting, and returned duplicate-call state.
- [x] Reject a wholesale rewrite based only on the function's length.

Further phase extraction is **deferred**, not an implementation obligation established by
this review. It needs a concrete ownership or testability problem and behavior-preserving
acceptance cases; moving lines alone is not an efficiency improvement.

### B2. Preserve already-tested lifecycle invariants

- [x] Inspect the explicit transition reset table (`run/turn/transition.rs`).
- [x] Retain steering tests for preserving budgets/project facts while resetting instruction
  recovery (`transition/tests.rs`).
- [x] Retain tests for input preservation, cancellation, and steering before accepting an old
  answer (`run/steering/routing/tests.rs`).
- [x] Retain stable system/tool bytes on follow-up requests (`turn/lifecycle_tests.rs`).
- [x] Retain focused-check reuse and zero default self-review on settled/read-only work.
- [x] Retain auxiliary-usage attribution and continuation cost bounds
  (`turn/usage_join_tests.rs`).

The full dial-agent library test run covers these existing owners as well as A1–A3.
Do not add duplicate fixtures merely to increase test count. New real failures should be
minimized into cases in these owners, with effects/state asserted in addition to final text.

### B3. Preserve architecture and scope

- [x] Keep current crate layering and existing tool/agent owners.
- [x] Do not add an effect framework, a second scheduler, a new compactor, or a plugin pipeline.
- [x] Do not replace the transcript representation with a new serialization format.
- [x] Keep the earlier MCP malware-check work and other unrelated changes intact.
- [x] Update the agent-loop documentation and Unreleased notes for recurrence behavior.

## C. Verification record

Checks belong to the working tree, not an assumed release or installed binary.

- `cargo test -q -p dial-agent --lib agent::safety`: 152 tests passed before the additional
  selector-parity test; final coverage is the full library run below.
- The registered `recurrence-current-turn-evidence` reproduction failed before the fix and
  passed after it.
- `make test-bench-offline`: 240 tests passed; no model or Dial binary required by this target.
- `cargo test -q -p dial-agent --lib`: 2,584 passed, 6 ignored, 0 failed.
- `cargo test -q --test integration architecture`: 27 passed, 0 failed.
- `cargo clippy -q -p dial-agent --all-targets -- -D warnings`: passed.
- `cargo fmt --check` and `git diff --check`: passed.

A macOS test link emitted an `__eh_frame` compact-unwind size warning during a focused run;
it did not fail the tests. No warning was suppressed. Offline fixtures do not establish live
provider latency, model quality, or savings from the recurrence implementation.

## D. Measurement requirements before changing defaults

These are definite measurements to do before changing efficiency defaults, not features
that should be enabled without evidence. The bounded follow-up in section F does not replace
the broader matched comparisons below; unchecked items remain deferred, not completed.

### D1. Current baseline and advisory/tool-surface ablations

- [ ] Select an exact model/provider, inference budget, and stable source/config identity.
- [ ] Run at least three repetitions of the existing lifecycle and representative suites.
- [ ] Compare the shipped configuration, minimal assistance, and one changed lever at a time.
- [ ] Include advisory overhead, contract failures, premature questions, completion nudges,
  malformed calls, successful repairs, and independently verified outcomes.
- [ ] Keep failed/unavailable trials in denominators; do not score the agent's self-report.

Reuse `docs/benchmarks.md` assistance ablation and the existing reliability/tool-effectiveness
analyzers. Avoid another benchmark runner. Publish per-model results rather than one supposed
best tool surface for all models.

### D2. Context reduction, summaries, and cache economics

- [ ] Compare existing `enable-shake_before_summary` against the shipped method order.
- [ ] Sweep controlled context-window sizes and evaluate correct continuation, not just size.
- [ ] Measure retained goals, corrections/retractions, evidence pointers, and repeated work.
- [ ] Measure speculative summary hit/discard rates and foreground stalls before changing
  its existing admission policy.
- [ ] Compare cache traffic and billed warm-up/summary usage, not just uncached prompt tokens.

Existing background compaction already has claim/start/refresh/defer behavior in
`run/turn/step/prepare.rs`; paid cache warming and prefix stability already exist. Do not
implement either capability a second time.

### D3. Critical path and whole-task accounting

- [ ] Reconcile main-agent, workers, judgments, compaction, warming, repairs and verification
  into task-level totals using existing usage and diagnostic records.
- [ ] Separate approval waits, runnable waits, inference, tool execution and result settlement.
- [ ] Report missing observations as unknown; distinguish aggregate durations from wall time.
- [ ] Report verified success first, then median/tail latency and cost per attempted/successful task.
- [ ] Compare parallel scheduling, background verification and delegation only where traces
  show a real opportunity; count invalidated checks and worker coordination/merge costs.

Existing `tool_trace` excludes several auxiliary scopes (`docs/tool-effectiveness.md`);
this is a reporting boundary, not proof that their usage is unaccounted everywhere. The
turn usage ledger already joins auxiliary work and enforces its cost boundary.

### D4. Promotion/removal decision for experiments

- [ ] Attach hypothesis, baseline, success criterion and complexity/cost budget to each tested
  experiment in its result report.
- [ ] Review when the matched repeated batch is complete, not after an arbitrary successful case.
- [ ] Promote only with maintained correctness and measured benefit; retain an opt-in only
  with a stated use case; propose removal separately when evidence supports it.

No automatic default adoption or deletion of existing experimental behavior is authorized
by an inconclusive offline test result.

## E. Ideas not yet justified as definite implementation work

Ranked graph retrieval, broader parallelism, additional automatic delegation, speculative
summary policy changes, typed replacement of all JSON plumbing, and generalized effect
separation remain proposals. Their admission criterion is a measured bottleneck or a
specific correctness/testability defect. Existing owners must be extended, not duplicated.

## Review coverage map

The preceding source review's numbered suggestions map here so nothing silently disappears:

| Review items | Disposition |
| --- | --- |
| 1, 2: preparation and parent/worker recovery | A3, B1; focused extraction implemented, broad rewrite deferred |
| 3: advisory effectiveness | D1; live evidence deferred |
| 4: recurrence correctness/efficiency | A1–A2 implemented |
| 5: steering invariants | B2; existing coverage inspected and exercised |
| 6–8: context reduction, semantic compaction, speculation | D2; existing mechanisms, comparative evidence deferred |
| 9: cache stability | B2 existing regression; D2 economics deferred |
| 10: ranked repository retrieval | E; experiment, not established need |
| 11: per-model tool exposure | D1; existing ablation machinery |
| 12–15: critical path, parallelism, checks, delegation | D3; existing owners, measurement before expansion |
| 16–18: effects, typed internals, layering | A3/B1/B3; no wholesale framework migration |
| 19–20: repeated comparisons and joint quality/cost/latency | D1/D3; offline graders exercised, live trials deferred |
| 21: lifecycle replay | B2 plus the new failing-then-passing recurrence regression |
| 22: auxiliary accounting | B2 existing ledger checks; D3 cross-report reconciliation deferred |
| 23: experiment lifecycle | D4; explicit decision criteria recorded |

## F. Bounded local follow-up

### Defects fixed

- [x] Read lifecycle counters from the actual per-send RPC observation shape rather than
  expecting flat counters at the top of the retained efficiency envelope.
- [x] Sum counters across observed turns, exclude recovery snapshots, and leave missing,
  empty or overflowing observations unavailable. Preserve older flat-report parsing.
- [x] Accept informational `answered` outcomes without pretending they increment the
  runtime's work-completion counter. Keep completion-counter consistency and reject
  blocked, incomplete, failed and unknown outcomes.
- [x] Keep independent graders unchanged. An accepted RPC outcome is not correctness proof.
- [x] Add and run regressions: the answered-outcome test failed before the fix and passed
  afterward; counter tests exercise aggregation, missing evidence and overflow.

Owners: `crates/dial-app/src/benchmark.rs` and `src/cmd/benchmark/rpc.rs`.
Checks: 47 app benchmark tests and 34 CLI benchmark tests passed; scoped Clippy passed.

### Measurement provenance and failures

The service initially reported only an embedding model loaded. The approved chat model was
loaded at 32,768 context with a 1,800-second idle TTL. No other model/service was replaced.

The first launch stopped before inference because the checkout contains an ignored benchmark
environment with an external Python symlink. The source-copy containment guard was preserved.
Eight suite/fixture files were copied byte-for-byte into an isolated source tree, and hashes
were retained in `benchmarks/harness-review/source-hashes.json`.

**Invalid current-build measurements:** the first lifecycle report (18 trials), compaction
report (6 trials), and interrupted lifecycle rerun used the stale `target/debug/dial` binary.
Cargo was actually building into a session-specific `.tmp/cargo-target` directory. These
reports must not be used as current-build performance or fix verification. The lifecycle
failures exposed defects also confirmed in current source/tests; all six stale compaction
trials timed out at the 60-second case limit. That is not evidence favoring either method order.
The stale rerun was stopped after the executable mismatch was discovered.

The corrected executable was resolved with `cargo metadata`, checked using `--build-info`,
and hashed before the final current-build validation. Its provenance is retained in
`benchmarks/harness-review/validation-build.json`; original reports and partial journals were
not overwritten. The local source revision was `f3ca216889838ed5d7bcfa2ada9d634f4b39fa3d`
with uncommitted changes; the executable hash, not that revision alone, identifies the run.

### Decision

Fix the demonstrated measurement defects; **do not change assistance or compaction defaults**.
The bounded batch is not a valid repeated current-build A/B comparison. Broader compaction
window sweeps, advisory ablations, tail latency and delegation experiments remain deferred.
Benchmark records stay under ignored `benchmarks/harness-review/`, not in committed fixtures.

### Current-build validation result

The corrected executable passed **9/9 lifecycle trials** (three repetitions each):
`self-contained-answer`, `investigate-before-asking`, and `finish-the-fix`.
Two investigation trials returned `answered` and were accepted without work-completion
increments. All required lifecycle counters were observed rather than unavailable.

The existing reliability analyzer counted **6/6 independently outcome-checked trials**:
the arithmetic answer and implementation cases. It correctly excluded the investigation
case from correctness scoring because that fixture only checks behavior, not the semantic
quality of the explanation. Three repetitions do not establish broad model reliability.

Recorded total wall time was 222,929 ms, with 225,733 observed prompt tokens and 4,810 output
tokens. Cost is unknown (`null`), not zero. This is one current configuration, not an A/B
speedup claim. Reports: `current-validation.json` and `current-reliability.json` under
`benchmarks/harness-review/`. No current-build compaction comparison was completed within
this bounded batch; no compaction setting was promoted.

Final offline evaluator checks: `make test-bench-offline` passed 240 tests; formatting and
tracked diff checks passed. The macOS linker warning noted above remained unsuppressed.

## G. Completed bounded hosted follow-up

- [x] Run the fixed hosted matrix: 18 lifecycle, 18 representative and 12 compaction trials,
  with three repetitions per case/configuration on one pinned executable.
- [x] Retain failed trials, distinguish independently checked correctness from behavior-only
  checks, and include auxiliary request usage and failed work in accounting.
- [x] Publish [results, provenance and limitations](hosted-harness-study.md). Costs are unknown
  on the subscription route; the user explicitly approved the fixed remaining quota matrix
  rather than a billed API fallback. Initial authentication-only failures are retained separately.
- [x] Keep assistance and compaction defaults unchanged: results do not justify promotion.
- [x] Add exact `verify` recovery targets to retained-check completion refusals; prove the
  suggested call clears the failure only after the original check passes, with target-limit
  coverage. The regression failed before the fix and passed afterward; 263 verification tests,
  workspace lint, formatting and diff checks passed afterward.

This closes the bounded study, not every prospective experiment in section D. No release
preflight, broad advisory ablation, delegated-task comparison, cross-model replication or
post-fix live recovery comparison is claimed. The study binary deliberately predates the
recovery-guidance patch so the measurement matrix does not mix implementations.
