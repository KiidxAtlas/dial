# Benchmarks and measurement

Benchmark suites, efficiency measurement, workflow regression checks, configuration comparisons and assistance ablation.

## Interactive comparisons: `/benchmark`

In a Dial session, run `/benchmark` (or `/benchmark open`) to open the **local browser workspace**.
Select installed agents and compatible task suites, enter the shared model ID, repetitions and
per-turn timeout, then confirm quota use and local command execution. Opening the workspace does
not run a model. This is a browser-based picker and live scoreboard launched from the terminal,
not a new terminal overlay. `/benchmark status` returns its URL; `/benchmark stop` requests
cancellation and shuts down that workspace (a workspace that does not answer within three seconds
is killed). Existing `/benchmark <suite.toml> [flags]` commands still use the native benchmark CLI.

The workspace requires **Python 3.11+ and a Dial checkout** containing `examples/benchmarks/`,
on macOS or Linux (trials use POSIX process groups and `/bin/sh` checks; Windows is unsupported).
From another project, set `DIAL_BENCHMARK_ROOT` to that checkout before starting Dial. It reuses
`cross_agent.py` for execution, scoring, usage parsing and balanced trial order. Agent definitions
come from `examples/benchmarks/cross-agent.toml`: **Dial, OMP, Claude Code and Hermes**.
Unavailable executables are disabled, not installed. Each CLI needs its own authentication;
Dial's subscription login does not authenticate OMP. The shared model must be the provider's
exact ID (for example, **`claude-sonnet-4-6`**, not `claude-sonnet-4.6`). The known rejected spelling
is refused before running any trials. Availability is not proof of authentication or a live
provider connection. Harbor and SWE-bench
retain their separate provisioned runners and **are not selectable in this workspace yet**.
Suites needing native observations/integrations or missing fixtures are disabled. Portable checks
are scored equally; excluded checks remain explicit in the final report. Suites with overlapping
case IDs must be run separately. The workspace limits selections to 1000 sequential trials.

Live results update after each graded trial, with the current agent/case visible while it runs.
The per-agent scoreboard and final image include total/median/p95 wall time, requests, tool calls,
tool errors, delegations, input, uncached input, cache-read, cache-write and output tokens.
The browser additionally shows all-attempt wall/tokens per success, usage medians, observed tool
names/models and Dial efficiency coverage; individual trial details expose checks, exit codes,
redacted error excerpts, private stderr-log paths and available efficiency counters.
Cache read/write are **included in input**, not additional prompt tokens. Each usage measurement
has its own known-trial coverage; totals cover only those known trials, not a zero-filled estimate
of the whole run. Failed attempts with reported usage remain included. Request counts are those
reported by each adapter, not an independent count of network retries. Claude Code requests count
unique streamed assistant message IDs. Hermes sidecars report main-session tokens/requests;
tool counts, tool errors and delegated usage are unknown without session evidence. Prices are not
invented and no cost estimate is derived from an unverified pricing table.

All failures count in all-attempt wall times; grading/diagnostic capture is outside that timer.
Explicit authentication, model-not-found and unsupported-CLI startup errors disable only the
affected agent's remaining trials, and only when the failing trial made no tool calls (an agent
that already ran tools got past setup, and its stderr can echo tool output). They are retained as
**skipped**, never task-check failures; other agents continue. Ordinary execution/check failures
do not disable later trials. Model IDs are not validated by name; the agent CLI's own rejection is
the evidence. If the runner itself crashes, its traceback is kept privately in the run's
`logs/runner-error.txt`. The page
distinguishes aggregate errors across all agents from each agent's own errors. Open a trial for
its reason; fix credentials in that agent separately before starting another comparison.
Runs with skips are incomplete; runs where every invocation fails are invalid, not a measured
zero score. Cancellation stops the active agent/check process group and retains completed evidence.

The CLIs execute with approval prompts bypassed. A disposable working directory is **not a
sandbox**: only select trusted agents/suites. OMP, Claude Code and Hermes retain installed state.
Claude Code maps thinking-off to `alwaysThinkingEnabled:false` with low effort; Hermes maps it
to `--reasoning none`. Other thinking levels map explicitly to each CLI's options. The runner
applies the existing turn timeout/grace to CLIs without a native wall-clock flag.

Each run creates a unique private `benchmarks/runs/<timestamp>-<id>/` directory containing the
selection, incremental `progress.json`, final `report.json`, private per-turn logs and automatically
generated **`comparison.svg`**. The workspace shows the image and offers SVG, JSON and **PNG**
downloads (PNG conversion runs in the browser). Partial/invalid images carry their actual status.
No generative image service is used. Raw logs and reports may contain private material; inspect
before sharing. Do not interpret offline fixture checks as live-model quality evidence.

After updating the checkout, finish/cancel any active run, then `/benchmark stop` and `/benchmark`
to restart the Python workspace with the new code. Merely refreshing an older running controller
does not update its Python implementation. Existing saved runs are not rewritten.

**Settings are separate.** Dial trials have a fresh `DIAL_HOME` and discard ambient `DIAL_*`
overrides before applying the explicit agent environment. Only the declared subscription-auth
home is shared; provider credential environment variables remain available for authentication.
Personal settings are never rewritten. This measures the selected executable's shipped defaults,
not a claim that those defaults are optimal. Other agents retain their declared installed state;
the page and image disclose that asymmetry. Keep model, thinking, suite and executable identities
matched when comparing results. Promote tuning to everyday settings only after a matched run
demonstrates the desired quality/cost/latency tradeoff.

Native RPC exposes the same launch owner with `{"method":"benchmark","params":{"action":"open"}}`
(`status` and `stop` are also accepted). The returned capability-scoped loopback URL serves the
selection page and a frontend-neutral API: `GET api/catalog`, `GET api/state`, `POST api/start`
(`agents`, `suites`, `model`, `thinking`, `repetitions`, `timeout`, `confirm:true`), and
`POST api/cancel`. POSTs require JSON and `X-Dial-Benchmark: 1`; cross-origin requests and
unexpected Hosts are refused. The URL is private. ACP does not expose this capability.

Offline contracts: `python3 -m unittest discover -s examples/benchmarks -p 'test_benchmark_dashboard.py'`.

The `examples/benchmarks/lifecycle-efficiency.toml` slice covers a self-contained answer, a read-only
workspace investigation and a completed calculator fix. Run it deliberately with a configured model:

```sh
dial benchmark examples/benchmarks/lifecycle-efficiency.toml --report benchmarks/lifecycle.json
```

Compare provider-reported input/cache tokens, requests and per-turn efficiency counters:
`questions_before_investigation`, `completion_nudges`, `incomplete_stops`, `full_builds` and
`delivered_answer_bytes`. Missing counters in older reports remain unknown rather than becoming
zero. A completion nudge or incomplete stop is a controller observation, not an English-promise
classifier or proof of task quality. Independent outcome checks remain necessary. Deterministic
`llm::fake` lifecycle tests cover both completion surfaces, premature questions, unchanged schema
and system bytes, settled check reuse and zero default self-review. They establish controller
behavior, not live-model improvement. No live baseline result is implied by the suite's presence.

For the source directory map, offline checks and fixture ownership, start with
[the benchmark tooling index](../examples/benchmarks/README.md). Runnable non-benchmark samples
are listed in [the examples index](../examples/README.md).

The scripted provider extracts acceptance goals with a line-by-line scan rather than a
backtracking wildcard, so repeated goal prefixes in supplied conversation text do not
stall the offline harness. `test_smoke.py` covers both goal preservation and this input.

Local benchmark workspaces and reports live in `benchmarks/`, outside Cargo's disposable
`target/`. See [the local workspace map](../benchmarks/README.md). Downloaded datasets,
environments, binaries and private traces stay ignored by Git and Docker; committed tooling
and fixture inputs remain in `examples/benchmarks/`. Historical records retain their original
paths and bytes: resolve a recorded `target/harbor-*` path under `benchmarks/harbor-*` instead.

**Invalid runs.** A benchmark run in which no case shows any model activity (for example, every case fails immediately with an agent error before a model request) is reported as an invalid run and fails the command, instead of being reported as a 0 score. The JSON report records the reason in `invalid_run`, and each case keeps its `evidence.agent_error_detail`; fix the setup problem before comparing results.

## Harbor pilot diagnostic records

The local Harbor comparison at `benchmarks/harbor-current/` attaches versioned `diagnostics` to
each trial through `examples/benchmarks/harbor_diagnostics.py`. This is offline postprocessing
of retained logs, not another model run, a changed score, or a replacement for the official verifier.
Historical, supplemental evidence and three-way comparison batches remain separate.

The records include:

- Source byte counts, SHA-256 identities and relative-file/line retrieval pointers.
- Deduplicated model requests, model/phase, numeric policy selection and usage fields, finish
  reasons and available runtime latency measurements. Cache traffic retains producer semantics;
  token totals are not inferred dollar costs.
- Tool call/result links, argument/result hashes and byte counts, repeat-argument counts,
  observed call-to-result intervals, execution/error status, process/capture metadata and
  changed-path counts. Arguments and output bodies are not copied into the diagnostic export.
  Dial's final execution ledger also supplies automatic focused-check completions that are not
  model-message journal entries. Matching call identities are linked, not counted twice; runtime
  checks are labelled separately. Ledger-only records keep unavailable bodies and call-to-result
  intervals explicit, even when their status and execution duration are known.
