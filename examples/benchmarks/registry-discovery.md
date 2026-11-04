# Tuple registry discovery regression

`registry-discovery.toml` reuses the existing `cross_agent.py` runner. Its portable
command check independently verifies `answer.json`; the runner additionally grades
ordered native calls for cases tagged `registry_discovery_batch` or
`registry_discovery_one_call`. Passing requires **both** checks. Running the TOML
through `dial benchmark` alone checks accuracy, not ordered discovery behavior.

## Offline contracts (no model, binary, network or installation)

```sh
python3 -m unittest discover -s examples/benchmarks -p 'test_registry_discovery.py'
make test-bench-offline
```

These test deterministic good/bad transcripts and exercise the existing runner with
a scripted process boundary. They are **not live-model quality measurements**.

## Live model tier (optional, not run by offline tests)

With an already-built `dial` on PATH and the provider subscription configured:

```sh
python3 examples/benchmarks/cross_agent.py \
  examples/benchmarks/registry-discovery.toml \
  --agents examples/benchmarks/cross-agent.toml --only dial \
  --model YOUR_MODEL --thinking off --jobs 1 --repetitions 3 \
  --report /tmp/registry-discovery-report.json
```

This uses the existing isolated per-trial `DIAL_HOME` from `cross-agent.toml`;
only subscription authentication is shared. To use another provider, supply an
operator-owned agents TOML using the same `dial-json` output and a per-trial
`DIAL_HOME = "{trial_root}/dial-home"`. Do not point the home at operator sessions.
No grader invokes a model, installs dependencies or initiates provider requests.
The command above does invoke the selected model and may incur provider costs.

The runner resolves the session ID in Dial's terminal JSON result to exactly one
`*_<session_id>.jsonl` beneath that isolated home. Before cleanup it exports the
session to `<report-stem>.logs/<agent>-<case>-r<N>/discovery-trace.jsonl`. These are
raw, private artifacts like the existing stdout/stderr logs. Do not publish them
without reviewing their contents. A missing, ambiguous, oversized, unsupported or
non-isolated transcript is `unavailable`, never a successful trace check.
Non-Dial agents can still run accuracy checks, but cannot pass this Dial-specific
trace contract without a supported transcript adapter.

`discovery_trace` in each trial records the result, failure reasons, observed policy
modes, source inspections, useful batches and assistant round trips (including the
final answer). These are assistant replies, **not** failed transport retries or
provider-wide request accounting. Existing usage counters remain separate.

## Acceptance semantics

- Commands are positional Rust tuples. Independent docs, TOML settings and Rust
  keybindings supply the other answers; correct command count alone cannot pass.
- Actual tool result content must expose the declaration and all independent
  evidence. Repeating empty registry-shape searches before reading the declaration
  is rejected even if a later read recovers the correct answer.
- Batch mode requires at least two distinct independent evidence sources in one
  round, through multiple tools or a multi-path read. Repeated reads of one source
  do not count. Batch mode has a five-assistant-round budget; one-call mode has
  eight. These budgets are fixture-specific, not runtime tool-call quotas.
- Explicit one-call mode rejects multiple native calls in one assistant response;
  a multi-path read is still one call. It does not demand batching.
- Session `dial_state` state/patch records must expose `reliability.call_policy`.
  Missing policy is unavailable. A runtime one-call restriction makes the batch
  case unavailable, not an inefficiency failure. The explicit one-call case may
  run under a batch-capable runtime because the user's stricter instruction wins.
- The grader supports fresh, single-turn, uncompacted/unbranched transcripts.
  It does not attempt an independent implementation of storage's branch replay.

Live-model pass rates and actual CLI/session compatibility must be measured using
the command above; offline fixture acceptance does not establish them.
