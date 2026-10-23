# Benchmarks and measurement

Benchmark suites, efficiency measurement, workflow regression checks, configuration comparisons and assistance ablation.

**Invalid runs.** A benchmark run in which no case shows any model activity (for example, every case fails immediately with an agent error before a model request) is reported as an invalid run and fails the command, instead of being reported as a 0 score. The JSON report records the reason in `invalid_run`, and each case keeps its `evidence.agent_error_detail`; fix the setup problem before comparing results.

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

For evidence-led quality work, use `dial benchmark examples/benchmarks/quality.toml --report target/quality.json`.
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
It verifies orchestration and graders, **not real-model performance**. See
[Harness quality design](audits/harness-quality.md) for the research rationale and interpretation rules.

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
mode = "browser" # or "delegation", "mcp"; default is "core"
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
python3 examples/benchmarks/interactive_smoke.py --dial target/debug/dial --captures /tmp/dial-pty-captures
```

The second check requires Python 3.11+, Git and tmux and creates its own disposable Git root,
private configuration, isolated tmux server and loopback provider. It exercises delayed-discovery
favorite switching/cancellation, picker resizing, wrapped input cancellation, full blockers, and
queue management, refused model restart and live help during a gated SSE reply. Streaming answer
markers must appear exactly once, without cancelling or regenerating the request.
Captures are real terminal text, **not pixel screenshots**. Scripted provider runs verify harness
behavior, **not model quality or a measured model-speed improvement**. Run
`examples/benchmarks/recovery.toml` through the benchmark CLI with a selected real model to obtain
model-specific measurements; compare verified task success alongside tool/error counts.

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

- `enable-compact_tools` and `enable-repair_hints` turn on features that ship off;
- `ablate-initial_tool_limit`, `ablate-tool_grammar`, `ablate-edit_mode`, `ablate-reliability_gate`,
  `ablate-lean_local` and `ablate-local_profile` turn off or change features that ship on.
  `ablate-local_profile` sets `assistance.local.enabled = false`; see the local profile in `agent-loop.md`.
- `enable-strict_requirements` and `enable-independent_review` turn on the shipped-default-off
  outcome/review policies; their baseline settings and effective values are recorded as above.

Machine runs map each single-turn case's zero-exit command assertions without stdout expectations to
suite-owned verification commands and requirements, identically in strict and non-strict variants.
The frozen suite and executable contract identify this mapping; effective settings record
`case_requirement_mapping`. Independent acceptance still executes outside the model-editable copy.
Strict variants without an applicable contract report unavailable rather than measuring a no-op:
file-only cases and final-only journey assertions are not invented intermediate obligations.
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
examples from prior successful repairs. `assistance.compact_tools` ships `false`.

`assistance.constrained_repair` (default `true`) allows one constrained retry for local endpoints after a
malformed or schema-invalid tool call. The retry asks the endpoint for JSON that names one offered tool and
its arguments (LM Studio `response_format` with `json_schema`; Ollama `format`). The converted call passes the
same runtime schema validation and approvals as any other call, and an unconvertible reply is rejected as
before. The retry is sent at most once per malformed sequence, is never used in hermetic sessions, and is
recorded as a `constrained_repair` event. Its JSON may appear as streamed text before the call runs. Profiles
can set `capabilities.structured_output` to `json_schema`, `ollama_format`, or `none`. The default is
`json_schema` for `lm-studio`, `ollama_format` for `ollama`, and `none` for other providers.

## Recorded runs

Single-model runs accept `--checkpoint PATH` and `--resume` for durable case checkpoints. Resume
requires the same suite, frozen source, executable and scrubbed configuration contract; incomplete
usage and agent failures remain retryable. Profile and assistance matrix runs freeze one source
snapshot and emit contracts but currently reject checkpoint/resume flags. Comparisons refuse
incompatible recorded source/suite contracts. Scripted regressions establish harness behavior;
real-model quality and performance require matched provider runs.
