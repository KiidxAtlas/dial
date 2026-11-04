# Local benchmark improvement batch

These observations are from one macOS machine using the existing LM Studio server and
`qwen3.8-27b-splash`. They are not cross-model quality results or a general speed claim.
Raw run files below are generated artifacts under `target/`, not committed fixtures.
The checkout contained other changes; this is not a clean-release comparison.

## Typed judgment output budget

Two sequential production-shaped judgment requests used the old 64-token output cap: the
initial request and its invalid-reply retry. Both returned empty visible content with
`finish_reason=length`; all 64 completion tokens in each response were reasoning tokens.
The question asked how much reasoning a failing-parser-test repair required, with the exact
choices `off`, `low`, `medium`, and `high`.

Replaying the same initial request with only the output allowance changed to 512 yielded
`{"choice":"medium"}` and `finish_reason=stop`.

| Measurement | Before: initial + retry | After: initial |
| --- | ---: | ---: |
| Requests | 2 | 1 |
| Valid typed answers | 0 | 1 |
| Completion tokens | 128 | 322 |
| Reasoning tokens | 128 | 314 |
| Summed request time | 3.040 s | 5.791 s |

This supports a **reliability correction**, not lower latency or token use. The default
judgment allowance is now 512. Explicit exact-model profile output budgets, including the
provider-resolved zero sentinel, remain authoritative. Reasoning controls are unchanged.
Wire-level tests cover default, 64, 1024 and zero budgets and explicit high reasoning.

Raw requests/responses: `target/judgment-budget-before.json` and
`target/judgment-budget-after.json`. These are direct inference probes, not full agent trials.

## Live compaction journey

Command (three repetitions, local-only, 180-second case deadline):

```sh
target/debug/dial benchmark examples/benchmarks/compaction.toml \
  --endpoint http://127.0.0.1:1234/v1 \
  --profiles examples/benchmarks/local-measurement.toml \
  --report target/compaction-live-batch-after.json
```

| Trial | Outcome | Elapsed | Completed turns | Actual compactions | Judgment requests |
| --- | --- | ---: | ---: | ---: | ---: |
| 0 | RPC deadline exceeded | 180.005 s | 1 | 0 | 1 |
| 1 | Passed all three checks | 71.737 s | 2 | 1 | 2 |
| 2 | Passed all three checks | 73.728 s | 2 | 1 | 2 |

The result is **2/3**, not 100%; the command correctly exited nonzero because the configured
correctness floor is 1.0. The successful trials produced the exact goal file after a real
history rewrite. Their judgment usage was one request per user turn. This differs from the
previous efficiency fixture and does not establish a matched before/after quality gain.
A broader live quality baseline was stopped after an RPC timeout; it is not a completed score.

Machine runs now persist each checked result in a unique, synced progress journal beside
`--report`. The journal was observed retaining these live results before the final report
was emitted. Journals are not scored reports or resume files. RPC errors additionally name
the waiting method and request ID without logging request parameters. The live timings above
predate that final diagnostic-only change.

### Reliability follow-up

The historical report and adjacent progress journal were unavailable in this checkout during the
0.3.0 reliability pass. The waiting RPC operation cannot be attributed from the aggregate table;
the live timeout remains unresolved. The benchmark's 180-second case deadline covers startup
through shutdown, while production compaction maintenance can allow 300 seconds. That difference
does not establish a production compaction defect, and no timeout budget was increased.

A separate deterministic RPC defect was reproduced and fixed: an expired call could dispatch
request bytes or accept a queued event/reply. Deadline checks now precede dispatch and event
consumption and follow receipt. The focused RPC suite passed **15 tests**, including expired
buffered events/replies, zero-byte expired requests and parameter-redacted diagnostics.

The rebuilt local binary passed scripted recovery **16/16** cases (76 provider requests) and
compaction **4/4** cases (20 provider requests), using baseline/compact-tools configurations and
two repetitions. Reports are local artifacts at `.tmp/reliability-recovery-62e9120d.json` and
`.tmp/reliability-compaction-62e9120d.json`. These prove harness orchestration only, not resolution
of the live timeout, model quality, or an improved live success rate.

The subsequently authorized three-trial local-only run used the existing
`http://127.0.0.1:1234/v1` endpoint and `local-measurement.toml` without downloading a model.
It failed waiting for RPC **request 4 (`send`)**, the post-compaction final response, within
the unchanged 180-second whole-case deadline. This is not evidence that `compact` itself
timed out. The process then reported a missing temporary benchmark source; neither the
requested final report nor its kept progress journal remained available. Source-lifetime
inspection established no cleanup cause. This is an unsuccessful measurement attempt, not
a completed three-trial result or an improved live success rate. No further live retry or
timeout extension was performed. The focused binary-target RPC suite again passed **15 tests**.
Retained failure output: `artifact://73be35444598ec1711fa2f8dc20f4ee5b35c864175e3911ee6d69c5e1b03b3f9`.

## Workspace copying: candidate rejected

Reusable real-filesystem benchmark:

```sh
cargo run -p dial-core --release --example workspace_copy_bench -- 9
```

Nine measured rounds per backend and fixture, after warm-up, alternating backend order.
The complete output manifest is checked outside the timer. An allocation-avoidance candidate
was tested and then reverted; production workspace-copy behavior was left unchanged.

Median milliseconds for the clone backend:

| Fixture | Baseline | Candidate | Restored baseline |
| --- | ---: | ---: | ---: |
| 1,200 small files | 150.428 | 154.939 | 158.488 |
| Nested source files | 87.974 | 100.573 | 93.935 |
| Excluded dependencies | 11.756 | 13.172 | 13.569 |
| Internal symlinks | 57.609 | 66.733 | 64.727 |

The plain-copy backend also showed no reliable gain. Drift between original and restored
baselines precludes attributing small timing differences to the candidate. No optimization
was retained. New regressions cover file modes and write isolation, escaping/broken file
symlinks, and aliases to excluded directories for both copying implementations.

## Scripted integration and tests

Against the rebuilt binary:

- Recovery: **12/12** trials across baseline and compact-tools configurations, three repetitions;
  54 scripted-provider requests. Report: `target/recovery-batch-after.json`.
- Compaction: **6/6** trials across the same configurations, three repetitions;
  30 scripted-provider requests. Report: `target/compaction-batch-after.json`.
- Python benchmark tests: **52 passed**.
- App benchmark tests: **37 passed**, including success/failure observation, observer failure
  stopping the next case, and observing resumed cases without inference.
- CLI benchmark tests: **29 passed** before the diagnostic addition; the final RPC subset
  then passed **14 tests**, including the new timeout-context/redaction regression.
- Typed judgment tests: **25 passed**. Workspace-copy tests: **9 passed**.

Scripted provider runs prove harness orchestration, not model quality. No model speedup,
dollar saving, or cache-hit rate is inferred. The build emitted existing desktop dead-code
warnings in test configurations and a macOS linker unwind-table size warning; they were
not suppressed.