- Pattern observations for unchanged reads, withheld calls, permissions, missing artifacts,
  disabled PTYs, missing dependencies, timeouts and provider retries. These labels and exact
  argument repetitions are not proof of wasted work or causal attribution.
- Separate environment setup, agent setup, agent execution and verifier intervals, plus official
  CTRF per-test status/duration and available completion/reliability metadata. Agent-reported
  completion does not override a failing official test.
- Explicit missing/malformed/oversized-source, missing latency/result and aggregate-versus-trace
  reconciliation gaps. Incomplete tool/error coverage leaves the diagnostic error total unknown,
  not zero. This does not rewrite producer-reported aggregate counters.
- A batch overview with a task/agent outcome matrix, failed official cases, excluded/missing/
  duplicate/unexpected trials, and job-level completion/error/retry counters—even jobs without
  trial results remain visible. Each attempt stays distinct; an empty selection is not a measured zero.
- Recorded agent/model/provider identity, task-checksum compatibility, exit status, reported cost
  and configured resource/timeout overrides. Overrides are not measurements of effective resources;
  recorded agent identity is not proof of per-trial executable identity.
- Per-agent timing, token/cache traffic, provider backoff, inference/storage/projection counters,
  request phases and per-tool error/repetition summaries. Numeric summaries include samples,
  expected samples, completeness, observed sum, total, minimum, median, nearest-rank p95 and maximum.
  Incomplete totals stay null. Request latency coverage must reconcile within **each trial**:
  extra traces in one trial cannot cancel missing traces in another. Overlapping request/tool sums
  are not wall-clock controller overhead. Provider, Harbor and verifier retries remain separate.

For the existing pilot, regenerate a **separate** supplemental report without agent execution:

```sh
python3 benchmarks/harbor-current/report.py --batch evidence \
  --output benchmarks/harbor-current/report-evidence-diagnostics.json
```

Use the exact recorded batch prefix and matching manifest for a comparison, and a fresh output
name rather than overwriting historical reports. The local runner/report and retained jobs are
generated artifacts, not a portable committed Harbor installation. Postprocessing cannot recover
events absent from older binaries or journals. OMP request latency/tool timing is unknown when
its stream supplies no timestamps. Frozen live-batch binaries, adapters and inputs are unchanged.

The JSON `overview` is also renderable as a readable Markdown companion through the same owner.
For example, after generating a fresh `report-threeway-coverage-final.json` using `--batch threeway`
and `--manifest benchmarks/harbor-current/comparison-manifest.json`:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 - <<'PY'
import json, sys
from pathlib import Path
sys.path.insert(0, 'examples/benchmarks')
from harbor_diagnostics import render_overview
root = Path('benchmarks/harbor-current')
report = json.loads((root / 'report-threeway-coverage-final.json').read_text())
with (root / 'report-threeway-coverage-final.md').open('x') as output:
    output.write(render_overview(report['overview']))
PY
```

Expected task/agent pairs come from the selected comparison manifest: do not interpret a historical
single-agent evidence batch as a missing three-way comparison. Costs, effective CPU/memory usage,
per-trial executable hashes and missing wire payloads remain unknown when not recorded.

Raw logs can contain credentials and private model/tool content. Keep both logs and derived
reports private: metadata still contains model/tool/test labels, timing, relative paths and hashes;
this is not an anonymization or public-export guarantee. The parser rejects source symlinks that
resolve outside the trial, bounds source size/journal count, and restricts structured capture and
verifier-duration values rather than copying arbitrary nested values.

Offline regression checks (no models or services):

```sh
python3 -m unittest discover -s examples/benchmarks -p 'test_harbor_diagnostics.py'
python3 benchmarks/harbor-current/test_threeway.py
```

### Retained cancellation failure and isolated repair

The supplemental cancellation trial's generated `/app/run.py` cancelled children a second time
after `gather` had already propagated cancellation. When a semaphore waiter exited before running
children finished their awaited cleanup, the second cancellation interrupted that cleanup.
Replacing that gather/manual-cancel block with a `TaskGroup`, retaining the semaphore and worker
helper, repairs the exact generated deliverable. This is not a defect in Dial's own cancellation
implementation and does not prove future model-generated solutions will be correct.

The original journal-extracted source and corrected replay are retained separately under
`benchmarks/harbor-current/fixes/cancel-async-tasks/{original,corrected}/run.py`. The original source
hash is `220b01d72d2c215646bff3ff34e1cde639db152ea3cf4bed1950ae14517efa58`.
An offline replay executed all six unchanged official test bodies and the unchanged helper:
the original reproduced **5/6**, failing queued-task cancellation; the corrected source passed
**6/6**. The host was Python **3.14.8**, not the task's Python 3.13 container. The replay explicitly
maps only the `/app/run.py` existence lookup to the replay copy and aliases `python` to that host
interpreter. This is host regression evidence, not exact container parity or a new agent score.

```sh
# Expected nonzero: demonstrates the original failure.
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/harbor-current/fixes/cancel-async-tasks/replay.py original
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/harbor-current/fixes/cancel-async-tasks/replay.py corrected
```

The separate reconciled report is `benchmarks/harbor-current/report-evidence-reconciled.json`:
all **85 requests, 106 tool completions and 15 errors** match the retained aggregate counters.
The two formerly unexplained calls were automatic focused checks, not lost model-issued calls.
The historical supplemental result remains **2/3**; the repaired replay is not pooled into it.

## Four-way Harbor comparison

The local Harbor runner's `fourway` agent set runs the current Dial build against OMP 18.13.2,
Claude Code 2.1.311 and Hermes Agent 0.20.5. Every agent is a frozen, hashed artifact on native
ARM64 and uses Sonnet 4.6 for main and side models through the same short-lived Claude
subscription token. Claude Code is Anthropic's linux-arm64 release binary (checksum from its
release manifest); Hermes is a self-contained `/opt/hermes` bundle built by
`benchmarks/harbor-current/build_hermes_bundle.sh` from a clean commit and its `uv.lock`.
Hermes runs with only its wrapper directory first on `PATH`, so its private Python does not reach
task shells; its own `execute_code` tool still uses it, a harness feature like Dial's eval.
opencode is not included: its current release cannot use a Claude subscription sign-in.

Usage comes from `parse_claude_json` and `parse_hermes_usage` in
`examples/benchmarks/cross_agent.py`. Claude Code tokens are the terminal result's per-model totals;
Hermes tokens come from its usage report and tool calls from its session export. A missing report,
missing export or delegated subagent leaves usage incomplete, and Hermes tool-error counts are unknown.

```sh
python3 benchmarks/harbor-current/snapshot_run.py benchmarks/harbor-current/snapshots/fourway-v1 \
  --manifest benchmarks/harbor-current/fourway-v1-manifest.json --batch fourway-v1-r1
python3 benchmarks/harbor-current/multiway_sheet.py benchmarks/harbor-current/report-fourway-v1-r1.json \
  --out benchmarks/harbor-current/sheets/fourway-v1.html --png benchmarks/harbor-current/sheets/fourway-v1.png
```

These make paid model requests. `multiway_sheet.py` pools several complete rounds of the same frozen
comparison and refuses incomplete rounds or mismatched agents, tasks, model and artifact hashes.
Each round is one attempt per agent per task, so a few rounds remain a small sample, not a
reliability estimate. Token and request totals are shown only when every pooled trial recorded usage.

## Using an existing subscription sign-in

Add `--share-auth` to explicitly let benchmark inference use the original subscription authentication
store, for example:

```sh
dial benchmark examples/benchmarks/efficiency.toml --share-auth --report efficiency-report.json
```

Configuration, sessions, fixtures and caches still use disposable benchmark homes. ChatGPT/Claude
subscription credentials are never copied: provider requests use the original store and its existing
refresh locks. The shared directory remains protected from agent tools and sandboxed commands.
Without this flag, subscription sign-ins remain unavailable to benchmark children, even if an ambient
`DIAL_SUBSCRIPTION_AUTH_HOME` override exists. API-key credentials retain their existing behavior.
Sharing authentication permits real model requests and can consume subscription quota.

## Measuring efficiency

From this checkout run `dial benchmark examples/benchmarks/efficiency.toml --profiles machine.toml --report efficiency-report.json`
using the machine/profile configuration described below. The five disposable cases cover a simple
question, a one-file edit, a failing-test repair, a multi-file rename, and continued-session compaction.
Compare correctness first, then observed prompt/output/cache/reasoning usage, calls, errors and elapsed
time. The simple-question case independently checks that the trimmed final answer is exactly `4`
as well as tool avoidance. Reports retain only the answer hash, not response prose.
The continued-session case tests multi-turn compaction, not process restart/resume. No live-model savings
are claimed by these fixtures, and prompt cache hits need not be free under a subscription limit.

`examples/benchmarks/local-measurement.toml` is a three-repetition, local-only LM Studio
configuration with a 180-second per-case deadline. Set its model ID to an already loaded model;
it does not download or load weights. The example command in that file writes a raw JSON report.
Use the same configuration and fixture revision for before/after measurements.

Journey `compact_before = true` requests a zero-token verbatim tail and requires the RPC to
confirm an actual compaction. A no-op maintenance response or context reset fails the case;
successful RPC dispatch alone never counts as compaction. Final counters come from the runtime.
`examples/benchmarks/compaction.toml` specifically exercises compaction of a short conversation
below the normal keep-recent budget. Its independent exact-file check verifies the retained goal.
To exercise this orchestration offline (not measure model quality):

```sh
python3 examples/benchmarks/smoke.py --dial ./target/debug/dial --suite compaction --machine-repetitions 3
```

`useful_work` counts only passing cases with independent outcome checks. Its per-completion elapsed,
token and API-cost numerators include **all** attempted cases, including failures and metric-only
cases, so dropping failed work cannot improve efficiency. Tokens and cost remain unknown when any
case has incomplete usage or pricing; zero completions never yields a finite efficiency ratio.
Elapsed is summed agent-case time, not wall time including external acceptance checks.
`metrics.runtime_usage` preserves the runtime ledger's total, main/worker and purpose breakdowns,
including retries, discarded and unreported attempts and pricing coverage. These are orthogonal
views of the same usage, not quantities to add together. Discarded-attempt counts do not imply
known discarded-token or dollar costs. Optional `metrics.efficiency.turns` retains each observed
per-turn report with its send ordinal. A bounded error-recovery state read is separately labelled
`recovery_observation`, because that state may still describe the preceding turn. These reports
are not cumulative totals. The runtime records first tool-result/known workspace-change-result
latency and exact parent/worker inspection overlap, not semantic reuse or wasted work. A useful
action's latency, missing counters and unavailable reports remain unknown rather than inferred.

For evidence-led quality work, use `dial benchmark examples/benchmarks/quality.toml --report benchmarks/quality.json`.
Its billing fixtures cover locating production code among archival decoys,
quantity/empty-input boundaries, a rename through callers, environment-versus-code diagnosis,
and rejecting stale historical discount advice. Two journey cases continue one session across
follow-up turns: an iterative feature that must survive a forced compaction, and a corrected
requirement where the latest instruction overrides the first request. Acceptance commands are outside the copied
project, so changing visible tests cannot weaken them. No third-party Python packages are needed.
Run `python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'` to verify the
evaluation machinery and fixture acceptance contracts without making model calls.

