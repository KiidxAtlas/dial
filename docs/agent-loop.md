# Agent loop and context maintenance

The agent re-evaluates context pressure at request boundaries using the larger of the serialized-request estimate and the calibrated provider-token estimate. Successful compaction returns to the loop and the replacement request is measured again; it is not limited to one proactive compaction per user turn. Failed attempts are associated with the unchanged request snapshot so the same dead end is not retried until history changes.

Overflow recovery uses a fit-only compaction target: the request only needs to fit the model's context window and output reserve, even when the normal auto-continue threshold would pause the turn. Compaction preserves complete assistant/tool groups while tolerating orphan or duplicated tool-call records in irregular imported history.

Portable summaries update the previous committed summary with newly summarized history instead of summarizing the summary as transcript. A cut within a user turn gets a distinct turn-prefix summary, while the recent suffix remains verbatim. Read and modified file lists carry forward between compactions. Summarizer input is divided at message boundaries, conservatively discounted against the selected summarizer window, and a rejected window is halved and replanned. Single messages too large for a window are clamped with an explicit omission marker.

Parent and child conversations share reply recovery in `agent/turn_recovery.rs`. Empty answers and action-only stops get at most three continuation attempts, and malformed or mixed stalled replies share an eight-attempt bound. Productive work remains unlimited when `max_steps = 0`. Exhausted recovery leaves work incomplete, preserves completed effects, and never records completion. Quotes, code examples and optional future work do not restart a finished report; no classifier request is required. A completed `finish` must contain a report rather than a promise to act.

Background worker findings arrive once in the parent's next request with status, a bounded report and a reference to the complete artifact. Typed assignments, rule state and result data remain in the execution journal rather than being repeated in the model-facing summary. Use the delivered findings before repeating a worker's investigation. Todos are optional progress bookkeeping. `done` needs no finding/evidence payload and does not run a verification round. Accepted completion reconciles pending progress; explicit blockers and running work remain completion prerequisites. Plain final replies and explicit `finish` share these prerequisites and the final configured verification/repair pipeline. A failed check cannot become a completed turn, and unfinished replies share bounded reply recovery rather than a separate todo-reminder budget. Routine inspection does not force a separate todo-only request; the configured reasoning-recovery plan gate still applies when it is needed.

Session history is append-only; tree navigation selects an abandoned branch without deleting it, and branch summaries are optional context for navigation.

Duplicate recovery withholds only the exact repeated call, including reads. Different ranges and files remain available, changed source identities clear the affected restriction, and new user steering resets the turn's recovery state. A withheld call is reported explicitly as unexecuted; its cached observation still contributes to the shared repetition bound rather than being misclassified as a new execution failure.

Explicit evaluation/learning runs capture the actual inference request and target. Normal coding launches disable trajectory and recovery-sample recording, learned candidates, automatic Laya/learned-skill capture, project-learning updates and startup evaluations without rewriting saved learning preferences. When exporting a verified example to the training template, valid tool-result error flags are removed as execution metadata while the evidence text is preserved. Malformed flags and unknown fields remain rejectable. An HTTP 200 response to a WebSocket upgrade establishes SSE-only transport for that session, so an unsupported upgrade is not retried at every request.

Stop hooks can resume a completed candidate within their existing three-round bound. Every resumed candidate returns through focused verification before another stop-hook decision; hook resumptions do not reset the repair allowance or turn step budget. Persistent blocking ends with a blocked outcome, and a hook cannot restart a turn that needs user input, has failed, has unknown effects, or was interrupted. Rejected completion reports keep the candidate work summary separately labelled beneath the final verification or hook diagnostic, so callers retain useful work details without mistaking them for accepted completion.

## Evidence-driven verification and escalation

Focused checks distinguish application failures from unavailable test infrastructure.
Recognized missing selected toolchains and offline Cargo dependency-cache failures are
environment-unavailable, never passing checks; compiler errors, assertions and missing
application imports remain code failures. A command that executes zero tests supplies
no passing behavioral evidence, including in the persisted behavior-check cache.
Round settlement retains verification statuses and bounded receipt history.

Automatic coach consultations retain their failure streak, evidence and attempt cap
through final-verification repair reentries in the same user turn. A new user turn
resets that allowance. Attempts count before authorization/routing, so denied or failed
consultations cannot repeatedly bypass the cap. Existing approval policy still applies.

Semantic evidence retrieval limits contributions from any single file before global
candidate truncation. This keeps smaller candidate files eligible for semantic ranking;
it remains bounded retrieval, not an exhaustive or resolved call-graph guarantee.

## Shared execution ownership

`agent/engine.rs` owns the lifecycle driver used by parent and worker adapters, result settlement, output-length recovery, repetition checkpoints, unchanged-read reuse, narration progress, reply recovery, committed-content edit-loop steering, and the uncertain-effect stop boundary. `tools/execution.rs` owns native dispatch, pre-start cancellation, panic containment, and callback effect propagation. A callback tool cannot hide an uncertain nested result by catching its error or returning success; subsequent callbacks are refused.

