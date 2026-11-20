# A smaller, measurable coding harness

Decision: improve the existing edit path first. Then test whether less advice helps, before changing cache or loop architecture around today’s prompts. Preserve the sandbox, approvals, source witnesses, atomic writes and honest verification in every variant. These are correctness constraints, not benchmark levers. This is a sequence of independently reversible changes, not a second agent loop.

## 1. Repair less source, prove the requested behavior survived

**Implemented first slice:** Dial already had an opt-in, tool-free coach call and a full-file parse gate. The built-in root and worker coaches now receive the working and proposed versions of the changed line span. Dial fixes the splice position, keeps the rest of the proposal locally and uses the existing validator and transaction. The public `EditRepair` callback contract is unchanged. See [editing](tools.md) and `crates/dial-agent/src/agent/run/edit_runtime.rs`.

**Isolation:** for separated line edits, try undoing each one in the full proposal in memory. Use a span only when exactly one undo parses. The line diff is limited to 1 MiB per side and a 100 ms deadline; trial parses are limited to eight hunks and 4 MiB total trial source. The built-in helper will not accept the original hunk as its fix. This is not an exhaustive search for interacting subsets or a proof of intent. The 64 KiB disclosure cap is a ceiling, not a measured optimum.

**Next experiment:** compare ordinary retry, the old whole-file protocol and scoped repair on synthetic fixtures, including distant parser diagnostics and already-broken files. Score checked behavior, lost requested edits, bytes sent, calls and wall time. Do not collect user source. Keep the feature opt-in pending those results.

### Deterministic repair: offline paired replay

`cargo test --locked -p dial-agent --lib native_edit_with_and_without_local_repair` runs eight fixed proposals through native `read` and `edit` in fresh workspaces with syntax blocking. It compares full file bytes, not just parser success. The three dropped-closer cases (Python call, JavaScript array, Python dictionary) landed the reference edit in one attempt with local repair; without repair they required a second, explicitly correct edit. Both settings refused the five negative cases without writing. A callback that panics if invoked guards both arms. This establishes a saved **tool attempt on those fixtures**, not a measured natural recovery rate, latency or model-quality gain. The mechanism is now the first and only automatic repair unless the operator separately enables the coach; the global switch remains opt-in.

**Refusal-only promotion decision: keep the parser optimization; do not enable automatic insertion by default.** Syntax checking off incurs zero parses at the native edit validation boundary. A valid guarded proposal uses one parse with either setting. A successful local restoration uses three: proposed, prior, corrected. No partial-stream validation, persistent cache, model request or new tool schema is involved.

The ordinary-edit timing experiment used native read + replace in fresh temporary workspaces on one macOS development build. It timed only `edit`, alternated arm order, discarded ten warm-up pairs per size, and checked exact file bytes and identical tool text in all 400 measured pairs. Values in microseconds:

| Python fixture | Off median | On median | Off p95 | On p95 |
|---|---:|---:|---:|---:|
| 1 line | 732 | 711 | 1191 | 1161 |
| 201 lines | 2205 | 2219 | 2960 | 2913 |

These timings do not establish zero overhead or a general speedup. More importantly, a second replay checked six intended edits, all refused before writing with repair off. With repair on: **one exact file, two wrong files that parsed, three unchanged refusals**. Two scenarios intentionally share identical input: given `abs(a-b)` and proposed `abs(a+b`, the intended edit could be `abs(a+b)` or `abs(a)+b`. Restoring the original grouping cannot satisfy both. The three abstentions used the historical drill and existing regression fixtures. This is a counterexample to safe default promotion, not a production error-rate estimate; no private session corpus or live model was used.

Reproduce: `cargo test --locked -p dial-tools --lib paired_ordinary_edit_latency -- --ignored --nocapture` and `cargo test --locked -p dial-tools --lib local_repair_intent_replay -- --ignored --nocapture`. The latter reports wrong writes separately; successful execution of the experiment does not mean its promotion criterion passed.

### Live paired pilot: coach repair (historical)

This pilot predates deterministic delimiter repair: the switch below selected the coach. Its injected edit introduced an opening parenthesis where no group previously existed; it is **not** evidence for restoring an existing delimiter.

The [24 trial records](measurements/edit-repair-pilot.csv) are from the same development binary (SHA-256 `84c3661d3507c4104efbad46d93092809e1af8378c01329aff6ee953028da03a`). Dial's native benchmark ran a fresh copy of one synthetic Python fixture per case, with a 32,768-token context budget and a 90-second case limit. The grader required native `edit` use and checked `add` on positive, negative, zero and fractional arguments; it rejected the original and accepted the reference fix. The two repetitions reversed arm order. Models ran concurrently with one another, not concurrently across arms of the same model. The local server initially had no model loaded, so its first baseline time includes possible cold-start effects.

Every arm passed **2/2**. Times below are per-run elapsed seconds (median of two); request counts include both runs and all purposes. `off → on` changes only `edit.auto_repair.enabled` for the first four rows, and only advisory shell hints for the last two (repair off).

| Task | Model | Seconds, off → on | Requests, off → on | Coach calls, off → on |
|---|---|---:|---:|---:|
| Deliberately missing `)` in first edit | ChatGPT `gpt-6.2-astra` | 18.4 → 21.4 | 14 → 14 | 0 → 2 |
| Same injected error | LM Studio `google/gemma-4-e4b` | 52.1 → 43.4 | 14 → 15 | 0 → 2 |
| Ordinary arithmetic fix | ChatGPT | 16.4 → 17.0 | 12 → 13 | 0 → 0 |
| Ordinary arithmetic fix | LM Studio | 36.1 → 34.9 | 14 → 15 | 0 → 0 |
| Start with `cat calc.py`, then fix | ChatGPT | 23.8 → 17.4 | 14 → 13 | 0 → 0 |
| Start with `cat calc.py`, then fix | LM Studio | 25.3 → 21.5 | 11 → 10 | 0 → 0 |

