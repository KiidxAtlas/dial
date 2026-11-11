# Hosted harness study: bounded results

## Decision

Keep assistance disabled and compaction defaults unchanged. The assistance results are mixed; the compaction slice preserves its goal under every tested policy but does not establish a preferred method. Implement only the narrow failed-check recovery guidance improvement described below. No architecture rewrite or additional delegation is justified by this study.

## Protocol and provenance

Completed the fixed 48-trial matrix on Claude Sonnet 4.6 through the existing Claude subscription route, with three repetitions per case/configuration. Assistance off/on was tested at 32,768 context on the lifecycle and representative suites. Compaction policy orders were compared at 16,384 and 32,768 context. Profiles, isolated fixture snapshot, hashes, raw reports and analysis scripts remain under ignored `benchmarks/hosted-harness-study/`.

Pinned executable SHA-256: `76918fe7e88c76e023c0ae281c648b6e673cdb720572cae617f451bc97af96c2`. Build revision: `f3ca21688983` (dirty, debug build). This executable predates the new failed-check recovery guidance; the study does **not** measure that patch's effect.

The initial 18 lifecycle attempts without shared authentication failed before any observed model activity and are retained in `lifecycle.json` as invalid access attempts, not scored model trials. The 48 authorized measured trials include every failure; no failed trial was replaced. Lifecycle and representative benchmark commands exited 1 because their correctness/regression floors were not met; the compaction command exited 0.

The original $30 target with observed-dollar stopping could not operate on subscription access: cost coverage is non-invoice and costs are null, not zero. After the first 18 measured trials the user explicitly authorized the remaining fixed 30 trials on the subscription route only, with no API-key billing fallback. That later authorization supersedes the original stop-on-unknown policy recorded in `build.json`. No invoice ceiling or dollar-spend measurement is claimed.

## Results

All-pass includes both independent outcome checks and behavioral requirements. Checked-pass excludes the lifecycle investigation task, which does not independently grade answer correctness. Time and tokens include failed trials.

| Suite | Configuration | All-pass | Checked-pass | Wall seconds | Prompt tokens | Output tokens |
|---|---|---:|---:|---:|---:|---:|
| lifecycle | assistance-off | 8/9 | 5/6 | 62.471 | 289,295 | 2,945 |
| lifecycle | assistance-on | 7/9 | 6/6 | 67.523 | 304,324 | 3,313 |
| representative | assistance-off | 6/9 | 6/9 | 669.824 | 2,302,632 | 43,024 |
| representative | assistance-on | 7/9 | 7/9 | 876.858 | 2,065,438 | 49,600 |
| compaction | 16384-shake-first | 3/3 | 3/3 | 34.393 | 137,853 | 1,375 |
| compaction | 16384-summary-first | 3/3 | 3/3 | 34.513 | 137,892 | 1,559 |
| compaction | 32768-shake-first | 3/3 | 3/3 | 27.971 | 137,244 | 1,289 |
| compaction | 32768-summary-first | 3/3 | 3/3 | 35.318 | 138,695 | 1,748 |

### Task-level consistency

Counts below retain all three repetitions, including agent errors. The existing `reliability.py` outputs retain pass@k, pass^k and per-task Wilson intervals; three repetitions leave wide uncertainty.

| Suite | Configuration | Task | Passed / trials | Median seconds |
|---|---|---|---:|---:|
| lifecycle | assistance-off | finish-the-fix | 2/3 | 10.638 |
| lifecycle | assistance-off | investigate-before-asking | 3/3 | 8.231 |
| lifecycle | assistance-off | self-contained-answer | 3/3 | 1.343 |
| lifecycle | assistance-on | finish-the-fix | 3/3 | 13.554 |
| lifecycle | assistance-on | investigate-before-asking | 1/3 | 6.832 |
| lifecycle | assistance-on | self-contained-answer | 3/3 | 1.237 |
| representative | assistance-off | corrected-retracted-compaction-journey | 1/3 | 85.949 |
| representative | assistance-off | rust-workspace-nondefault-member | 2/3 | 128.897 |
| representative | assistance-off | typescript-boundary-javascript-caller | 3/3 | 32.621 |
| representative | assistance-on | corrected-retracted-compaction-journey | 1/3 | 126.695 |
| representative | assistance-on | rust-workspace-nondefault-member | 3/3 | 83.008 |
| representative | assistance-on | typescript-boundary-javascript-caller | 3/3 | 35.126 |
| compaction | 16384-shake-first | short-history-compaction | 3/3 | 10.710 |
| compaction | 16384-summary-first | short-history-compaction | 3/3 | 11.984 |
| compaction | 32768-shake-first | short-history-compaction | 3/3 | 9.556 |
| compaction | 32768-summary-first | short-history-compaction | 3/3 | 11.255 |