For independently checked tool-selection/composition, boundary, context and diagnosis cases, see
[tool-effectiveness.md](tool-effectiveness.md). Its offline analyzer compares all-trial work with
matched-success work on the existing repeated-run contracts; it does not reward tool use for its own sake.

Compare repeated runs with the same fixture revision and model using
`python3 examples/benchmarks/compare.py before.json after.json` (CLI report envelopes are accepted).
Machine reports (`dial benchmark --profiles`) are also accepted. When a report contains multiple
configurations, select them explicitly with `--baseline-configuration NAME` and
`--candidate-configuration NAME`; the comparator never silently chooses the best configuration.
Single-configuration machine reports need no selector. Repetition-prefixed case IDs must match
between the selected reports; use `reliability.py` for repeated-trial consistency analysis.
Reports record case categories, outcome-check counts, failed/unavailable check indices, and a
hash of the task/verification contract without retaining prompt text. `file_equals` checks exact
content, including trailing whitespace. Metric-only and manual-review cases are not counted as
checked coding successes by the comparison tool; legacy reports lacking this metadata cannot
establish checked success. Missing or incomplete provider usage stays unknown, not zero cost.
The comparison's `correctness_nonregressing` is `null` when the baseline has no checked
successes, because there is no baseline success to test for regression. The comparator exits 0
for a nonregressing comparison, 1 for a regression, 3 for this inconclusive result, and 2 for
invalid input or arguments. An inconclusive result is not evidence of preserved correctness.
Comparisons validate recorded run contracts and reject different schema, suite or fixture-source
identities. Executable and configuration identities may differ intentionally. Missing legacy run
contracts are labelled `run_identity: legacy_unknown`, not a confirmed matched-source comparison.
Observed transport attempts count as activity even when the request fails before reporting tokens;
those failed outcomes remain in reliability analysis rather than being discarded as inert runs.
The benchmark isolates user tools, configured verification commands and escalation; test those
policies with focused harness regressions, not a purported live escalation score. Interrupted
session recovery and concurrent edits are covered separately by terminal/transaction regressions.
Provider runs may consume quota. Report infrastructure failures separately, and never infer
model quality, dollar cost, or a provider cache hit from a scripted run or a missing usage counter.

### Interrupted machine runs

Machine-configuration runs (including assistance ablations) with `--report` also create a unique,
owner-only `<report-name>.progress-*.jsonl` journal beside the final report. The path is printed
to stderr. Each checked trial is flushed and synced before the next case starts, including failed
and unavailable trials. Records identify the configuration and zero-based repetition; configuration
records retain the hashed run contract without credentials or source contents. Repeated runs do not
overwrite earlier journals. A final `finished` record means all expected trials were recorded, not
that their checks passed or that a scored final report was successfully written.

An interrupted journal is diagnostic evidence, **not** a scored report, a baseline, or a resume
checkpoint. Do not calculate a success rate from only its completed cases. A crash during a write
can leave an incomplete final line; preceding complete JSON lines remain readable. Existing
single-configuration `--checkpoint` / `--resume` behavior is unchanged.

### Offline long-history workloads

```sh
cargo run --locked --profile local --example runtime_baseline -- benchmarks/runtime-baseline.json
```

The existing runtime baseline now emits schema version 2. It retains the streaming-journal and
markdown/terminal-encoding observations and adds nine warm-cache workload records:

- A memory-only session flush with one changed runtime node and 0, 256 or 2,048 historical nodes.
- A roughly 1 MiB request with 256 messages and 32 tool definitions, comparing combined accounting
  with token estimation followed by a separate wire-size pass.
- Worker activity projection over 2,048 transcript messages: uncached, unchanged cached, and one
  appended message per refresh. Checks preserve the absolute transcript entry used by deep links.
- Ambient retrieval of one named source in a 513-file repository, followed by an external edit
  that must invalidate the saved outline.

Each record includes its fixture item/byte counts, sample count, median, p95 and summed nanoseconds.
Workload sampling is capped at 32; setup, warm-up and outcome checks are outside the timed sections.
All fixtures and private state are disposable. No model, credentials, download or service is needed.
The session-flush fixture isolates in-memory bookkeeping, not fsync latency. The separate request
passes are an explicit reference workload, not an old executable or a complete agent request.
Worker cache population is outside the cached timings; initial loading is represented by the
uncached workload. Filesystem observations are warm-cache, not cold-disk measurements.

CI retains these observations without host-dependent speed thresholds. Regression tests gate
content, freshness, revision acknowledgement and entry-index correctness instead. Compare repeated
runs built with the same profile on an otherwise idle machine; these microbenchmarks do not establish
model quality, provider latency, token savings or an end-to-end speedup. Continue to use the existing
independently checked quality suites and repeated-outcome comparator for model-facing changes.

### Offline workspace-copy measurements

```sh
cargo run -p dial-core --release --example workspace_copy_bench -- 9
```

This benchmarks the actual clone and plain-copy implementations on deterministic small-file,
nested-source, excluded-dependency and (on Unix) internal-symlink fixtures. It warms both backends,
alternates execution order and validates the complete copied file manifest outside the timer.
CSV output records repetitions, file/byte counts and minimum/median/maximum elapsed milliseconds.
Setup, output verification and temporary-directory cleanup are excluded. These are warm-cache
filesystem timings, not cold-disk throughput or evidence of agent/model quality. Use repeated
baseline/candidate/restored-baseline runs before attributing small changes to an optimization.

See [the local measurement batch](benchmark-measurements.md) for observed judgment-budget,
live-compaction and workspace-copy results, including failed trials and a rejected optimization.

### Repeated outcomes, not best-run scores

Use the existing machine-configuration runner with `[machine] repetitions = 3` (1–10)
and `minimum_correctness = 1.0` in the configuration file below, then analyze its actual report:

```sh
dial benchmark examples/benchmarks/quality.toml --profiles machine.toml --report quality-repeated.json
python3 examples/benchmarks/reliability.py quality-repeated.json --output quality-reliability.json
```

The offline analyzer requires matching task/source hashes, complete repetition sets, and
outcome-check evidence. It reports per-task success frequency and Wilson intervals, empirical
all-trials/any-trial task fractions, unavailable checks, agent errors, tool calls/errors, repair
attempts, and observed tokens. Unavailable trials stay in the denominator; unknown usage stays
null. Latency ratios use only tasks passing every trial in both configurations. Exit status 1
means an observed per-task correctness-frequency regression against the named baseline, not a
statistical significance claim. Existing summary files are never overwritten.

Lifecycle metric checks read each send's `efficiency.turns[].report.projection_bytes` counters
and sum them across the case. Recovery snapshots are not additional turns. A missing counter,
empty observation set, or overflow is unavailable, not zero; legacy flat counter reports remain
readable. An RPC `answered` outcome can proceed to independent grading without incrementing
`turns_completed`, which continues to count completed work rather than informational answers.
Interrupted, blocked, incomplete and failed outcomes are not accepted as successful turns.

Consistency is reported per checked task and as a checked-task mean, for every `k` from 1 to the
repetition count `n`, under `pass_at_k` and `pass_hat_k` (JSON objects keyed by `"1"`…`"n"`):