Repair saved elapsed time in both local injected-error pairs, but added time in both hosted pairs; neither model gained a checked success. The coach never ran on the ordinary case, so its timing differences cannot establish repair speedup. Hints did not hurt checked success here; deliberately starting with `cat` does not represent normal shell use. No default changed. The hosted route reported cached input; the local route did not report cache coverage, and neither supplied invoice cost. The CSV leaves these unknowns empty. This was not a cache-on/off comparison or a test of the old whole-file protocol. Two repetitions on one file do not estimate a meaningful tail latency, a natural syntax-failure rate, or a population effect. Before promotion, test a broader corpus with held-out behavior checks, multiple languages and nontrivial sibling edits, including cases where ordinary retry actually fails.

## 2. Measure optional advice before changing defaults

Compare `dial` with `ablate-bash_hints` in the [assistance ablation matrix](benchmarks.md#assistance-ablation). They differ in one requested setting; in block mode they intentionally do not differ in behavior. Check the effective settings. The `raw` row changes several things and cannot isolate the cost of shell hints.

Run paired, interleaved trials on a pinned hosted model and a pinned local model separately, with matching fixture revisions, context budgets, verification and repetitions. Report checked successes over all trials, advisory bytes, redundant calls, cost per checked success and median/p95 latency. Record missing usage as unknown. If results differ by model family, keep the useful local behavior; if inconclusive, keep the default. Only after that first comparison, test emitting repeated advice once per actionable condition rather than once per result. Never deduplicate a denial, an error receipt, user steering or a required verification result.

## 3. Make prefix changes explainable before retaining more data

Dial already separates frozen/stable system bands from dynamic guidance, tracks the final post-hook request, and observes transformed provider requests. These are not the same as memoized serialization or paid cache hits. See [stable prompts](llm-transport.md#stable-prompts-and-immutable-media) and [request preparation](agent-loop.md#request-preparation-and-tool-concurrency).

One route already sends an actual suffix: `llm/codex_ws.rs` validates append continuity against the prior request and native response items. `chained` now copies options and the suffix without deep-copying the discarded history. The socket loopback test checks the actual next `response.create` frame. This is not a generic serialization cache: validating the full transcript and reconstructing the logical request still cost work.

Replay a long append-only session and then change one input at a time: tool withdrawal, hook rewrite, model/route switch, compaction, branch/resume. Inspect the first differing wire component, request-preparation time and stable bytes. Confirm billing effects separately on a provider that reports cached input. If unchanged history serialization is costly, cache only an immutable segment at its owning provider encoder, keyed by all encoding inputs. Bypass reuse for mutable/unclassified transformations; compare cached and uncached wire requests for equality, including after invalidation. Never retain a withdrawn tool or stale policy to buy a cache hit. Reject the optimization if the speedup is immaterial or retention outweighs it.

## 4. Simplify the loop along a real debugging path

For a normal foreground request, start at `run/turn/step/prepare.rs`, then `generate.rs` → `Agent::provider_request_with`; for a worker, `workers/child/turn.rs` → `child_provider_request`. Both now delegate to `run/dispatch.rs::prepare_provider_request`, which checks recorded contracts against **callable** tools, then applies request hooks. Notices return to the owning UI or child observer. Sending a definition for caching does not make that tool callable. Next trace a failed edit through retry and settlement before extracting another decision; preserve event ordering, cancellation and approvals rather than targeting a line count.

## 5. Improve extension iteration without granting ambient authority

The Python SDK fixtures now exercise a pre-tool refusal and an aliased `before_compact` note through the Rust hook runner; the engine attributes the latter as untrusted data. Invalid timeout declarations fail during registration. These are existing operator-installed subprocess contracts, not sandboxed plugins. Next cover hostile/malformed replies and version skew at this boundary. Do not install code from workspace discovery or auto-update executables. Honor the [integration pause](../CONTRIBUTING.md#new-integrations-and-tools-are-paused).

## 6. Prove reuse with one independent consumer

The [standalone client check](client-contracts.md#compatibility-policy) links `dial-client` and its `dial-protocol` dependency, not engine crates. It negotiates with the supplied executable and checks both a successful read and a coded refusal. Run it against each supported release before claiming cross-release compatibility. Extract another library only for a demonstrated second consumer that needs neither the agent loop nor its private state; the root facade is not a separately versioned SDK.

**Rollout gate:** baseline first; land one change at a time. Keep a result table by model and task class with trial counts and uncertainty. Promote a default only on repeatable benefit without worse checked outcomes or a safety failure. If the evidence is weak, retain the previous behavior and record what would distinguish the alternatives. No live-model improvement is established by this plan or the offline repair tests.

### Reproduce the offline evidence

From the repository root, these checks exercise the individual boundaries without sending a model prompt:

```sh
cargo test --locked -p dial-agent --lib edit_runtime::tests
cargo test --locked -p dial-agent --lib agent::run::dispatch::
cargo test --locked -p dial-llm --lib codex_ws::
cargo test --locked -p dial --bin dial cmd::benchmark::tests
python3.12 -m unittest discover -s sdk/python/tests -q
```

Build the executable with `cargo build --locked --bin dial`, then run the [independent client](client-contracts.md#compatibility-policy) against it. A passing run checks only that executable. The WebSocket tests use a loopback server; the edit tests use supplied repair responses. Neither measures a provider's cache-hit rate or a model's ability to preserve the requested change.
