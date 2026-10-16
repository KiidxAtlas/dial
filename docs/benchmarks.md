# Benchmarks and measurement

Benchmark suites, efficiency measurement, workflow regression checks, configuration comparisons and assistance ablation.

**Invalid runs.** A benchmark run in which no case shows any model activity (for example, every case fails immediately with an agent error before a model request) is reported as an invalid run and fails the command, instead of being reported as a 0 score. The JSON report records the reason in `invalid_run`, and each case keeps its `evidence.agent_error_detail`; fix the setup problem before comparing results.

## Measuring efficiency

From this checkout run `dial benchmark examples/benchmarks/efficiency.toml --profiles machine.toml --report efficiency-report.json`
using the machine/profile configuration described below. The five disposable cases cover a simple
question, a one-file edit, a failing-test repair, a multi-file rename, and continued-session compaction.
Compare correctness first, then observed prompt/output/cache/reasoning usage, calls, errors and elapsed
time. The simple-question case checks tool avoidance only; manually review its answer for correctness.
The continued-session case tests multi-turn compaction, not process restart/resume. No live-model savings
are claimed by these fixtures, and prompt cache hits need not be free under a subscription limit.

For evidence-led quality work, use `dial benchmark examples/benchmarks/quality.toml --report target/quality.json`.
Its billing fixtures cover locating production code among archival decoys,
quantity/empty-input boundaries, a rename through callers, environment-versus-code diagnosis,
and rejecting stale historical discount advice. Two journey cases continue one session across
follow-up turns: an iterative feature that must survive a forced compaction, and a corrected
requirement where the latest instruction overrides the first request. Acceptance commands are outside the copied
project, so changing visible tests cannot weaken them. No third-party Python packages are needed.
Run `python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'` to verify the
evaluation machinery and fixture acceptance contracts without making model calls.

Compare repeated runs with the same fixture revision and model using
`python3 examples/benchmarks/compare.py before.json after.json` (CLI report envelopes are accepted).
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

These are descriptive measurements, not independent `pass@k`/`pass^k` estimates or proof that a
small sample generalizes. Metric-only/manual-review tasks are excluded from checked correctness.
Keep model/configuration and fixture versions fixed when comparing harness revisions, and inspect
traces and final patches for output quality that executable checks do not capture. The underlying
runner already isolates each task and repetition; this analyzer does not launch more model calls.

To exercise the real repeated-run/reporting path without credentials or a remote model:

```sh
python3 examples/benchmarks/smoke.py --dial ./target/debug/dial --suite recovery --machine-repetitions 2
python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'
```

The smoke uses a loopback scripted provider with baseline and compact-tool configurations.
It verifies orchestration and graders, **not real-model performance**. See
[Harness quality design](harness-quality.md) for the research rationale and interpretation rules.

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
Each case has fresh state against one source snapshot, with automatic improvement, secondary model
roles, search and escalation disabled. Reports include machine identity, settings, suite hash, checked
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

Supported keys: `enabled`, `compact_tools`, `initial_tool_limit` (0 = unlimited, or 1..=64), `tool_grammar`,
`repair_hints`, `syntax_guard`, `selective_verification`, `edit_mode`, `constrained_repair`, `lean_local`, and
`reliability_gate` (maps to `reliability.enabled`). Unknown keys are rejected.

## Assistance ablation

`dial benchmark --ablate-assistance [suite.toml] [-m model] [--report out.json] [--allow-remote]` runs an
automatic matrix on one model: `raw` (assistance off), `dial` (the shipped defaults, independent of your
config), `lean` (`dial` plus the `lean_local` profile, which only changes requests to a locally served model),
and `ablate-<feature>` for each of `compact_tools`, `initial_tool_limit`, `tool_grammar`,
`repair_hints`, `edit_mode` and `reliability_gate`. Each ablation changes exactly one feature from `dial`.
The suite defaults to `examples/benchmarks/efficiency.toml`. The command prints a table with checked
correctness, tool errors, malformed tool calls (tool contract errors), repair rounds, observed tokens and
wall time, plus deltas against `dial`; the JSON report includes each configuration's effective settings.
It does not gate on correctness, so a negative delta is measurement rather than failure. `--baseline` and
`--profiles` cannot be combined with it.

`tool_grammar` is also a user-level setting, `assistance.tool_grammar` (default `true`). It enables the
MLX tool-call grammar only for tokenizers whose parser is one of the formats listed under *Schema-constrained tool calls* in [improvement.md](improvement.md#fast-weight-candidates-without-long-training-runs); disabling it sets `DIAL_TOOL_GRAMMAR=0`
for the spawned server. `assistance.repair_hints` (default `true`) gates verified repair hints and verified
examples from prior successful repairs.

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