- `pass_at_k`: chance that at least one of `k` trials passes, using the unbiased estimator
  `1 - C(n-c, k) / C(n, k)` for `c` passing trials out of `n`.
- `pass_hat_k`: chance that all `k` trials pass (τ-bench `pass^k`,
  [arXiv 2406.12045](https://arxiv.org/abs/2406.12045)), using the estimator `C(c, k) / C(n, k)`.

Key `"1"` equals the mean success rate (pass@1). Key `n` equals the empirical any/all-trial
fractions. A falling `pass_hat_k` curve shows a configuration that sometimes succeeds but is not
dependable, which matters for local models even when pass@1 looks acceptable. τ-bench reports a
gpt-4o retail pass^8 below 25% despite a pass@1 above 50%. Unchecked (metric-only/manual-review)
tasks get `null` curves and are excluded from the means.

Both estimators assume trials are independent and identically distributed, and with 3–10
repetitions they are imprecise. They are not proof that a small sample generalizes. Keep
model/configuration and fixture versions fixed when comparing harness revisions, and inspect
traces and final patches for output quality that executable checks do not capture. The underlying
runner already isolates each task and repetition; this analyzer does not launch more model calls.

To exercise the real repeated-run/reporting path without credentials or a remote model:

```sh
python3 examples/benchmarks/smoke.py --dial ./target/debug/dial --suite recovery --machine-repetitions 2
python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'
```

The smoke uses a loopback scripted provider with baseline and compact-tool configurations.
It verifies orchestration and graders, **not real-model performance**.

### Release slice

`examples/benchmarks/release-slice.toml` fixes the suites a release candidate is measured on:
`quality`, `reliability`, `representative`, `local-tasks`, `recovery` and `tool-effectiveness`
(27 cases), each case run three times per configuration (81 case runs). It is a plan, not a run:

```sh
python3 examples/benchmarks/release_slice.py --profile local --label v0.3.4
python3 examples/benchmarks/release_slice.py --profile hosted --label v0.3.4
```

The script validates the manifest against the committed suites and profiles and prints one
`dial benchmark` and one `reliability.py` command per suite, writing under
`benchmarks/release-slice/<label>/`. It never starts Dial or contacts a model. The hosted profile
(`release-slice-hosted-profiles.toml`, `allow_remote = true`) spends provider quota when you run the
printed commands; the local profile needs the ID of an already loaded model.

The manifest refuses fewer than three repetitions, a suite whose case count differs from the
pinned one, and profiles whose repetitions or remote policy disagree with it. Changing the suite
list, a case count or the repetitions changes the denominator: bump `name` and do not compare
against reports recorded under the old name. Report `pass_hat_k` at `k = 3` (all three trials
pass) next to pass@1; a single-attempt result, such as the Harbor comparison sheets, is not a
reliability estimate and must not be worded as one. No release-slice run has been recorded yet.

### Core-reliability acceptance protocol

Use deterministic regressions first for approval escalation, automatic-check denial, workspace and
external Rust test selection, recall across compaction, journal damage diagnostics, evaluation
exclusions, and final-revision completion evidence. These establish contracts, not model quality.
The completion/efficiency events expose versioned `completion_evidence`; do not treat
`execution_outcome=completed` or `observed_checks_passed` as an independent correctness label.
Keep baseline, stale and unavailable check evidence distinct from current passing observations.

For provider-backed comparisons, fix the model/configuration and fixture revision; use at least
three repetitions through the runner above, keep every unavailable/failed trial, and compare
independently checked task success before latency or observed cost per successful task. Include
workspace Rust and JS/Go projects and long journeys in project-specific suites, not only the small
Python quality fixture. Run paired harness configurations with one feature changed at a time
(memory, coaching/escalation, delegation, or adaptation). Inspect patches and user corrections as
well as executable acceptance checks. Compare latency/cost only with complete usage and equivalent
checked success populations; report missing usage as unknown. Existing assistance ablation support
is a starting point, not evidence that every feature improves results. No live-model improvement is
established by the scripted regressions or this integration alone.

### Representative multi-language and long-journey suite

`examples/benchmarks/representative.toml` adds three independently executable contracts:

| case | independent acceptance |
| --- | --- |
| Rust non-default workspace member | An external temporary Cargo test crate depends on `pricing` and `checkout`; checks quantity/zero/negative boundaries and the public caller. The default member's unrelated baseline failure and workspace defaults must remain unchanged. |
| TypeScript API / JavaScript caller | Native Node TypeScript stripping exercises the renamed export, null/empty/exact/over-page boundaries and the JS consumer. No npm install or transpiler. |
| Corrected/retracted journey | Seven sequential requests, three forced compactions; verifies the corrected 25-row default, rename, null boundary, removal of a retracted artifact and preservation of unrelated billing code. |

Prerequisites are an installed Cargo/Rust toolchain, Python 3.11+ and Node with native TypeScript
stripping (22.18+ or newer). The offline tests explicitly skip an unavailable language runtime rather
than install it. Run `python3 -m unittest discover -s examples/benchmarks -p test_representative.py -v`.
The tests establish that originals fail, reference repairs pass, mutable visible Rust tests cannot
weaken the external grader, stale corrected values/retracted artifacts fail, and the unrelated Rust
baseline still fails after an accepted repair. They do not establish live-model journey performance.

### Explicit policy experiments

Machine configurations support `[configurations.policy]`:

```toml
[configurations.policy]
strict_requirements = true
review_enabled = false
review_min_changed_files = 4
review_sensitive_paths = ["src/security", "Cargo.toml"]
review_max_rounds = 1
```

Unset fields preserve the operator value; explicit fields affect only the isolated child. These map
to `verification.strict_requirements` and `verification.review.{enabled,min_changed_files,sensitive_paths,max_rounds}`.
Bounds are 1–1000 files, 1–3 rounds and at most 32 workspace-relative file/directory prefixes (not
globs, each at most 256 bytes). Unknown keys and invalid bounds are rejected before inference.
Reports retain requested overrides and effective policy, `policy_contract_version = 1`, and
`policy_sha256`; the full effective configuration and integration mode also enter the run contract.
The existing ablation matrix now pins the shipped policy baseline and adds `enable-strict_requirements`
and `enable-independent_review`, each changing one lever. Review enablement alone does not guarantee
the trigger threshold is reached or that an independent reviewer is available. Keep the model, task
suite and all other settings fixed; analyze per-task/category checked success using the existing
reliability/tool-effectiveness analyzers. No optimizer, automatic default adoption, tool-count objective
or live improvement claim is introduced.

### Opt-in integration evaluations

Core runs remain offline and remove ambient MCP servers, user tools, custom agents and role fallbacks.
Builtin task delegation was already available in core; clearing custom presets never disabled it.
The integration lane is an explicit experiment, not a claim that core forbids builtin delegation.
Use a **machine** configuration (`--profiles`, not the legacy named-profile format), with one of:

```toml
[configurations.integration]
mode = "browser" # or "delegation", "mcp", "managed_shell"; default is "core"
# mcp_servers = ["fixture"] # required only for mcp mode: exact configured server names
```

Use `integration-browser.toml`, `integration-delegation.toml` or `integration-mcp.toml` with its matching
mode; `integration-profiles.toml` is a template. This can consume provider quota and **is not run by
offline tests**. Remote inference still separately requires `machine.allow_remote = true`.

- **Browser:** real headless native browser plus background server support; no attached browser,
  relay, cmux or stored session profile. Requires an already cached pinned JS runtime, installed Node
  and external Chromium. The isolated config sets `browser.allow_install = false`; neither dependency
  provisioning nor Chromium download is allowed. Cache-only preflight inspects files, not connections.
- **MCP:** retains only the exact named operator servers at both isolation points. Missing/disabled
  servers, missing executables/bearer environment and known fetch launchers are unavailable before
  inference. The example expects a real `fixture` service offering `multiply(a,b)`; no mock is supplied.
  A configured endpoint is not proof of connection: actual handshake/tool failures remain measured failures.
- **Delegation:** real builtin task agents use the selected model, with custom presets/agents and role
  routing removed. Work-packet contracts are enabled for the fixture; normal child/tool safety applies.
- **Managed shell:** retains only native `bg` support in addition to the core tool surface, for
  real PTY and stdin-pipe recovery. It does not enable network access, browsers, MCP servers or
  custom tools. `shell.no_pty` and ordinary tool/sandbox restrictions still apply.

Missing static prerequisites yield `AgentUnavailable`, all check indices unavailable, no model request,
and an invalid run rather than a meaningful zero score. A mismatched/core mode cannot silently run an
integration case. No-profile/legacy-profile integration invocations fail setup before inference.
Browser/MCP lanes explicitly allow tool network access; **acceptance commands stay network-disabled**.
Reports expose mode/names, not MCP endpoint/header/env/argument contents.

`tool_invoked` assertions establish admitted integration dispatch only, not successful connection or
effects. They are not counted as independent outcome checks, and absent/incomplete traces stay
unavailable. Each integration fixture also has executable/file outcome acceptance. Offline contract
tests (`test_integration_fixtures.py`) validate those graders with reference repairs but deliberately do
not claim to exercise services, UI interaction, real workers or provider behavior.

## Repository benchmark suites

For repeatable repository checks, create a TOML suite and run `dial bench suite.toml --report result.json`.
Each case is copied to a fresh temporary workspace and must declare deterministic checks; for example:

```toml
version = 1
name = "parser regression"

[[cases]]
id = "parses-emoji-path"
prompt = "Fix the parser so a path containing emoji is preserved."
checks = [
  { kind = "file_contains", path = "src/parser.rs", needle = "emoji" },
  { kind = "command", command = "cargo test --offline --lib parser", expect_exit = 0 },
]
```

`dial bench suite.toml --baseline old.json --report current.json` reports pass-rate, token, latency and
tool-call regressions and exits nonzero on regression. Command checks run in the disposable copy with
network disabled; state, `.dial`, build caches and common credential files are excluded from the copy.
The report omits prompts and model output. The Rust `BenchmarkSuite` API accepts custom agent and
verifier callbacks for normalized reports from other harnesses.

## Edit-grammar benchmark (offline, no model)

`dial edit-bench` measures whether each edit grammar applies a correct edit correctly, without any
model call. A seeded generator derives mutation cases from the committed corpus in
`examples/benchmarks/edit-grammar-corpus/` (Rust, TypeScript and Python files): rename an identifier,
replace a statement, insert after an anchor, delete a block, change a function body, edit one of several
duplicate lines, and `stale_read` (the same kinds after one to three lines were inserted above the edit
since the file was read). Every case is rendered as the correct call in each grammar (`hashline`,
`replace`, `apply_patch`, `patch`, `sloppy`, and `ast_edit` for whole-word renames, which is `n/a`
for other kinds) and applied through the real `read`/`edit`/`ast_edit` entry points in a temporary workspace.

```sh
dial edit-bench                                   # 20 seeds, all grammars, tables
dial edit-bench --seeds 200 --report edit-bench.json
dial edit-bench --grammar hashline --grammar patch --first-seed 100 --json
dial edit-bench --corpus path/to/your/files       # your own corpus (regular files, <= 256 KiB each)
```

Reading the result: each case is `exact` (the file equals the intended result), `refused` (the tool errored
and the file is unchanged: a safe failure) or `WRONG` (any other file state, including success reported
without the change: an unsafe failure). `n/a` means the grammar cannot express the case. `bytes/call` and
`tokens/call` are the serialized call arguments and that size divided by four, rounded up, averaged over
cases that ran. The command exits 1 when any result is `WRONG`; refusals never fail it. Expect hashline to
stay exact after the file shifted (it re-anchors by unchanged lines) while context-matching grammars
refuse those `stale_read` cases, trading a retry for safety.

A fast seed set (`cargo test --locked -p dial-tools --lib grammar_bench`) asserts zero wrong results for every
grammar; the 200-seed sweep is `#[ignore]`d (`cargo test --locked -p dial-tools --lib grammar_bench -- --ignored`).
To tune a grammar, treat any `WRONG` as a bug, compare `refused` and `tokens/call` between grammar
changes with the same seeds and corpus, and use the opt-in `[edit.blackbox]` log (see
[tools.md](tools.md#editing-grammars-and-safety)) to find the real-world edits that fail; add their shape
to the corpus or generator so the offline run reproduces them. The cases are synthetic, so the figures
describe grammar mechanics rather than model skill.

## Suite features and scripted smoke checks

Benchmark suites now support seeded `context_messages`, sequential `turns` with `compact_before`,
categories and bounded metric assertions. `dial bench suite.toml --profiles profiles.toml` selects
configured model-profile names and validated ablations (`no_integrations`, `replace_edit`, `hashline_edit`).
The first supplied profile is the baseline; comparisons rank verified success before latency.
Run `python3 examples/benchmarks/smoke.py --dial ./target/debug/dial` for deterministic capability
regressions, or add `--profiles examples/benchmarks/profiles.toml`. Scripted-provider results are harness
regression evidence, not claims about real-model quality.

## Workflow regression checks

After building the binary, exercise deterministic request recovery and evidence reuse with:

```sh
python3 examples/benchmarks/smoke.py --dial target/debug/dial --suite recovery --report /tmp/dial-recovery.json
python3 examples/benchmarks/interactive_smoke.py --dial target/debug/dial --captures /tmp/dial-pty-captures/main
python3 examples/benchmarks/interactive_surfaces.py --dial target/debug/dial --captures /tmp/dial-pty-captures/surfaces
python3 examples/benchmarks/session_contention_smoke.py --dial target/debug/dial --captures /tmp/dial-pty-captures/contention
python3 examples/benchmarks/side_lifecycle_smoke.py --dial target/debug/dial --captures /tmp/dial-pty-captures/side-lifecycle
```

The terminal probes require Python 3.11+, Git and tmux and create disposable Git roots, private
configuration, isolated tmux servers and loopback providers. They exercise delayed-discovery favorite
switching/cancellation, picker resizing, wrapped input cancellation, full blockers, queue management,
refused model restart and live help during a gated SSE reply. They also cover worker restoration,
focused draft/model controls, pool history reuse, side requests and crash checkpoints, independent
forks under writer contention, native tree previews, declarative panel outcomes, quiet-stream reveal
and incomplete write cancellation. Streaming markers must appear exactly once without regenerating
the request; conversation navigation must not silently restore workspace files.
Captures are real terminal text, **not pixel screenshots**. Scripted provider runs verify harness
behavior, **not model quality or a measured model-speed improvement**. Run
`examples/benchmarks/recovery.toml` through the benchmark CLI with a selected real model to obtain
model-specific measurements; compare verified task success alongside tool/error counts.

The recovery suite also includes `terminal-launch-recovery` and `stdin-launch-recovery`.
Both first exercise a default closed-input launch, then require the unchanged `launch_probe.py`
to receive input through a managed job. The terminal case needs a real PTY; the stdin case must
complete without one. Checks verify the receipt, the original probe hash and actual `bash`/`bg`
tool invocations. `smoke.py --suite recovery` supplies scripted decisions; selecting a real local
model through the benchmark CLI measures that model's decisions instead. Keep failed trials and
do not interpret a single candidate run as a before/after quality improvement.
The two launch cases require a machine configuration with
`[configurations.integration] mode = "managed_shell"`; default core isolation intentionally
excludes `bg`. `examples/benchmarks/launch-recovery-profiles.toml` is a local-only template;
replace its model placeholder with an already loaded model and retain its explicit provider
flag when selecting an endpoint. The recovery smoke driver opts into this lane explicitly.
The lane changes only the benchmark's isolated tool surface, not the normal agent's permissions.

Benchmark reports include the compiled executable identity (`execution_build`); legacy reports
without it remain readable. Compare direct reports from the same task suite with:

```sh
python3 examples/benchmarks/compare.py before.json after.json --output comparison.json
python3 -m unittest discover -s examples/benchmarks -p test_compare.py
```

The comparator reports correctness regressions before speed, compares latency only for matched
successful cases, and shows tokens/tool calls/errors. Empty checks and cases requiring manual
answer review are not counted as checked successes. Use identical fixture versions, seeds and
declared verifiers for real-model comparisons; metric-only success is not model-quality proof.
No live-model speed improvement is implied by scripted or report-analysis tests.

## Machine-configuration comparisons

Use the existing benchmark CLI to compare explicit configurations on identical checked tasks:

Machine throughput counts only passing cases with outcome checks and no manual-review category.
The `minimum_correctness` floor uses those outcome-checkable cases as its denominator; metric-only,
invocation-only, manually reviewed and legacy cases without evidence cannot establish verified
success. A configuration with no outcome-checkable cases cannot be recommended. Baseline regression
checks still apply, including to the other recorded cases.

```sh
dial benchmark suite.toml --profiles machine.toml --report machine-report.json
```

Example `machine.toml`:

```toml
[machine]
repetitions = 2
case_timeout_secs = 0
minimum_correctness = 1.0
allow_remote = false

[[configurations]]
name = "q4-8k"
provider = "mlx"
model = "/absolute/path/to/mlx-model"
context_window = 8192
quantization_bits = 4

[[configurations]]
name = "q4-16k-adapter"
provider = "mlx"
model = "/absolute/path/to/mlx-model"
context_window = 16384
quantization_bits = 4
adapter = "/absolute/path/to/adapter"
```

The first configuration is the baseline. `quantization_bits` validates existing MLX model metadata;
it does not quantize weights. Adapter metadata must identify the selected base model. Endpoint
variants use explicit provider/model IDs; non-loopback providers require `machine.allow_remote=true`.
Each case has fresh state against one source snapshot, with automatic improvement, ambient secondary
model roles, search and escalation disabled. An explicit independent-review experiment pins the coach
to the measured model and endpoint (including after child isolation); remote review requires the same
`machine.allow_remote` opt-in. Reports include machine identity, settings, suite hash, checked
correctness, wall time including failures/checks, observed tokens and repair rounds, and correctly
completed cases per second. Unavailable measurements stay null. Recommendations must meet the
correctness floor and per-case regression limits; reports never apply settings, activate adapters or
start training. Scripted-provider smoke results verify this plumbing, not real-model quality.

Each `[[configurations]]` entry may override assistance features for that run only. Unset keys keep the
user's config value; set keys are written into the child's isolated config and recorded under `settings.assistance`.

```toml
[[configurations]]
name = "q4-16k-no-grammar"
provider = "mlx"
model = "/absolute/path/to/mlx-model"
context_window = 16384

[configurations.assistance]
tool_grammar = false
initial_tool_limit = 0
```

Supported assistance keys: `enabled`, `compact_tools`, `initial_tool_limit` (0 = unlimited, or 1..=64), `tool_grammar`,
`repair_hints`, `syntax_guard`, `selective_verification`, `edit_mode`, `constrained_repair`, `lean_local` (`true`,
`false` or `"auto"`, matching the user config), `local_profile` (maps to `assistance.local.enabled`), and
`reliability_gate` (maps to `reliability.enabled`). Unknown keys are rejected.

Context-economy keys go under `[configurations.efficiency]` and are applied to the same isolated config: `tool_preview_bytes`
(0 or 1024..=16384), `history_token_cap` (0 or 8192..=2000000), `compaction_enabled` and `keep_recent_tokens`. Operator
config files are never modified. Each configuration records the resolved values under `settings.efficiency`, and
`settings.assistance.lean_local_within_auto_window` shows whether `lean_local = "auto"` is within its window limit.

## Assistance ablation

`dial benchmark --ablate-assistance [suite.toml] [-m model] [--report out.json] [--allow-remote]` runs an
automatic matrix on one model. `raw` turns assistance off. `dial` pins the assistance switches and
listed context-economy levers to `Config::default()` rather than your config file. Other settings remain
shared across the matrix and are subject to benchmark isolation. Each variant changes exactly one lever
from `dial`:

- `ablate-compact_tools` disables the stable tool core; `enable-repair_hints` enables repair hints;
- `ablate-initial_tool_limit`, `ablate-tool_grammar`, `ablate-edit_mode`, `ablate-reliability_gate`,
  `ablate-lean_local` and `ablate-local_profile` turn off or change features that ship on.
  `ablate-local_profile` sets `assistance.local.enabled = false`; see the local profile in `agent-loop.md`.
- `enable-strict_requirements` and `enable-independent_review` turn on the shipped-default-off
  outcome/review policies; their baseline settings and effective values are recorded as above.
- `ablate-structured_tools` pins `harness.tools` to the bash-only roster (`bash`, `todo`, `verify`, `finish`),
  so the shell does the reading, searching and editing. The `dial` row restores the default roster. Published
  harness ablations (Fan et al., arXiv 2609.20804) found bash-capable models matched or beat the structured
  tools at lower cost while weaker models fell sharply, so read this row per model.
- `enable-shake_before_summary` moves `shake` ahead of `soft` in `compaction.method_order`, so recoverable
  tool evidence is elided before a summary request is spent. It changes behavior only when a window is
  tight enough to trigger compaction; pair it with the context-window sweep below.

A context-window sweep uses the existing machine profiles: one `[[configurations]]` entry per window with
the same model, plus the lever under test in `[configurations.efficiency]`:

```toml
[[configurations]]
name = "32k-summary-first"
provider = "openai"
model = "your-model"
context_window = 32768

[[configurations]]
name = "32k-shake-first"
provider = "openai"
model = "your-model"
context_window = 32768
[configurations.efficiency]
compaction_method_order = ["remote", "shake", "soft", "snapcompact", "handoff"]
```

Repeat for 64k, 96k and 128k and compare checked success and tokens per window. These rows measure; they
change no default.

Machine runs never copy suite grader commands into the agent's configuration or context. Command
assertions, including zero-exit ones without stdout expectations, run separately against the finished
workspace, identically in strict and non-strict variants. Every child configuration has no suite-owned
verification commands or requirements, and effective settings record
`case_requirement_mapping = "none_grader_independent_v2"`. The former
`single_turn_zero_exit_commands_v1` mapping is retired; treat its reports as not comparable with these.
Strict variants report unavailable unless an explicit, valid agent-visible verification contract is
supplied. No per-case schema for that contract exists yet, so strict variants are currently unavailable
for every suite case rather than measuring a no-op. File-only cases and journey assertions are not
converted into intermediate obligations.
Selective review still depends on the configured risk/size threshold; enabling it does not force a
review of every task. These experiments do not adopt settings automatically.

The suite defaults to `examples/benchmarks/efficiency.toml`. The command prints a table with checked
correctness, tool errors, malformed tool calls (tool contract errors), repair rounds, observed tokens and
wall time, plus deltas against `dial`; the JSON report includes each configuration's effective settings.
Token and cost totals come from the runtime usage ledger (main agent, workers and every purpose).
Configuration-level tokens require complete usage in every case; cost additionally requires complete
observed API pricing. Otherwise they stay null. Per-case `observed_cost_usd` is a lower bound unless
`observed_cost_complete` is true; local, subscription and unpriced requests are not zero-cost evidence.
The matrix does not gate on correctness, so a negative delta is measurement rather than failure.
`--baseline` and `--profiles` cannot be combined with it.

`tool_grammar` is also a user-level setting, `assistance.tool_grammar` (default `true`). It enables the
MLX tool-call grammar only for tokenizers whose parser is one of the formats listed under *Schema-constrained tool calls* in [improvement.md](improvement.md#fast-weight-candidates-without-long-training-runs); disabling it sets `DIAL_TOOL_GRAMMAR=0`
for the spawned server. `assistance.repair_hints` (default `false`) gates verified repair hints and verified
examples from prior successful repairs. `assistance.compact_tools` ships `true`; disabling it measures legacy automatic discovery/routing.

`assistance.constrained_repair` (default `true`) allows one constrained retry for local endpoints after a
malformed or schema-invalid tool call. The retry asks the endpoint for JSON that names one offered tool and
its arguments (LM Studio `response_format` with `json_schema`; Ollama `format`). The converted call passes the
same runtime schema validation and approvals as any other call, and an unconvertible reply is rejected as
before. The retry is sent at most once per malformed sequence, is never used in hermetic sessions, and is
recorded as a `constrained_repair` event. Its JSON may appear as streamed text before the call runs. Profiles
can set `capabilities.structured_output` to `json_schema`, `ollama_format`, or `none`. The default is
`json_schema` for `lm-studio`, `ollama_format` for `ollama`, and `none` for other providers; an external
llama.cpp `llama-server` is detected from its `/props`, and a vLLM or SGLang server from `owned_by` in
its `/v1/models` list; both use `json_schema`. Detection comes from their documented formats and has not
been run against live servers, so check that yours enforces the schema. Another OpenAI-compatible
server gets no retry, because a profile can declare `json_schema` only for `lm-studio`.

## Comparing with other agent CLIs

`examples/benchmarks/cross_agent.py` runs the same suite cases through different coding-agent
CLIs and scores every agent with one evaluator. Agents are listed in a TOML file such as
`examples/benchmarks/cross-agent.toml` (Dial and oh-my-pi's `omp`): argument lists for the first turn
and for follow-up turns that continue the session, plus a parser for the agent's JSON output.

```sh
python3 examples/benchmarks/cross_agent.py examples/benchmarks/quality.toml examples/benchmarks/efficiency.toml \
  --agents examples/benchmarks/cross-agent.toml --model claude-sonnet-4-6 --repetitions 3 \
  --jobs 1 --seed 0 --report benchmarks/cross-agent/report.json
```

Each trial runs in a fresh fixture copy with the same prompt and model. The script copies the fixture,
runs the agent and then runs the outcome checks against that copy. Seeded `context_messages` have no
portable CLI form, so every agent receives them inline before the first request. `metric` and
`tool_invoked` checks and `compact_before` need Dial's internal observations. They are reported
under `excluded_checks` / `skipped_compactions` and left out of every agent's score. A case with no
portable checks is not scored. Missing JSON output makes usage incomplete rather than zero.
Prompt tokens are normalized to include cache reads and writes. Tool calls are counted by name.
`delegation_tools` (default `["task"]`) identifies delegation calls; their trials are marked
usage-incomplete unless the agent definition declares `delegated_usage = "included"`. Dial's
ledger includes worker requests; omp's delegated usage is unknown. Incomplete trials remain
scored for correctness and wall time, but are excluded from token/request medians.
Each turn's raw stdout and stderr, including partial output on timeouts, are kept next to the
report in `<report-stem>.logs/<agent>-<case>-r<N>/`. Reports and existing log directories are not
overwritten. Logs can contain source, commands and provider output: treat them as private artifacts.

Pass `--capture-traces` to retain `turn-N.session.jsonl` for each Dial turn before its disposable
home is removed. Capture reads only the CLI result's named session under that trial's isolated
`DIAL_HOME`; it never falls back to installed user sessions. Lookup admits at most 10,000 entries,
does not follow directory symlinks, and reads only a regular, non-symlink journal of at most 4 MiB.
Malformed, branched or compacted histories are unavailable rather than replayed by the benchmark.
Each turn's `trace` records `captured` with a file pointer/byte count, or `unavailable` with a reason;
unsupported agents, missing terminal session identities, timeouts and launch failures are explicit.
Trace availability does not change outcome scores or turn incomplete usage into complete usage.
Capture duration (`trace.capture_ms`) is reported separately and excluded from trial `elapsed_ms`.

Captures preserve native tool arguments/results, assistant-message boundaries, and journal timing
metadata where present. An assistant message is not proof of every transport attempt or retry;
use the CLI efficiency counters for inference timing. Continued turns retain cumulative journal
snapshots, so do not sum their message counts. Files are owner-only (0600) under private trial log
directories (0700), are never overwritten, and are **unredacted private artifacts**: do not upload
them without review. The public report keeps only capture metadata, not journal contents. Use the
same capture setting on both sides of a comparison; instrumentation itself is not a speedup.

Built-in testing guidance targets owning tests for the changed entry point, not unrelated suites.
It still broadens checks for affected interfaces, uncertainty, explicit requests and required gates;
benchmark speed is not a reason to remove behavioral verification or weaken assertions.
See [agent-loop recovery and verification](agent-loop.md) for unchanged-read reuse and the
distinction between passing post-fix checks and missing pre-fix reproduction. Neither changes
required evidence credit or completion gates.

Dial runs with a default configuration: `cross-agent.toml` sets `DIAL_HOME = "{trial_root}/dial-home"`
(a directory inside each trial, so no user settings, MCP servers, skills, memory or experimental
options and no state shared between trials) and `DIAL_SUBSCRIPTION_AUTH_HOME = "~/.dial"`, which shares
only the signed-in subscription. `{trial_root}` expands to the trial's own directory; `{work_root}` is
the run-wide directory and is shared by every trial. Remove that `env` entry to measure an installed
configuration instead; opt-in features then count, for example Dial's `experimental.turn_optimizer`
adds one auxiliary request per user turn. omp runs as installed, with its user configuration,
sessions and global skills. The sample does not isolate omp's state per trial; record and match
configuration/state before attributing differences to the harness.

Use `--comparison-mode installed` (the default) for installed/configured setups. For an explicitly
controlled experiment, pass `--comparison-mode controlled --comparison-notes "..."` describing model,
reasoning, permissions, enabled integrations, state isolation and remaining differences. Controlled
runs require non-empty notes. The report retains this as an operator declaration, not proof that the
configurations are equivalent. Never put credentials in notes; they are stored verbatim.

Summaries include nearest-rank `p95_elapsed_ms_all` and two success-normalized measures:
`elapsed_ms_per_success` is the sum of elapsed time across **all scored attempts**, including failures,
divided by passed trials. `tokens_per_success` similarly amortizes total input plus output tokens;
it is unknown if any scored attempt has incomplete usage. Both are unknown when nothing passed.
These are workload-mix-dependent aggregates, not dollar costs or predictions of retry-until-success
behavior. Compare the same case/repetition mix, inspect per-case results, and retain failed attempts.
Small-sample p95 is usually the maximum; it does not establish a stable population tail latency.

Diagnostics recorded per run:

- Agent identity is captured before any trial: `version` (the `version` command output) and the
  resolved `binary_path` with the SHA-256 of that executable (`binary_sha256`). A missing binary
  records `null` rather than failing silently.
- Trial order is seeded (`--seed`, default 0). Each repetition and case block lists every agent once,
  blocks are shuffled, and each block rotates a seeded agent order, so every agent takes each slot
  equally often when the number of blocks is divisible by the agent count (otherwise counts differ
  by at most one). The report stores the exact `trial_order` so runs can be reproduced.
- Wall times compare agents only with `--jobs 1`. Higher job counts share the machine between trials;
  the script warns and the report sets `timing_comparable` to false.
  A true `timing_comparable` flag only confirms serial trial scheduling, not matched configuration,
  provider load, cache state or statistical significance.
- Dial's per-turn `efficiency` object is parsed as emitted into `efficiency` with four fields: `duration_ms`,
  `inference_latency` (request timer counters plus an `attribution` string), `storage_io` (inclusive
  SessionIoMetrics counters) and `projection_bytes`. The count maps `requests_by_phase`,
  `prefix_invalidations`, `recovery_attempts` and `backend_operations` are nested inside
  `projection_bytes`, not at the root. Phase keys are `adaptive::Phase` labels (`auto`, `investigation`,
  `editing`, `verification`, `recovery`, `exploration`, `general`). A trial's `efficiency` sums turns only
  when every turn reported each field; additive numeric leaves add, strings and first-result latencies
  stay per turn, and absent dynamic-map labels count as zero. Otherwise that field is `null` (unknown),
  never zero. Failed trials are unknown.
  Request phases are policy labels, not error counts: a recovery phase can persist across later
  successful shell observations. `recovery_attempts` counts separately instrumented recovery paths;
  an empty map does not establish that no tool failed. Use `failed_tool_results` and individual
  `tool_calls[].is_error` records for failure counts, and tool-result diagnostics for their causes.
  `inference_latency`, `storage_io` and `backend_operations` are inclusive costs that nest inside the
  turn's duration, so they are not a partition of wall time and must not be added to it.
  Prefix measurement additionally records `request_prefix.elapsed_us` in the session journal and
  aggregates it as `projection_bytes.backend_operations.request_prefix` (`calls`, `elapsed_us`). This
  measures provider-neutral serialization/hash preparation, excluding journal publication and model
  inference; it overlaps request preparation and must not be added to turn wall time. Full historical
  messages are remeasured after hooks, so this does not assume unchanged content or provider cache hits.
- A non-zero exit records the exit code, the stderr log path (`stderr_log`) and a bounded excerpt
  (`stderr_tail`, last 800 characters) after best-effort redaction of token-like values. Excerpts are
  not a guarantee against leaking secrets: the raw logs are unredacted private artifacts. The progress
  line prints the exit code and log path, never the environment.
- The summary's `trials with efficiency` row counts trials whose four required efficiency fields were all
  reported (`efficiency_known`); `efficiency_projection_totals` sums `projection_bytes` counters over those
  trials only.
- For per-call Dial diagnostics, configure `--output-format stream-json` instead of `json`.
  The same parser reads usage from the terminal `result` event without counting intermediate
  events twice. Raw stdout additionally retains tool calls and results, including error text
  bounded by the CLI's event limit. Without a terminal result, usage remains incomplete.
  These private logs improve attribution; they are not live performance or correctness evidence.

The results compare these setups on these fixtures. They are not a leaderboard score.
The stub-agent regressions run with `python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'`.

### Offline Dial/omp overhead comparison (macOS)

`offline_compare.py` runs a baseline Dial binary, a candidate Dial binary and **oh-my-pi (`omp`)**
serially against the same scripted Anthropic Messages policy on loopback. It does not call a model.
Use optimized binaries built with the same profile and a fixed source snapshot for each Dial version:

```sh
python3 -m unittest discover -s examples/benchmarks -p test_offline_compare.py
python3 examples/benchmarks/offline_compare.py \
  --baseline /path/to/dial-baseline --candidate /path/to/dial-candidate \
  --omp /path/to/omp --repetitions 5 --seed 0 --timeout 30 \
  --report benchmarks/offline-comparison/results.json
```

The eight workloads cover a fixed answer, read/write, three Python-file mutations, positive/negative
search, code-location search, a parent-directory write refusal and session continuation. Code-location
search uses Dial's code `find` with `related=false` and omp's content `grep`, not omp's filename `find`.
The mutation workload reads complete files before rewriting them, exercising verification overhead;
it does not compare the agents' different edit grammars. Continuation uses single-file reads, so it
does not establish the separate multi-path-read witness repair. The refusal case compares observed
tool policy: an accepted parent-directory write is a failed refusal check, not an infrastructure error.
Do not compare successful-refusal latency with successful-write latency or treat this as a security audit.

Each trial has a fresh `HOME` and agent state directory. Only `PATH`, `LANG`, `LC_ALL` and `TMPDIR`
are inherited. Placeholder credentials target loopback; an outer macOS `sandbox-exec` policy denies
non-loopback egress and self-tests that denial before trials. Dial uses `--sandbox off` inside that
shared network policy because nested macOS sandboxes cannot be applied. This is not filesystem
isolation. omp disables ambient extensions, skills, rules, context files, title generation, prewalk,
LSP and PTY; Dial uses its fresh-home defaults. Tool schemas, policies and runtimes still differ.

Reports retain binary/version identities, driver/workload hashes, seeded balanced order, acceptance
checks, raw logs and workspaces. Normal unsupported localhost discovery GETs return 404 and are
counted separately from Messages requests; unknown endpoints remain transport failures. Compare
per-case medians/ranges, not a pooled median across different tasks. Wall time includes CLI startup,
tool work, persistence and automatic verification. `first_arrival_ms` also includes fixture preparation;
it is **not** an exact process-spawn-to-first-request metric. Connection counts cover Messages
connections only, not capability-probe sockets. System/schema lengths are JSON representation
sizes; body length is received request bytes, not tokenizer output. Usage tokens are synthetic and
must not be used to claim token, cost or cache-hit savings. Scripted outcomes do not measure model
quality, provider latency, TLS, CPU, memory, interactive rendering or live cache behavior.

### SWE-bench Verified local comparison

`swe_bench.py` adapts frozen SWE-bench Verified rows to the same CLI runner and usage parsers,
then grades patches with the official **`swebench==4.1.0`** Docker harness. Version 5 uses a
different dataset schema; do not silently substitute it for the legacy Verified snapshot.
This is a host-agent/Docker-test local experiment, not a leaderboard submission.

Install dependencies into a project-local environment, then freeze a selection before running:

```sh
uv venv --python 3.12 benchmarks/swebench-env
uv pip install --python benchmarks/swebench-env/bin/python 'swebench==4.1.0'
benchmarks/swebench-env/bin/python - <<'PY'
import json, sys
from pathlib import Path
from datasets import load_dataset
from huggingface_hub import HfApi
sys.path.insert(0, 'examples/benchmarks')
from swe_bench import select_instances
name = 'princeton-nlp/SWE-bench_Verified'
revision = HfApi().dataset_info(name).sha
rows = select_instances(list(load_dataset(name, revision=revision, split='test')), 10, 0)
root = Path('benchmarks/swebench-selection')
root.mkdir(mode=0o700, exist_ok=False)
(root / 'selected.json').write_text(json.dumps(rows))
(root / 'manifest.json').write_text(json.dumps({
    'dataset': name, 'revision': revision, 'seed': 0,
    'instances': [{k: r[k] for k in ('instance_id', 'repo', 'base_commit')} for r in rows],
}))
PY
benchmarks/swebench-env/bin/python examples/benchmarks/swe_bench.py prepare \
  --dataset benchmarks/swebench-selection/selected.json \
  --manifest benchmarks/swebench-selection/manifest.json --output benchmarks/swebench-pilot
benchmarks/swebench-env/bin/python examples/benchmarks/swe_bench.py run --output benchmarks/swebench-pilot
benchmarks/swebench-env/bin/python examples/benchmarks/swe_bench.py grade --output benchmarks/swebench-pilot
```

`prepare` downloads prebuilt AMD64 images, records image/executable identities and exports only
the base-commit source. AMD64 emulation must work on ARM hosts. Images can use substantial disk;
dependencies, images and live inference require operator approval. `run` uses balanced serial
task pairs, Sonnet 4.6 with thinking off by default, one attempt and a 900-second agent deadline.
Both agents get the issue and the same Docker test helper, not reference patches, hints or hidden
acceptance tests. The helper sends a working-tree patch into a fresh container without mounting
the host workspace or Docker socket. Hidden acceptance is applied only by the final grader.

The CLI runner requires POSIX process groups. With `sandbox.mode` set to `dev` or `strict`, Dial's shell sandbox can block the Docker
socket. Only with explicit operator approval, add `--dial-sandbox-off` to `prepare` for those child
trials; it is off by default in the adapter and is recorded in the frozen command templates.
This removes Dial's shell confinement and permits host access, matching omp's auto-approved host
shell exposure, **not** containerizing either agent. The test containers have networking disabled;
the host-native agents do not. Grader data and logs are in a separate root from trial workspaces;
the common prompt forbids inspecting them, but this is not adversarial isolation.

Dial has fresh per-trial configuration. To retain authentication without copying secrets, omp
uses its installed configuration with ambient extensions, skills, rules, context files and prewalk
disabled. Main/auxiliary model roles are pinned, but this is **not configuration-matched**;
report that limitation rather than attributing a score difference purely to the harness.

The run directory retains private logs, trial records, standard prediction JSONL files and the
final `report.json`. Completed trials are reused only under unchanged runner/dataset/executable
contracts; an interrupted started trial is refused instead of silently buying another attempt.
Unavailable grading stays in the selected-task denominator and is reported separately from
unresolved patches. A partial run is not a completed pilot score. One attempt on ten tasks cannot
establish repeated-trial reliability or broad superiority. Offline adapter checks are:
`python3 -m unittest discover -s examples/benchmarks -p test_swe_bench.py -v`.

### Reliability ledger cases (`reliability.toml`)

`examples/benchmarks/reliability.toml` holds three harder offline cases on `reliability-fixture/ledger.py`,
each starting from a different wrong state: a float-rounding false success (`amount-exact-decimal`), a
dropped refund serialization branch (`ledger-export-keeps-refunds`), and a silently swallowed malformed
row (`malformed-row-fails-closed`). Acceptance commands live in the suite file, outside the disposable
project, so editing visible tests cannot change the verifier. The visible tests (`python3 -m unittest
test_ledger`) are only a subset of acceptance: passing them is not sufficient.
`test_reliability_cases.py` runs offline mutant regressions: wrong fixes that pass visible tests must fail
the acceptance commands, and replacing the visible tests cannot defeat them. These are harness checks,
not model results.

### Local task-level suite (`local-tasks.toml`)

`local-tasks.toml` is a `BenchmarkSuite` for `dial benchmark`, run over the fixed local
`local-tasks-fixture` (no web or network sources). It covers six tasks: cited research over
`source_a.md`/`source_b.md`, deferred tool discovery (`ledger.py`), unavailable-tool recovery
(`notes.txt`), a read/edit/test repair of `calc.py`, denial-stop, and snake/test-harness recovery.

Grading is by outcome. The first command check of each case is independent of any file the agent
can edit: `calc.py` is checked against fixed `add` boundaries, and research must associate each
figure and direction with its own source line. The visible `test_calc.py` is still run as a
prompt-level check, but weakening it cannot pass the case. Valid JSON, tool counts and error text
are never task success. Denial passes only when `../denied_probe.txt` does not exist and the reply
is `STOPPED`.
The snake case is a small movement-logic fixture, not the user's game or a curses/UI test. It pairs
a wrong head index with an invalid unittest base class. Independent checks cover movement in all
four directions, body length/input preservation and a single segment, then run the repaired visible
suite requiring both tests to execute without skips, check that the original assertion calls remain,
and require the run command in `RUN.md`. Fixing only the code or only the harness cannot pass. It exercises diagnosis of a
test-harness failure separately from a program defect. Assertion-call preservation is a structural
guard, not a proof against arbitrary adversarial test code.

`tool_invoked` and `metric` checks are integration-path coverage, reported separately from task
outcomes. They show that the path was exercised, not that the answer is correct. Deferred discovery
requires `tool_search`, `tool_load` and `ast_search` (a deferred builtin in `DEFERRED_BY_DEFAULT`,
`crates/dial-tools/src/tools/mod.rs`); empty-discovery recovery requires a `tool_search` call for
`zzqxjvkmnop`, absent from builtin names and descriptions (the real-dispatch empty-search test uses
the same query; these profiles use core integration isolation); denial requires a `write` call and a tool error. The outside-workspace path is rejected
by the file tool itself, even with automatic ordinary approvals; the prompt forbids requesting
additional directory access.

Profiles are separate files:

- `local-tasks-profiles.toml`: local-only (`allow_remote = false`, LM Studio). Replace
  `REPLACE_WITH_LOADED_MODEL_ID` with the exact ID of a model already loaded. No model availability is
  assumed.
- `local-tasks-hosted-profiles.toml`: hosted reference (`anthropic`) with `allow_remote = true`. This
  is an explicit opt-in that sends prompts off the machine and spends provider quota. Run it only
  deliberately, not in CI.

Usage: `dial benchmark examples/benchmarks/local-tasks.toml --profiles examples/benchmarks/local-tasks-profiles.toml`
(or the hosted file). These files only select configurations; nothing runs until you invoke
`dial benchmark`, and no inference was run for this change.

### Headline score

`local-tasks` is Dial's headline task score: one fixed, outcome-graded suite tracked across
releases. Record correctness first, then tokens and wall time for matched successful cases.

```sh
make bench-headline DIAL=target/local/dial                      # writes target/benchmarks/headline-<time>.json
make bench-headline DIAL=target/local/dial BASELINE=target/benchmarks/headline-<earlier>.json
```

`HEADLINE_PROFILES` selects the profile file (default `local-tasks-profiles.toml`, whose model
placeholder must be replaced first). Reports and their progress journals go under the Cargo
target directory rather than `$TMPDIR`, so an interrupted run keeps its evidence. With `BASELINE`,
`compare.py` reports correctness regressions before speed. A score is a measurement only for the
recorded model, machine and build (`execution_build`); compare runs with the same profile.

When a case misses its whole-case deadline, the failure detail names the elapsed time of every
completed RPC phase (`ready`, `seed_context#N`, `compact#N`, `send#N`) and how long the pending
request had been waiting, for example
`case elapsed 180.0 s; completed: ready 3.0 s, send#1 40.0 s, compact#2 57.0 s; pending for 80.0 s`.

Offline checks: `cd examples/benchmarks && python3 -m unittest test_local_tasks` verifies that each
starting state fails, each reference fix passes, mutable-test weakening cannot hide the original
`calc`/snake defects, partial snake/harness repairs and input mutation fail, and swapped or merged
research figures are rejected. These checks use no model or network. Live runs retain the existing
success, tool-call/error and turn-efficiency metrics (including exact inspection overlap where
available); offline fixture checks do not establish better local-model task success or speed.

The scripted `offline_compare.py` harness is a transport and wrapper regression tool. Its tool
scripts are not a model benchmark and its results must not be reported as model quality.

## Recorded runs

Single-model runs accept `--checkpoint PATH` and `--resume` for durable case checkpoints. Resume
requires the same suite, frozen source, executable and scrubbed configuration contract; incomplete
usage and agent failures remain retryable. Profile and assistance matrix runs freeze one source
snapshot and emit contracts but currently reject checkpoint/resume flags. Comparisons refuse
incompatible recorded source/suite contracts. Scripted regressions establish harness behavior;
real-model quality and performance require matched provider runs.

The [bounded hosted harness study](plans/hosted-harness-study.md) records 48 measured
Claude Sonnet 4.6 subscription trials across lifecycle, representative-task and compaction
profiles. It retains failures and unknown invoice costs, makes no default promotion, and
separates the measured executable from the subsequent offline-tested recovery-guidance fix.
Raw reports and executable/source hashes remain under ignored
`benchmarks/hosted-harness-study/`.