`agent/turn.rs` is the parent adapter for durable session state, live steering and learning policy. `agent/child.rs` is the worker adapter for private history, assignment permissions and worker request budgets. Both adapters advance the same lifecycle driver and share inference transport, authorization helpers and the parallel scheduler. Their preparation and persistence callbacks retain explicit role responsibilities. New common execution policy belongs in the shared engine, not copied into both adapters. `agent/tool_runner.rs` handles foreground UI events and routes callbacks back to their owner.

Read-batch admission applies recovery restrictions to every candidate, not just the first call.
The presence of pre-tool hooks conservatively routes calls through serial admission so rewritten
arguments can be validated and authorized before execution; hook-free reads remain parallel, and post-tool-only
hooks do not disable concurrency. Parent and worker hook rewrites both recheck their executable
schema and path authority. Mutable authorization and recovery decisions are never cached for
an entire turn.

Unknown effects end the current turn as incomplete after saving results and skipping unstarted actions. They bypass automatic stall recovery, context transitions and memory maintenance. A refusal before execution records no effects. A timeout or interruption after execution starts does not imply rollback. Recovery from a missing durable tool result records an explicit unknown effect and never replays the call. Productive `max_steps = 0` remains unlimited.

Successful edits can also loop: a model may keep inserting and removing the same lines, and each success used to clear repetition checkpoints. `agent/mutation_cycles.rs` hashes the actual committed content of each path the transaction owner reports in `Execution::changed_paths`, resolved and canonicalized through the workspace (including added roots). It never parses tool text or diffs. One return to an earlier version is an allowed revert. A version committed for the third time, with other versions in between, is an edit loop. An edit loop never ends the turn. The edit's result tells the model that its edits are undoing each other. The exact calls (normalized arguments) that produced the alternating versions are refused for the rest of the turn. Different edits, the same file through other arguments, other files and every other tool remain available, and the turn continues to completion. A mutation that only returns a file to content it already had is not progress, so it cannot clear other recovery restrictions. Consecutive identical versions count as one visit. Reads, todos and other files do not clear this history; only new steering resets it. History is bounded to 64 files, 16 recent versions per file and 64 refused calls, so novel edits remain unlimited. Uncertain effects keep their stop. Deleted, unreadable or files over 16 MiB are not tracked.

## Reviewed gaps

The gaps reviewed for this work and their implementation owners are:

| Area | Dial owner and boundary |
| --- | --- |
| Historical image lifetime | `storage/media.rs` retains verified bytes; `llm/images.rs` verifies and projects them. |
| Parent/worker consistency | Shared `agent/engine.rs`, `tools/execution.rs`, authorization, scheduler and inference transport; explicit role adapters. |
| Uncertain effects | `runtime/execution.rs` owns typed facts; tools, session repair and worker resume apply them. |
| Concurrent edits | `tools/document_transaction.rs` coordinates cooperating writers and rebases disjoint ordinary text proposals. |
| Prompt stability | `llm/prompt.rs` bands and `llm/wire.rs` final outbound payload fingerprints. |
| Historical tool contracts | `tools/contracts.rs` persists schema identity and tool revisions and requires explicit compatibility. |
| Lifecycle regression gate | CI runs `examples/benchmarks/interactive_smoke.py` against the built binary and a local scripted provider. |

