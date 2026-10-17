# Harness quality: evidence before optimization

Dial's agent harness (tool scheduling, context, recovery and verification) and its evaluation
harness (isolated fixtures, graders, reports) have different responsibilities. Faster execution is
useful only when it preserves the requested result and the safety boundaries. A shorter transcript,
a successful final sentence, or a scripted provider's output does not establish model quality.

For the source-level comparison with Codex, Pi, and Anthropic, current loop ownership, implemented
admission/verification optimizations, and deferred tradeoffs, see [Core harness review](harness-core-review.md).

## Research used for this design

The following primary sources were fetched successfully during this work:

- Anthropic, [Demystifying evals for AI agents](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
  (January 9, 2026): distinguish transcripts from final environment outcomes, use executable graders
  for coding tasks, separate capability from regression suites, and repeat trials to expose
  reliability rather than reporting only a best run.
- Anthropic, [Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
  (September 29, 2025): treat context as finite attention, expose a small understandable tool set,
  retrieve evidence just in time, and preserve critical details when clearing redundant outputs.
- Anthropic, [Effective harnesses for long-running agents](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)
  (November 26, 2025): preserve concrete progress across context boundaries, work incrementally,
  and verify real end-to-end behavior rather than trusting an agent's completion claim.

These are design evidence, not measurements of Dial. Their examples do not authorize automatic
commits, repeated baseline checks, additional agents, or browser/network access in a Dial session.
OpenAI's harness-engineering article was also located, but fetching its primary page returned
HTTP 403; no design claim here relies on its contents.

## Applying the findings to Dial

| Concern | Implementation/evaluation boundary |
| --- | --- |
| False success from textual diagnostics | One authoritative process-based verification status must drive both final verification and behavioral credit. |
| Repeated attempts at unavailable checks | Keep environment/permission/cancellation/timeout/unverified statuses distinct from code failures; repair only actionable code failures. |
| Serial independent inspection | Registered native read tools must declare their actual concurrency contract; synthetic scheduler tests alone are insufficient. |
| Idle worker slots | Refill a bounded remote read-only/isolated worker queue when a slot is released, without parallelizing in-place writers or local model lanes. |
| Redundant observations | Supersede only an older, artifact-backed observation actually covered by newer evidence, under context pressure; preserve errors and unrelated ranges. |
| Growing tool schemas | Distinguish automatic discovery's bounded working set from explicit pins, and retain a way to reload evicted tools. |
| Inconsistent context thresholds | Plan pruning and its protected recent suffix against the same effective, capped history budget. |

Do not replace these controls with a bigger prompt or an unconditional multi-agent architecture.
Independent substantial tasks can benefit from delegation; shared edits, provider capacity and
coordination overhead still constrain useful concurrency.

## The evaluation loop

1. **Protect mechanics offline.** Rust regressions exercise authoritative check status, repair
   eligibility/budgets, real tool classification, bounded worker refill, and context/tool-state
   boundaries. The scripted smoke drives the actual binary, tools, isolation and report parser.
2. **Measure task outcomes.** `quality.toml` checks application behavior and preservation boundaries
   with verifiers outside the agent's copied project. `efficiency.toml` measures ordinary workflows;
   `recovery.toml` covers diagnosed contract recovery and evidence reuse. The terminal smoke covers
   interruption/crash lifecycle separately. A passing scripted run is not a quality benchmark.
3. **Repeat on a controlled model.** The existing machine runner supports 1–10 fresh repetitions.
   Keep the fixture/source hashes and model configuration fixed when measuring a harness revision.
   Compare full configurations only when that is the intended experiment. Remote inference remains
   an explicit opt-in and may consume quota.
4. **Analyze reliability before speed.** `examples/benchmarks/reliability.py` validates complete
   repetition sets and matching task/source/verifier contracts. It exposes each task's success
   frequency and a Wilson interval, empirical all-trial and any-trial task fractions, unavailable
   checks, agent errors, repairs, tool usage and known tokens. Compare latency only on tasks that
   consistently succeed in both configurations. Do not discard failed/unavailable trials to make
   a configuration look reliable.
5. **Review outputs and traces.** Executable tests do not fully grade maintainability, relevance,
   explanation quality, overengineering or appropriate user interaction. Review sampled successes
   as well as failures against those criteria before changing defaults. Wilson intervals assume
   independent trials; small samples and correlated provider behavior limit conclusions.

See [Benchmarks and measurement](../benchmarks.md#repeated-outcomes-not-best-run-scores) for executable
commands. The repeated-report analyzer is offline and does not add a model judge or consume quota.

## What the harness deliberately does not claim

- Empirical all/any-trial fractions are not independent estimates of `pass^k`/`pass@k`.
- A lower prompt-token count is not proof of lower provider charges or better output.
- Missing token accounting is unknown, never zero cost.
- A failed check caused by missing infrastructure is not proof that application code is wrong.
- Passing the local regression suite is not evidence of a real-model quality or latency gain.

Broader changes such as default preview truncation, additional autonomous roles, or model-based
quality grading require their own measured ablations. They are not enabled by this work.

## Live-session audit: problems outside the original seven

The implementation session itself exposed useful failure cases. They must not be conflated with
evidence that a particular model is generally inefficient:

- **Agent mistakes:** guessed source paths caused failed reads; requesting discovery of tools
  already offered was unnecessary; overlapping Cargo checks waited on the same build lock. A
  regression assertion comparing entire large histories also generated a needlessly huge failure
  diagnostic. Narrow source lookups, one integration build owner, and focused content assertions
  address these workflow mistakes without weakening Dial's checks.
- **Opaque isolation failure:** three requested copies failed before workers started. Investigation
  identified an oversized generated `bin/dial`, not a proven nested-worktree cause. Isolation now
  reports the offending path, size, limit and that preflight copied nothing. Sparse-file regressions
  protect both initial oversize and later worker-file growth. Repeating the same snapshot per worker
  remains a separate potential cost; batch-wide caching needs a correct source-invalidation contract.
- **Semantic-search endpoint compatibility:** an auxiliary `find` request failed with HTTP 400,
  `tools: Input should be a valid array`. OpenAI-compatible tool-less requests now explicitly
  emit `tools: []`, and a loopback wire regression confirms that the array survives the same
  profile transformations as auxiliary ranking calls. Toolful requests retain their real tools.
  The original endpoint was not retried live; this establishes the wire contract, not a claim
  about that endpoint's current health. There is no silent lexical-success fallback.
- **Noisy change suggestions:** change context now ranks import/module identifier matches ahead
  of weaker lexical identifiers, ignores comment-only/common filename noise, and checks candidates
  against their stored digest and current index visibility before suggesting them. Deleted,
  modified, newly ignored and nested linked-worktree sources are withheld without waiting for a
  refresh. Only relevant fresh saved findings accompany these hints; an empty result stays empty.
  Suggestions remain explicitly unverified hints, not a resolved dependency graph.
- **Test-environment sensitivity:** Git-negative fixtures now prevent parent repository discovery,
  and checker fixtures establish their own repository boundary instead of inheriting the checkout's
  ignore rules. Root-marker failure tests use a marker guaranteed absent from the fixture's
  ancestors, preserving LSP's intentional enclosing-project behavior. Test idle-limit overrides
  are thread-local, with a concurrent-isolation regression; output-free noise streams outlast the
  deadline they test so stream closure cannot race that deadline. Production timeout limits remain
  unchanged. A green focused run still does not erase any failed full-suite result.
  Curriculum property checks also cap their nested worker pool rather than multiplying libtest's
  concurrency by the full CPU count for sandboxed interpreters. LoRA status reads explicitly
  release successful shared-lock probes, so a descriptor inherited by a concurrent child cannot
  keep the probe alive and block the next training phase; a duplicate-descriptor regression
  exercises that release boundary.
  The local browser fixture treats socket read timeouts as shutdown polls, not a deadline for
  preconnect or partial request headers. Its regression holds headers idle across polls, completes
  the real HTTP exchange, and checks that shutdown releases connections still waiting for input.
  GitHub PR fakes now inject executable/home values through scoped test inputs rather than changing
  process-wide `PATH`/`HOME`; a regression keeps an unrelated Git consumer running concurrently
  and verifies it still sees the real executable and environment. A writer-only mutex cannot make
  global environment mutation safe for the many other tests that read it without that mutex.

These observations justify regression cases and clearer diagnostics, not blanket retries, larger
prompts, hidden failures, speculative provider fallbacks, or more concurrent workers.
