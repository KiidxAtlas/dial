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