The terminal gate exercises real PTYs, resize, cancellation, busy-turn commands, file review, retained image pixels after deletion, and SIGKILL/reopen without replaying a completed write. These fixtures establish behavior, not real-model quality, cache savings, or relative harness performance. Provider-backed performance measurements belong in the efficiency benchmark described in [benchmarks.md](benchmarks.md#measuring-efficiency).

Execution metadata stays separate from tool payload and display text. Document transactions report actual committed paths; progress guards never parse diff prose. Presentation hooks cannot clear execution failures or effect uncertainty. Configured subprocess tools share cancellation and started-process uncertainty handling.

## Coding workflow

Dial's default system guidance is Dial's own design: engineering principles, tool policy, scope/research/implementation/verification, and delivery.
It reuses current evidence, groups coherent edits, discourages repeated baseline checks and
routine narration, and verifies after the change rather than after each micro-edit. Dial retains
its own approvals and untrusted-context boundaries.
Custom operator guidance, fixed-prompt overrides and appended prompts remain supported.
Prompt assembly tests and scripted harness checks do not establish reduced looping in a model;
use the [efficiency benchmark](benchmarks.md#measuring-efficiency) for live-model comparisons. Rebuild/restart
Dial to use source changes; existing custom/admitted operator guidance is not overwritten.

## Focused verification, completion and tool output

After edits, dial derives focused Rust and Python syntax checks from checkpointed changed paths.
Python checks find the nearest owning Python metadata/environment boundary within the workspace,
including nested projects, and run source paths and related tests relative to that root.
`uv.lock` selects `uv run --offline --no-sync python`; an existing `.venv/bin/python` or Poetry
configuration selects that project's runner. Root uv workspace locks can supply a shared runner.
An operator can explicitly override argv with `[verification].python_command`, for example
`python_command = ["/absolute/project/.venv/bin/python"]`; this is argv, not an interpolated shell
snippet. There is no interpreter fallback after a selected runner fails. Setup failures stay
unverified until the environment is corrected and checks pass; installs/network retain approval.
Control state exposes `verification_results` with per-check `passed`, `code_failed`,
`environment_unavailable`, `permission_denied`, `cancelled`, `timed_out`, or `unverified` status,
along with the exact command/runner. Cancelled or empty checks never count as passing checks.
`[verification].commands` can add operator-owned sandboxed commands (name, command, optional file
extensions and timeout). Only confirmed `code_failed` diagnostics enter automatic bounded repair;
environment, permission, cancellation, timeout and unverified failures remain visible without
asking the model to edit code to fix missing prerequisites. Mixed runs send only actionable code
diagnostics to repair. Initial work, verification repair and stop-hook continuations share one
finite `max_steps` budget; zero remains unlimited. The edited tree and final diagnostics remain
visible for `/undo` or manual repair. Behavioral credit and final verification use the same
observed-process verdict; printed success without complete process evidence earns no credit.
Executable repair records use that same classified, typed execution evidence, not printed exit
markers. Unverified runs invalidate a matching failure baseline rather than pairing an old failure
with a later pass. Related-test planning reuses JS package/runner metadata and Go directory scans
only within one planning invocation; later invocations observe changed manifests and tests.
Behavioral provenance still hashes fresh bounded source state, but no longer loads dependency
outlines that its record does not use. See [Core harness review](harness-core-review.md) for the
comparison, rationale, and measurement limits.
Approval-denied writes are not treated as successful verification. `todo` is optional progress bookkeeping, not a verification checkpoint. `done` closes the current item and starts the next; findings and references are optional notes, including for legacy investigation entries. No separate tool-activity, citation or text-delivery gate applies. Accepted completion reconciles pending progress entries, while explicit blockers and running workers remain completion prerequisites. Failed checks remain failures independently of todo state.

Plain final replies and `finish` use the same completion prerequisites and final configured verification/repair pipeline. Empty replies, action-only promises and explicit admissions of unfinished work share bounded parent/child reply recovery. Invalid todo arguments still receive a simple corrected call and bounded recovery; there is no separate budget of todo-closing reminder turns. Productive work remains unlimited with `max_steps = 0`.

Default guidance starts with focused source inspection and moves to implementation once the path is known. It does not force a plan after an arbitrary number of reads. The optional reasoning-recovery plan gate still applies when explicitly enabled. Progress lists help substantial multi-step work; concise investigations and informational answers need no checklist. Optional source notes survive resume, and ordinary coding does not automatically harvest them into project learning. Identical successful observations remain subject to the shared read/investigation guards; changed sources and new user steering permit fresh observations.

Mid-turn requests end with a never-stored reminder of the current todo step, next step and recent findings.
Because it sits after the newest tool result, every earlier byte stays identical between requests and a
prefix (KV) cache only needs to process the appended reminder. Tool results are clipped to the
context-dependent per-model budget (4–48 KiB); optional `efficiency.tool_preview_bytes` further
reduces successful retrievable inspections. Errors and mutation results retain their diagnostics.
Clipping keeps both ends plus error, panic, failed-test and traceback lines from the omitted middle;
full output stays in artifacts. `bash` also bounds the output it holds in memory.
Each successfully prepared main-agent request journals `request_prep.elapsed_ms`: wall time from
the request boundary through history projection, contract checks, hooks and prefix observation,
including synchronous maintenance or waits along that path. It excludes the ensuing primary model
inference; it is not a pure CPU-time measurement or a timing of child requests.

Command execution and output storage are reported separately. If artifact writing/finalization fails,
`bash` retains its inline diagnostics and actual exit status, reports a capture error, and never
advertises a failed artifact as complete. `bg` continues capturing bounded live output when artifact
storage fails instead of stopping an otherwise healthy process. Cleanup drains output for up to
300 ms; output whose EOF was not reached is explicitly labeled partial.

## Shared context economy (all providers)

Dial's shared agent loop applies these controls to cloud and local models without changing output
limits, reasoning budgets, approvals or verification requirements:

- The default tool offer is the core roster plus the essential integrations when configured
  (`ask` when enabled, `find`, `lsp`, `bg`, `git`, `web_fetch`, `web_search`). `task` and `skill` are
  offered with them, and `message` is offered whenever `task` is, so peer messaging does not appear or
  disappear as workers start and stop. Overlapping specialists are deferred
  (`tools::DEFERRED_BY_DEFAULT`: `memory_update`, which needs an id and revision from `recall`, and
  `ast_search`, the structural variant of `grep`/`find`). Every catalog tool not offered is
  listed in the system prompt under "Loadable tools" with a one-line description, so the model
  loads it with `tool_load` in one round; schemas and recorded tool contracts are unchanged. The
  list changes only when a tool is loaded or unloaded, and the offer never reads live-worker state,
  so the provider's cached prefix survives ordinary rounds.
  MCP tools are the one exception: a server with at most 6 tools is offered up front, whole
  servers only, in server-name order, up to 16 MCP tools in total. Larger servers stay loadable.
  Discovery, lean-local, pinned (`[harness] tools`), hermetic and selection-only offers skip this.
  `assistance.compact_tools=true` switches to discovery mode for small local models: a core set
  bounded by `assistance.initial_tool_limit` (default 16) plus `tool_search`/`tool_load`.
  `assistance.lean_local` defaults to `auto`, which enables a lean profile when the main model is
  served locally (no endpoint, or a loopback or LAN host) and its planned context window is at most
  32,768 tokens. TOML `true` forces it on for every local route and `false` turns it off; hosted
  routes and hermetic runs are unchanged.
  When active, it turns on discovery mode, caps the initial tool budget at 8 (an unlimited
  0 also becomes 8), and omits the visual-verification guidance unless `browser` or `computer` is
  offered and the skills catalog unless `skill` is offered. The resulting prompt stays
  byte-stable across turns. Measure it with `dial benchmark --ablate-assistance`'s `lean` row
  before relying on it.
  Automatic search and compatibility activation retain at most eight discovered tools, oldest
  activation evicted first. Explicit `tool_load` names are pins and are never evicted by discovery;
  unloading clears both sets. Additive `discovered_tools` control state survives resume, and older
  `enabled_tools` state remains pinned for compatibility. A pinned `[harness] tools` list or a
  model-profile allowlist still restricts the offer; discovery never grants execution permission.
- Local model profile (`[assistance.local]`) applies only to locally served main models (the same
  routes as `lean_local`). Hosted routes and hermetic runs are unchanged, and explicit pins win
  over every local-profile policy below. Its keys are `enabled` (default `true`), `small_window`
  (default 16384; 0 disables the small tier, otherwise 1024..=1048576), `edit_mode`, `sampling`,
  `reasoning_share` (default 50), `escalate_after` (default 2), and the keys below.
  - Tiers: a lean local route whose planned window is at most `small_window` gets the small tier:
    a fixed minimal tool set (`read`, `write`, `edit`, `bash`, `grep`, `glob`, `todo`, `finish`,
    `tool_search`, `tool_load`) plus loaded and memory tools; everything else loads on demand. A
    pinned `[harness] tools` list keeps its own offer. A lean route above that window gets the lean
    tier (the `lean_local` profile above). Other routes get the full tier.
  - Edit mode: local routes default to the `replace` grammar. Precedence is the `--edit-mode`
    override, the model profile `edit_mode`, `[harness] edit_mode`, `[edit] model_variants`, then
    `assistance.local.edit_mode`, then `[edit] mode`. `inherit` makes the local default follow
    `[edit] mode`. Hermetic evaluation ignores the local default.
  - Sampling: `model` (default) leaves sampling to the model. Dial's own runtime (no endpoint)
    applies the model's shipped `temperature`, `top_p`, `top_k` and `min_p` from its
    `generation_config.json` (a `do_sample: false` file means greedy, so nothing is applied) or GGUF
    `general.sampling.*` metadata, and otherwise keeps the harness temperature. On local servers Dial
    omits `temperature`, so the server's per-model defaults apply. `harness` sends Dial's harness
    temperature instead. A temperature counts as pinned, and Dial keeps sending its own, when
    `[harness] temperature` differs from the default 0.2 (including a learned harness value), when a
    model profile sets a temperature or `thinking = true`, or when adaptive sampling or
    `adaptive.pin_sampling` is enabled.
  - Reasoning share: on lean tiers, streamed reasoning per request is capped at `reasoning_share`
    percent of the planned window, at about 4 bytes per token. The cap applies only when
    `reasoning_guard.max_reasoning_bytes` is 0 (unlimited) or larger than that cap; a stricter
    explicit limit still applies. When the cap is hit, the existing reasoning guard discards that
    generation and retries once; requests to Dial's own runtime also carry the `dial_reasoning`
    budget (3/4 of the cap). 0 disables it.
  - Escalation: `escalate_after` consecutive failing rounds (malformed tool calls, or failed `edit`
    or `write` calls) step the session up. Step 1 switches to the minimal tool set and lean prompt;
    step 2 switches to the `replace` edit grammar unless a pin chooses otherwise (it is skipped when
    the grammar is pinned or already `replace`). A step takes effect from the next request, so the
    prompt and tool offer change at most twice per session. The reached step is remembered per model
    in the user directory file `local-profile.json` and applied at the first turn of the next
    session. 0 disables escalation.
  - Benchmark lever: `ablate-local_profile` in `dial benchmark --ablate-assistance` turns the
    profile off (`assistance.local.enabled = false`) against the `dial` row.
  - Thinking (`assistance.local.thinking`, default `phase`): where the server exposes a thinking
    switch for the model (llama.cpp and Dial's MLX runtime: a chat template that reads
    `enable_thinking`, sent as `chat_template_kwargs.enable_thinking`; Ollama: a model with the
    `thinking` capability, sent as `think`), reasoning is on for a turn's first request, after a
    failed round, an intercepted generation or a successful `todo` update, and off after a
    successful tool round. LM Studio documents no request-level switch, so nothing is sent there.
    `model` leaves thinking alone. An explicit model-profile `thinking`, `reasoning_effort`,
    `thinking_budget` or `chat_template_kwargs.enable_thinking` always wins.
  - Model residency (`keep_loaded_secs`, default 1800, 0 = server default, max 86400): asks the server
    to keep the model loaded this long after each request (Ollama `keep_alive`, LM Studio `ttl`,
    which LM Studio applies to just-in-time loaded models). An explicit model-profile `keep_alive`
    wins. On Ollama, a model that was not loaded when the session started is unloaded when Dial
    exits (`keep_alive: 0`); a model that was already loaded is left alone.
  - Prompt cache (`prompt_cache`, default on, llama.cpp only): requests send `cache_prompt` and an
    `id_slot`. The main session uses slot 0. When the server reports more than one slot (`/props`
    `total_slots`), side requests on the main model use the last slot and workers use the others,
    so neither evicts the main prefix. Prompt tokens served from the cache are counted in the
    evidence and shown in the status line (`N cached`) while a prompt is processed.
  - Side models (`side_models`, default `warn`): a side role (`small`, `coach`) assigned to a
    different model on the main model's local server makes the server swap models for summaries,
    ranking and edit repair. `warn` keeps the assignment and says so once per session, with the
    measured load time when known. `reuse_main` sends those side requests to the loaded main model
    (thinking off where switchable). `allow` keeps the assignment without a notice.
  - Context fit (`context_advice`, default on): when a local server serves a smaller window than
    configured, one notice says so and how to raise it on that server. On Ollama without a pinned
    `context_window`, `num_ctx` is limited to the window the model's memory estimate fits on this
    machine, so the model is not offloaded or swapped, and a notice says so once.
  - Loop recovery (`loop_penalty`, default on): when the repetition detector stops a local
    generation, the retry carries a presence penalty of 0.5 and a repetition penalty of 1.1 (thinking
    off where switchable) for that retry only. Penalty values set in the model profile's
    `local_sampling` are kept.
  - Evidence and suggestions (`suggestions`, default on): per-model counters (failing rounds, edit
    outcomes per grammar, measured decode and prefill speed, cache-hit tokens, loads, side-model
    swaps, repetition loops, context overflows) are kept in `local-profile.json`, keyed by provider
    and model. When the evidence supports a change, `/model settings` lists it under Suggestions with
    its evidence. Enter applies that one change; nothing is applied silently. A speculative-decoding
    draft model is suggested only for MLX models with a smaller installed model whose tokenizer is
    identical. Once one is applied, decode speed with it is recorded against the earlier baseline;
    after 20 samples each, clearing it is suggested unless it is at least 5% faster. With
    `suggestions` off only the escalation step is recorded.
  - Time budget (`time_budget`, default on): when `reasoning_guard.max_pre_action_secs` is 0 and at
    least three decode-speed samples exist, the pre-action time limit for a request is derived from
    the measured prefill and decode speed (1.5x margin plus a minute, within 2 to 60 minutes). While
    a prompt is processed, the status line shows the time left (`~Ns left`) from the measured
    processing rate.
  Edit these keys in `/improve settings` under Local model assistance.
- Ambient codebase retrieval is off by default (`codebase.ambient_context`); the model calls
  `codebase_memory` when it wants the saved understanding. When on, it selects task-relevant
  sources/findings, prefers fresh evidence, omits the directory inventory and bounds stale notices.
- Tool results are shown whole up to the per-model limit (48 KiB for large windows, about an eighth
  of the window for small ones), with the complete output retained in artifacts.
  `efficiency.tool_preview_bytes` can reduce successful inspection results further; errors, read/edit
  snapshots, explicit artifact retrieval and unretained output are never reduced.
- Brief, non-complex tasks omit redundant tool-argument examples for the first few requests. An
  explicit plan, a tool error, debugging work or exhausting that allowance restores full guidance;
  todo enforcement and finish/verification gates do not change.
- Calm history remains byte-stable. Under context pressure, older large artifact-backed observations
  repeated successfully later can now be reduced in batches by default; failures, mutations,
  unrelated ranges and the protected recent suffix stay intact. Pressure begins no later than the
  effective automatic-compaction threshold, so this reduction can avoid an unnecessary summary.
  `compaction.supersede_reads` permits additional supersession and `compaction.drop_useless` permits
  broader old-observation pruning. Both pressure and tail protection use the effective capped
  planning window. Once projected, placeholders stay stable; archived evidence is never mutated.
  Compaction still uses configured token/percentage/model/agent policy with output/reserve headroom.
- Very large windows are planned against `efficiency.history_token_cap` (default 262144) rather
  than their full size, because every step resends the whole history. With `threshold_*` on `auto`,
  the trigger is 80% of the planning window after its reserve (15%, at least 16k): about 178k on a
  1M-token model instead of about 680k, and unchanged (about 136k) on a 200k model. The prefix
  stays byte-stable between compactions, so caching is unaffected; compaction still keeps
  `keep_recent_tokens` of recent history verbatim, a summary of up to 16k tokens, and the
  carried-forward read/modified file lists. Requests sent while a background summary is still
  running may exceed the cap up to the real window's safe budget. Set the cap to 0 (or
  `efficiency.enabled = false`) to plan against the full window; an explicit
  `compaction.threshold_tokens` / `threshold_percent` or model threshold is applied to the capped
  window.
- The transient task evidence packet is off by default (`efficiency.task_packet`); requests carry the
  conversation plus a one-line task focus while a todo is in progress.
- Request accounting measures serialized JSON bytes once for context estimates, overhead events and
  economy metrics, without allocating full JSON strings just to count their bytes. Owned projected
  messages are normalized in place; immutable session evidence and provider validation are unchanged.

```toml
[efficiency]
enabled = true
ambient_context_bytes = 8192    # per ambient source; 0 = unlimited
tool_preview_bytes = 0          # unlimited successful-result previews
lightweight_requests = 4         # 0 disables the lightweight contract
cache_pressure_percent = 75     # 50..79; below the normal compaction threshold
history_token_cap = 262144      # planning window for larger models; 0 = the model's full window
```

## Loop feedback and experimental workflows

Loop feedback recurs while narration continues without actual progress. Main and child recovery
release a withheld read when its supporting file changes externally, without invalidating unrelated
call feedback. Retrieval includes `source_sha256` so findings can be tied to the source version;
hashes and storage history are identity/freshness information, not proof that a finding is correct.

**Experimental workflows** are under `/settings` → **Experimental** and all default to off. Enable
each independently:

- `experimental.work_packet_contracts` exposes a `work_packet` on single or batched `task` jobs:
  define `scope`, `interface`, and 1–16 `acceptance` conditions. The child must report evidence
  for each condition; this is a handoff contract, not an automatic verifier.
- `experimental.verification_receipts` retains up to 20 execution receipts in session reliability
  state. Each records check names, redacted commands, statuses, and hashes for changed checkpoint
  files; it stores neither output nor source text. A passing receipt records execution status only,
  not correctness or complete coverage.
- `experimental.context_overlap_dedup` removes exact repeated paragraphs from earlier ambient
  instruction files when the same paragraph occurs later. Unique paragraphs, file labels, order,
  and the existing conservative containment behavior remain.
- `experimental.profile_matching` exposes `model_profile` on a `task` call. The delegated call
  uses that named profile for all jobs in the call; only profiles already configured by the
  operator are accepted. This is explicit routing, not automatic benchmarking or model selection.

## Human questions and investigation checkpoints

`ask` accepts an ordered `questions` array with unique `id`, `question`, optional `header`,
`options` (`label`, optional `description` and `preview`), `multi` and a zero-based
`recommended` index. The human can select options, provide custom input and notes, attach
explicit image paths, chat instead, or cancel. Previews are text; no clipboard or graphical
preview support is implied. CLI questions use the controlling terminal and fail explicitly
without one. Child questions go through the live owning controller, never an independent stdin.

```toml
[ask]
enabled = true
timeout_seconds = 0
notify = true

[checkpoint]
enabled = false
```

Zero question timeout waits for a real answer; plan mode disables timeout selection. RPC
advertises `ready.question_input` and emits a `question` event. Clients first send
`question_presented` with the exact `question_id`, `generation` and `session_id`; only then
does the configured deadline start. `question_answer` carries that same token, `kind`
(`answers`, `chat` or `cancel`) and ordered results with `id`, `selectedOptions`,
optional `customInput`, `note`, `customInputImages` and `noteImages`. Image arrays contain
explicit human-supplied file paths. Invalid, stale and replayed answers are rejected;
`question_closed` reports the outcome. ACP explicitly advertises no custom question-input
bridge. Authentic question/answer metadata is retained; `/tree` is session navigation,
not a promise of message-level re-answering.

Enable `[checkpoint] enabled = true` to offer paired `checkpoint` and `rewind` tools.
`checkpoint` accepts a nonempty investigation `goal` up to 8 KiB; activation occurs only
after its complete tool group is journaled. `rewind` accepts a nonempty `report` up to
64 KiB. Its immediate result requests a rewind; it does not claim that a branch commit
already happened. The owner verifies the checkpoint archive and current policy before
installing the report on the immutable conversation branch. This is context management,
not Git/file undo: completed effects, usage, todos, working state, notes and permissions
remain. New external input invalidates the investigation without discarding that input.
`new_context` and `finish` cannot silently bypass an active investigation.
Owner `session_search` with `query = ""`, `history = true`, `all_branches = true` pages
canonical archived evidence from that same session. Child archives are private to the
task lifetime and do not claim post-task persistence.
Explicit model tool profiles must permit `session_search` for checkpoint recovery; profiles
that exclude it do not enable an investigation whose evidence could not be recovered.

## Compaction policies

`/compact [soft|remote|snapcompact|handoff|shake] [focus]` selects a manual method;
unrecognized first words are focus instructions. Snapcompact and Shake reject focus text.
RPC `compact` accepts the closed `CompactionOptions` object and returns the actual outcome;
manual maintenance never silently resumes an old turn. Its `compacted` and `context_reset`
flags distinguish compaction from a committed notes-backed reset.
CLI overrides include `--compaction-strategy`, `--compaction-threshold-tokens` and
`--compaction-reserve-tokens`; the numeric flags accept positive integers or `auto` to clear
an inherited value. Omission follows persisted configuration.
All editor and compaction policy fields, including typed model/agent maps, are available through `/settings`.

Automatic method selection skips native Remote compaction when the route has no declared
compatible capability (unless a custom summary endpoint is configured). Explicit `/compact remote`
still reports an unsupported route honestly. Portable Soft/Handoff requests send an explicit empty
`tools` array to OpenAI-compatible servers. Foreground and background summaries use the `small`
role when assigned, otherwise the current model, including inside subagents. Internal summary
budgets never inject output-limit overrides into ChatGPT-plan requests; explicit unsupported
sampling or output-limit settings in a selected profile still produce a validation error.
Shake only elides recoverable tool evidence: if the
result still exceeds the safe request budget it tries the next configured method, never drops
conversation messages or commits an oversized replacement.
The compaction banner compares the previous and replacement **final projected requests** using
the same provider calibration. Automatic continuation requires a reduced request below the
compaction trigger and within the safe prompt budget, with `auto_continue` enabled and no
durability warning. The stricter recovery band still gates speculative results with live growth;
it does not pause an otherwise safe foreground continuation.

```toml
[compaction]
enabled = true
strategy = "context-full" # context-full | handoff | shake | snapcompact | off
method_order = ["remote", "soft", "shake", "snapcompact", "handoff"]
threshold_percent = "auto"
threshold_tokens = "auto"
reserve_tokens = "auto"
keep_recent_tokens = 20000
mid_turn_enabled = true
auto_continue = true
remote_enabled = true
remote_endpoint = "inherit"
remote_streaming_v2_enabled = true
v2_retained_message_budget = 64000
model_thresholds_enabled = true
async_enabled = true
idle_enabled = false
idle_threshold_tokens = 200000
idle_timeout_seconds = 300
handoff_save_to_disk = false
supersede_reads = false
drop_useless = false
experimental_context_management = false
summary_instructions = "inherit"

[compaction.model_thresholds]
# "provider/model" = "80%"   # exact or longest trailing-* selector
# "provider/model*" = "f120000" # fixed tokens; bare number is a scaled model-policy base
# "provider/model" = "inherit" # clear this exact lower-layer entry

[compaction.agent_threshold_overrides]
# "reviewer" = 90000        # exact named agent; bare numbers are fixed here

[compaction.native_models]
# "provider/model" = "responses-v2" # explicitly declared compatible endpoint/model
# Other values: responses-v1, responses-v1-v2, anthropic, none

[compaction.snapcompact]
shape = "auto" # actual Anthropic wire: 11on16-bw; other wire families: 8on22-bw
system_prompt = "none" # none | agents-md | all; protected instructions remain outside
tool_results = true

[compaction.snapcompact_models]
# "provider/model" = { kind = "fixed", tokens = 4096 }
# Alternatives: { kind = "pixel_ceiling" },
# { kind = "patches", patch_size = 32, tokens_per_patch = 1, base_tokens = 0 },
# { kind = "area", pixels_per_token = 750, base_tokens = 0 }
```

Native compaction requires an explicit compatible route declaration; protocol selection alone is
not evidence that an endpoint supports it. A custom summary endpoint resolves credentials for its
own URL. Image archives use actual bundled fonts, PNG dimensions and declared model billing;
unknown billing uses a conservative pixel ceiling and may legitimately fail to fit. All 17 font/
layout variants are selectable in settings. Characters a shape cannot draw (no bitmap or Silver ink,
including Arabic in every bundled font, controls and bidi/format marks) are rendered as exact JSON
`\uXXXX` escapes (surrogate pairs above U+FFFF), with literal backslashes doubled to distinguish
existing escapes. Decode that rendering layer before interpreting transcript JSON. The durable
source keeps the original characters; escaped text consumes normal cell and frame budget, and
overflow fails instead of truncating. Opaque native payloads and non-replayable image formats
still produce errors, not silent source loss.

Async maintenance owns a frozen snapshot and independent cancellation. Only the current owner
may claim a still-valid prefix; changed branches/models/configuration invalidate it. Accepted
provider usage remains charged even when the result is discarded. Idle maintenance is opt-in,
requires real inactivity without pending user/tool/child/background work, and does not restart
completed tasks. Local compute follows foreground lease ownership rather than duplicating a model.
Experimental context management exposes branch-owned `context_notes` and explicitly requested
`new_context`; notes are untrusted data, and a reset is acknowledged only after paired tool results,
protected-state budget validation and durable journal commit. Both tools must be available in the
active model/tool projection; disabling them does not silently substitute a summary or reset.
Reset outcomes identify `kind = "new_context"` with no compaction method and genuine archive
evidence. Child notebooks and archives are owned private task sessions, never the parent's session.

Handoffs commit within the same session. With `handoff_save_to_disk = true`, the authentic
summary also receives a private `artifact://` URI. Export errors and post-commit
`journal_warning` fields describe an already-applied operation, not a rollback or invitation
to retry. Notes-mode idle pressure produces guidance; it never silently resets context.

Bundled image-archive renderer/font notices are in `src/agent/snapcompact/fonts/` and `THIRD_PARTY_NOTICES`:
public-domain bitmap fonts, Unscii CC0 and Silver by Poppy Works/Wolfgang Wozniak under CC BY 4.0.

## Live task and phase policy

Dial now selects request controls between model steps, without a classifier model call, training,
or a compute lease of its own. `/policy` shows the last effective request settings, the reason for
selection and bounded recent request metrics; it also works while the agent is busy. Open
`/settings` → Model → Adaptive policy for controls. CLI inspection and previews load no model:

```sh
dial policy
dial policy "Investigate why the build fails" --json
```

A CLI preview uses the configured baseline; an active session may use a different evaluated harness
version. The live session report is the record of what that session requested. Capability-unverified
sampling is labelled unverified, rather than claiming the provider applied a temperature.

Initial phase comes from the task. Observed source reads select investigation, successful edits
select editing, recognized checks select verification, and errors or reasoning interceptions select
recovery. Design/alternatives requests start in exploration. Control calls do not clear recovery.
A recognized check is an observed execution event, not proof that the whole task is correct.
Later reads after an edit remain focused on implementation. New turns reset phase detection.
Pin a phase to override this heuristic. Settings saved during a running turn apply after that turn;
phase selection itself changes at each request boundary.

By default, phase-specific pre-action time limits are enabled; output-budget adaptation and **phase sampling are off**.
Explicit model-profile output/temperature/top-p/thinking/effort overrides win. Reasoning-model output
budgets are preserved, and supported unpinned reasoning effort can adapt only within the exact
model's declared capability list. Existing reasoning and retry guards remain authoritative.
Truncated output retries escape the phase token cap; partial tool calls still cannot execute.
Editing keeps the baseline output budget. Task evidence packet size can shrink for verification or
recovery, while cached source messages and tool permissions remain unchanged.

Enable Experimental phase sampling to try configurable temperature presets and top-p choices:

| Phase | Default temperature | Default top-p | Output limit |
|---|---:|---:|---|
| Investigation | 0.20 | 0.95 | min(baseline, 2048) |
| Editing | 0.15 | 0.90 | baseline |
| Verification | 0.05 | 0.80 | min(baseline, 1536) |
| Recovery | 0.10 | 0.90 | min(baseline, 2048); expanded during length recovery |
| Exploration | 0.50 | 1.00 | min(baseline, 4096) |

These are experimental operator-enabled presets, not measured optimal values. Unknown endpoint
capabilities keep their previous request behavior; MLX uses Dial's supported request controls.
Pin baseline sampling to preserve the baseline temperature/top-p regardless of phase. Explicit
model-profile sampler values remain pinned individually even when experimental sampling is enabled.
A one-request profile is consumed at the request boundary, including failed requests; retries keep
the same profile, and auxiliary calls cannot inherit it. No server restart or cache flush is needed
for these request controls. Activation steering remains a separate fixed server profile.

```toml
[adaptive]
enabled = true
sampling = false
reasoning = true
output_limits = true
pin_sampling = false
phase = "auto"
editing_temperature = 0.15
investigation_tokens = 2048
verification_tokens = 1536
```

Policy history stores settings and observed execution/token/latency metrics, not new training labels
or permission grants. It never promotes sampler settings from self-reported success. Existing task
routes between evaluated harness versions remain scoped to their admission records. Auxiliary roles
and sealed harness evaluations retain their existing request controls. To measure sampler changes,
compare pinned model-profile configurations on held-out coding tasks and check correctness, tool
validity, retries and latency; this release does not automatically tune and admit phase presets.

## Optional preparation

`experimental.tool_speculation` defaults to false. When enabled, the owner may prepare at most two
native `ast_search` reads of individual bounded local files later in an admitted batch. Actual calls
repeat schema, permission and rule admission; cached results require the same owner policy and the
hash of the bytes actually observed. Edits, processes, directory scans, callback tools and third-party
replacements are excluded. Batch teardown cancels unconsumed preparation. RPC and benchmark records
report preparation, reuse and discarded work separately from model calls. No live-model efficiency
improvement is implied by these counters.
