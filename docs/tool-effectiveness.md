# Tool-effectiveness evaluation

Offline analysis of repeated machine benchmarks that asks whether tool use helped complete verified
tasks, at what observed work cost, and whether a configuration changed that work. It never calls a
model or judge, never estimates quota or dollars, and never proves a live-model improvement.

## What is measured

Correctness comes only from recorded outcome checks, via the same validation as
[`reliability.py`](../examples/benchmarks/reliability.py) (run contracts, complete repetition sets,
identical task/source/verifier hashes, invalid-run refusal). Tool analysis then reports:

- **Per successful trial** (checked successes only): median tool calls, tool errors, tool contract
  errors, observed tokens and elapsed time.
- **All trials**: totals of calls, errors, contract errors, repair attempts, observed tokens and time,
  including failed and unavailable trials.
- **Recovery**: among trials with observed tool errors, how many still passed their checks.
- **Trace coverage**: trials with a trace, trials whose trace is complete, and trials without one.
  `invoked_calls_observed` is a per-tool lower bound summed over trials that carry a trace.
- **Declared tool expectations** (opt-in, see below): per-tool `used`, `declared_not_used`,
  `not_offered` and `unknown` counts, plus a declared-use rate over assessed trials.
- **Paired comparisons**: against the named baseline, only on tasks passing every trial in both
  configurations (matched success populations). Ratios are summed per-success medians; a task with
  fewer checked successes than the baseline is a listed regression.

Unknown stays unknown: missing usage, missing counters or missing trace evidence produce `null`,
never zero. Unavailable checks and agent errors stay in denominators.

Metric-only tasks and manual-review tasks are not checked successes. The suite deliberately contains
no tool-count checks: a successful coding task is judged by its final workspace, not by how many
calls it took.

## Fixture suite

`examples/benchmarks/tool-effectiveness.toml` (project `tool-effectiveness-fixture/`) has five
independent-outcome cases, each failing on the original fixture:

| case | exercises | outcome |
| --- | --- | --- |
| `selection-find-function` | selecting a tool to locate a function among decoys | `selection.txt` names the function |
| `composition-rename-consumers` | rename across definition and consumer, chained lookup | verifier runs, no stale name |
| `requirement-boundary-floor` | empty, negative, under/exact/over-limit boundaries | verifier plus unchanged settings |
| `long-context-current-code` | seeded superseded notes, one follow-up turn with compaction | current constant written, settings unchanged |
| `diagnosis-environment-not-code` | environment failure versus application defect | `diagnosis.txt` and environment-gated runner |

Acceptance commands run outside the copied project. `test_tool_effectiveness.py` checks that the
original fixtures fail and that a reference repair passes each case.

## Tool trace (main agent)

Each benchmark case's `metrics.tool_trace` records what the **main agent** did in that trial:

```json
"tool_trace": {
  "scope": "main_agent",
  "complete": true,
  "successful_requests": 3,
  "offered": {"read": 3, "find": 1},
  "invoked": {"read": 1}
}
```

- `offered[tool]` counts successful main-agent provider replies whose request offered that tool's
  schema. A reply counts when it is received, so a text-only final answer offers its schemas even
  though no tool runs, and a reply later discarded by portable-guidance regeneration still counts
  because its schemas were sent. Pre-reply regenerations, stream failures and reasoning interceptions
  are not counted.
- `invoked[tool]` counts admitted calls that reached dispatch, including tool-level permission refusals
  and runtime failures. Dispatch does not establish process execution or effects. Calls rejected for their form (unknown tool, invalid arguments, batch-contract or
  recovery-checkpoint refusals), skipped by steering, superseded, or answered from an identical
  repeat are **not** executions and are not counted.
- `complete` is `false` when the run failed or any turn's snapshot was missing. The counts from
  a partial run are still reported, but they are not a complete population.
- The counters are cumulative for the runtime. The benchmark child keeps the last snapshot, so a
  multi-turn case reports its whole session once.
- Worker, extension and compaction-internal model calls are **outside** this trace.

**Eligibility** here means only that the tool's schema was offered on a successful main-agent request
under the active role and mode. It does not mean the tool was approved to execute, connected, or
permitted by policy. Approval and sandbox refusals happen after the offer and appear as dispatched calls
or form rejections, not as the absence of an offer.

Reports from builds without this field keep `tool_trace` absent. Absent traces are always `unknown`.

## Tool expectations sidecar (opt-in)

Suite cases cannot declare tool expectations, so they live in a separate TOML file keyed by case ID.
Expectations are human-written declarations for analysis. They never route tools at runtime.