### Whole-task accounting

Requests include main and auxiliary usage; compaction requests are reported separately by purpose below and must not be added to the total again. Runtime `workers` includes auxiliary compaction work, not evidence of a delegated coding-task experiment. Token totals are observed usage, not billed-token totals. Phase timings in `summary.json` are not summed into wall time; zeros for loading/prompt-processing do not prove those provider phases were free.

| Suite | Configuration | Requests | Compaction requests | Tool calls / errors | Compactions | Max trial seconds | Tokens per checked completion |
|---|---|---:|---:|---:|---:|---:|---:|
| lifecycle | assistance-off | 25 | 0 | 21 / 2 | 0 | 12.916 | 58,448.0 |
| lifecycle | assistance-on | 26 | 0 | 22 / 3 | 0 | 15.348 | 51,272.8 |
| representative | assistance-off | 162 | 10 | 211 / 21 | 4 | 129.195 | 390,942.7 |
| representative | assistance-on | 157 | 14 | 189 / 23 | 5 | 227.399 | 302,148.3 |
| compaction | 16384-shake-first | 17 | 6 | 5 / 0 | 3 | 14.020 | 46,409.3 |
| compaction | 16384-summary-first | 17 | 6 | 6 / 0 | 3 | 12.434 | 46,483.7 |
| compaction | 32768-shake-first | 17 | 6 | 5 / 0 | 3 | 10.064 | 46,177.7 |
| compaction | 32768-summary-first | 17 | 6 | 7 / 0 | 3 | 14.984 | 46,814.3 |

## Interpretation and failure review

- Assistance-on improved the small correctness subset but worsened the lifecycle all-pass count. On representative tasks it improved the pass count while increasing total elapsed time. Neither comparison supports a default change; this is a bundled assistance profile, not an ablation of individual mechanisms.
- Four representative multi-turn trials ended incomplete with a retained Python AST failure despite the agent's claim that syntax/behavior checks passed. One assistance-off Rust trial failed its independent check without an agent-error diagnostic. The retained reports establish these outcomes, not the full causal history of the AST failures. No claim that every failure is a tooling defect is justified.
- All compaction trials passed exact goal preservation and the required compaction count. These are short histories, not a long-context quality benchmark. Both policy orders retain `remote` first, and the retained reports do not identify the selected method per event. Observed timing differences must not be attributed to shake versus summarization itself. No method-order promotion.
- All recorded outcome checks were available in the measured matrix. Failures and incomplete stops remain in the denominators. Unknown invoice cost remains unknown.

## Narrow implementation follow-up

The existing completion gate correctly retained failed checks; it named them but did not supply an exact tool invocation. The refusal now includes JSON `verify` targets for the retained checks, escaped correctly and bounded by the tool's target limit. It directs repair before rerun and warns that additional failures may remain. The gate, approval rules, evidence matching and defaults are unchanged.

The regression first failed for missing recovery guidance, then passed by parsing the suggested JSON and executing it through the real verify handler. It proves that an unrelated passing check cannot clear the failure, while the named check passing after repair does. A second test covers target-limit truncation without discarding retained blockers. This is an offline usability fix, not proof of improved live-model recovery.

Checks after this patch: `cargo test -q -p dial-agent --lib agent::verify::` (263 passed), `cargo fmt --check`, `make lint`, and `git diff --check`. The full workspace build/tests passed earlier in the study, before this recovery patch. The macOS linker emitted its existing large `__eh_frame` warning; it was not suppressed. No release preflight or post-patch hosted comparison was run.

## Limits and disposition

The bounded study is complete. One model, three repetitions, small fixtures, sequential non-randomized configurations, shared provider cache/quota conditions and concurrent local test compilation limit causal timing claims. Maxima are descriptive, not stable tail-latency estimates. Long-context quality, individual advisory ablations, delegated coding tasks, release-slice coverage and cross-model replication remain unmeasured; they are not silently promoted into completed experiments. No further hosted trials are launched by the analysis scripts.

## Raw report hashes

- `lifecycle-shared-auth.json`: `dede4f91ee18e7079c0b7fa79f9b8d4592187b4fb74198561b403de0a3342434`
- `representative.json`: `e236579b2a74e47ebc8fc201fdb46383f5c297771e70bf034b89e01645c6f8f6`
- `compaction.json`: `73a2410a7ed7c45d3c4411c9006319c5d6f2f9f19e6aac6bbc07113948a11d36`