```toml
version = 1

[[cases]]
id = "selection-find-function"
contract_sha256 = "<64 hex from evidence.contract_sha256 of that case in the report>"
expected_tools = ["find"]           # tools a reviewer expects this task to use
opportunity_tools = ["tool_search"] # tools that were available for the task; not a missed-opportunity claim
```

Each entry must match the verifier contract hash in the report; mismatches and absent case IDs are
rejected. Unknown keys and duplicate IDs are rejected.

Per trial, a declared tool is classified as:

| status | condition |
| --- | --- |
| `used` | `invoked[tool] > 0`. Recorded executions are observed even when the trace is incomplete. |
| `not_offered` | the trace is complete, with at least one successful request, and `offered[tool]` is 0. |
| `declared_not_used` | the trace is complete, with at least one successful request, and the tool was offered but never executed. |
| `unknown` | no trace, an incomplete trace without an execution, or no successful request. |

Only `declared_not_used` means "offered but not used" and is never called a missed opportunity. The
analyzer never reports `missed` and never infers that an unused tool was optimal or wrong. A missing
call alone is never evidence of a missed opportunity, and an incomplete or absent trace never
produces `not_offered` or `declared_not_used`.

The `declared_use_rate` is `used / (used + declared_not_used)`. It is `null` when neither occurs.

### Availability versus connection

A tool offered in a request means its schema was provided to the model. It does not mean an MCP
server or extension is connected, that the tool is installed, or that its execution is approved.
Connection state is not part of this trace, so an installed or configured but unconnected tool shows
as `not_offered` or `unknown`, not unavailable and not missed.

## Commands

Offline (no model, no credentials):

```sh
python3 -m unittest discover -s examples/benchmarks -p 'test_tool_effectiveness.py'
python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'
```

Repeated real-provider runs (not launched by the tooling; these consume quota):

1. Write a machine configuration with the existing schema, for example:

   ```toml
   [machine]
   repetitions = 3
   minimum_correctness = 1.0
   allow_remote = false

   [[configurations]]
   name = "baseline"
   provider = "openai"            # your provider
   model = "your-model-id"
   context_window = 32768

   [[configurations]]
   name = "no-tool-grammar"
   provider = "openai"
   model = "your-model-id"
   context_window = 32768

   [configurations.assistance]
   tool_grammar = false
   ```

   Keys like `compact_tools`, `initial_tool_limit`, `repair_hints` and `[configurations.efficiency]`
   are also supported. The first configuration is the baseline. Each ablation changes only the keys it names.

2. Run and analyze:

   ```sh
   dial benchmark examples/benchmarks/tool-effectiveness.toml --profiles machine.toml --report tool-effectiveness-raw.json
   python3 examples/benchmarks/tool_effectiveness.py tool-effectiveness-raw.json \
     --expectations tool-expectations.toml --output tool-effectiveness.json
   ```

The analyzer exits 1 when any configuration has an observed per-task correctness regression against
the baseline, and 2 for invalid input or an existing output file. Keep the model, fixture revision and
harness build fixed across configurations, and report infrastructure failures separately.

## Limits

- Repetitions are few; per-task frequencies are descriptive and carry no significance claim.
- Medians over successful trials are not means; small populations make them unstable.
- The trace covers the main agent only. Worker and extension tool use is not represented.
- A declared `declared_not_used` is a review signal, not evidence that a different tool choice would
  have succeeded.
- Scripted-provider runs test plumbing only. They are not model quality, latency or quota evidence.

## Representative and integration extensions

Use `representative.toml` for dependency-free Rust workspace/non-default-member, native TS/JS caller
boundaries and a seven-request corrected/retracted journey across three compactions. Its external
acceptance bodies and exact preservation checks use the same immutable source/task contracts and
repeated-run analysis as this suite. See [benchmarks.md](benchmarks.md#representative-multi-language-and-long-journey-suite).

Per-model/task experiments can set `[configurations.policy]` (`strict_requirements`, `review_enabled`,
`review_min_changed_files`, `review_sensitive_paths`, `review_max_rounds`). Effective policy v1/hash and
full configuration contracts make variants auditable. Change one lever per comparison; fewer tool
calls or less time never replace checked outcomes, and no setting is automatically adopted.

The separate `integration-{browser,delegation,mcp}.toml` suites require explicit matching machine
integration modes and real prerequisites. They are not offline model-quality tests. `tool_invoked`
records admitted dispatch/path coverage only and contributes **zero** independent outcome checks;
each fixture separately declares a task outcome. Unknown trace/prerequisite evidence must not be
reported as successful connection or task completion. Offline fixture tests do not start services,
launch browser UI, call providers or install packages. See the integration lane protocol in
[benchmarks.md](benchmarks.md#opt-in-integration-evaluations) before deliberately running these suites.
