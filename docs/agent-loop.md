# Agent loop and context maintenance

The agent re-evaluates context pressure at request boundaries using the larger of the serialized-request estimate and the calibrated provider-token estimate. Successful compaction returns to the loop and the replacement request is measured again; it is not limited to one proactive compaction per user turn. Failed attempts are associated with the unchanged request snapshot so the same dead end is not retried until history changes.

Failed blocking compaction applies the same growth-based retry floor to speculative launches, so
small tool results do not restart the failed method chain in the background. New user steering
clears that backoff. Successful background claims, overflow recovery, investigation rewinds and
context rollovers release duplicate-evidence restrictions just like blocking compaction: an
earlier result that left the active history cannot be required as visible evidence.

Context-economy decisions and request-overhead telemetry use the shared provider-neutral wire
estimate, including structured text, native replay and attached images while excluding internal
bookkeeping. Large structured/image suffixes therefore protect a warm prefix just like plain text.
Developer instructions count with system overhead, not conversation history. These estimates are
not billed token counts. Contradictory cache-write TTL buckets (including arithmetic overflow)
leave API cost unknown instead of producing a guessed or inflated charge.
When a request overflows even after ordinary maintenance, the existing lean-tool fallback also
uses the lean instruction variants. It does not keep paying for full workflow prose after reducing
the schemas; configured rules, project context, safety boundaries and explicitly loaded tools remain.
With efficiency enabled, interactive sessions reuse concise delivery and delegation guidance
on hosted models. Full tool policy and workflow rules remain in place. This reduces repeated
reporting prose without enabling the lean local tool profile: offered schemas, loadable-tool
descriptions, visual-verification guidance, operator instructions, custom templates and project
context are unchanged. Disabling efficiency restores the full built-in delivery/delegation prose.
For an unlimited foreground output setting, the model's advertised output ceiling is bounded by
the context remaining beside the current prompt; it is not reserved in full before admission.
Explicit output limits and the compaction policy's response reserve remain authoritative.

Both foreground and worker admission preserve the configured response reserve, using the effective
model/profile context cap. The lean fallback runs when the prompt would consume that reserve,
not only after it fills the entire window. Lean discovery keeps exact loadable names and omits
their optional descriptions; discovery still supplies those on demand. If immutable instructions
and pinned schemas still cannot fit, the request fails before inference without silently unloading
explicit tools or discarding the session's unfinished work.

Streaming accounting retains the latest provider-reported counters from each failed/retried
attempt, without adding cumulative snapshots together or counting the accepted final reply twice.
`partial_reported_attempts` distinguishes these observations from complete responses; their
usage remains incomplete and unpriced. Missing counters and unreported attempts are never
filled from prompt estimates. Model, purpose, worker and turn attribution remain attached.

Overflow recovery uses a fit-only compaction target: the request only needs to fit the model's context window and output reserve, even when the normal auto-continue threshold would pause the turn. Compaction preserves complete assistant/tool groups while tolerating orphan or duplicated tool-call records in irregular imported history.

Portable summaries update the previous committed summary with newly summarized history instead of summarizing the summary as transcript. A cut within a user turn gets a distinct turn-prefix summary, while the recent suffix remains verbatim. Read and modified file lists carry forward between compactions. Summarizer input is divided at message boundaries, conservatively discounted against the selected summarizer window, and a rejected window is halved and replanned. Single messages too large for a window are clamped with an explicit omission marker.

Parent and child conversations share reply recovery in `agent/turn_recovery.rs`. Empty answers and action-only stops get at most three continuation attempts, and malformed or mixed stalled replies share an eight-attempt bound. Productive work remains unlimited when `max_steps = 0`. Exhausted recovery leaves work incomplete, preserves completed effects, and never records completion. Quotes, code examples and optional future work do not restart a finished report; no classifier request is required. A completed `finish` must contain a report rather than a promise to act.

Background worker findings arrive once in the parent's next request with status, a bounded report and a reference to the complete artifact. Typed assignments, rule state and result data remain in the execution journal rather than being repeated in the model-facing summary. Use the delivered findings before repeating a worker's investigation. Todos are optional progress bookkeeping. `done` needs no finding/evidence payload and does not run a verification round. Accepted completion reconciles pending progress; explicit blockers and running work remain completion prerequisites. Plain final replies and explicit `finish` share these prerequisites and the final configured verification/repair pipeline. A failed check cannot become a completed turn, and unfinished replies share bounded reply recovery rather than a separate todo-reminder budget. Routine inspection does not force a separate todo-only request; the configured reasoning-recovery plan gate still applies when it is needed.

Workers publish their terminal runtime result before releasing their lease or exposing completion,
so status does not wait for the parent's event pump. An early acknowledgement suppresses later
redelivery of the same execution. A local worker cannot resume until its previous thread has been
joined; admission retains that thread's slot in the meantime. Batch admission scans persisted task
identities once and validates workspace/model compatibility before publishing runtime nodes.
Artifact or transcript publication failures mark the task failed rather than reporting a clean
success. Cancellation hooks run outside the active-tool mutex.

Experimental child context carry omits the assignment when its protected brief remains visible,
and omits current coordination already present verbatim in user history. History replacement still
carries missing instructions forward; rules, identity and safety framing remain. Worker
output-length recovery honors the same effective profile/inherited context cap as request sizing.

Turn efficiency reports count delivered worker results once per live execution, retain bounded
hashes of successful worker `read`/`grep`/`glob`/`find` calls, and count exact parent call overlap
after delivery. They retain at most 256 identities and explicitly flag truncated coverage.
Call arguments/source are not copied into the metrics. Different arguments/ranges are not matched;
changed source can make the same inspection necessary. Delivery and overlap do not establish
whether findings were understood, reused, or wasted, and never block legitimate verification.
Worker cost/token overhead remains in the usage ledger's worker and purpose views. First tool-result
and first known workspace-change-result latency are observations, not proof of a useful action;
tool-free turns leave these fields unknown. Semantic reranking has a bounded index-only response;
busy-input routing avoids inference when either concurrency limit leaves no worker slot.

Session history is append-only; tree navigation selects an abandoned branch without deleting it, and branch summaries are optional context for navigation.

Task-tool routing is local, deterministic and turn-scoped. The first request selects native specialists by workflow intent and MCP tools by indexed capabilities/domain relevance. Successful manifest reads can add dependency-domain signals from the existing result (currently complete single-target `package.json` only). Successful MCP discovery can expose matching same-server consumers for the following model request. Both serial and parallel results feed the same observer. Selection never executes tools or changes permissions; pinned harnesses, lean context profiles, child allowlists and ordinary approval policy remain authoritative. Automatic additions are monotonic within the turn, capped at six tools and 24 KiB of schemas; explicit unloads suppress reactivation until the next external request. Selection introduces no extra inference or project reads.

When tool discovery is available, model guidance prefers relevant eligible native or MCP specialists already offered over generic shell/web substitutes. If a needed capability is missing, it asks for task/domain discovery and use of the returned next-round schemas, not rediscovery of offered tools. This guidance is not gated on an MCP inventory, does not require calls for tool-free answers and grants no execution permission. It is prompt guidance, not evidence of increased live-model tool use; routing limits and approval policy are unchanged.

Duplicate recovery withholds only the exact repeated call, including reads. Different ranges and files remain available, changed source identities clear the affected restriction, and new user steering resets the turn's recovery state. A withheld call is reported explicitly as unexecuted; its cached observation still contributes to the shared repetition bound rather than being misclassified as a new execution failure.

Read restrictions track every target in `read.paths`, not just `read.path`; a changed or removed
target releases that batch's restriction without releasing unrelated calls. Unchanged reads use
short pointers when their earlier full results remain visible, including adjacent or overlapping
ranges jointly covered by several reads. Missing lines or pruned results still return full content.

Busy input defaults to **Let Dial decide**: at the next safe request boundary, a bounded auxiliary model request classifies the exact user message as steering or independent work. Changes of approach, corrections, dependent or overlapping work, and ambiguity steer the current task through its existing steering journal. Clearly independent requests start a background worker in an isolated project copy using the existing task authorization, spawn hooks, concurrency limits and result delivery. Splitting leaves the parent's current instruction and completed inference intact. Worker findings are not new user instructions; any copy integration follows the existing worker workflow. Routing failures, invalid decisions or failed admission visibly fall back to steering; cancellation restores unhandled input in order. When spawning is unavailable, steering is the only admissible route and no classifier request is made. Steering discards queued advice about the previous task's failures, repetition and completion, preserving protocol data, worker findings and safety boundaries. **Queue after this turn** remains an explicit opt-out. Automatic input does not cancel in-flight inference or tools while awaiting a decision, so a long operation can delay routing. No classifier is run when there is no pending input.

The busy-message chooser also offers explicit **Steer now** and **Independent worker** routes.
Steer now bypasses classification and cancels stale inference through the existing steering path;
started edits and tools retain their effects and only interruptible waits are stopped. The composer
shows pending, received, then applied after the steering journal accepts the direction. If the turn
ends before application, the direction is preserved as follow-up input. Independent worker bypasses
classification without changing the foreground instruction; failed admission preserves the message
as follow-up input rather than silently steering the foreground task. These choices do not grant
worker permissions or bypass isolation, approval, or concurrency limits.

Explicit evaluation/learning runs capture the actual inference request and target. Normal coding launches disable trajectory and recovery-sample recording, learned candidates, automatic Laya/learned-skill capture, project-learning updates and startup evaluations without rewriting saved learning preferences. When exporting a verified example to the training template, valid tool-result error flags are removed as execution metadata while the evidence text is preserved. Malformed flags and unknown fields remain rejectable. An HTTP 200 response to a WebSocket upgrade establishes SSE-only transport for that session, so an unsupported upgrade is not retried at every request.

Stop hooks can resume a completed candidate within their existing three-round bound. Every resumed candidate returns through focused verification before another stop-hook decision; hook resumptions do not reset the repair allowance or turn step budget. Persistent blocking ends with a blocked outcome, and a hook cannot restart a turn that needs user input, has failed, has unknown effects, or was interrupted. Rejected completion reports keep the candidate work summary separately labelled beneath the final verification or hook diagnostic, so callers retain useful work details without mistaking them for accepted completion.

## Evidence-driven verification and escalation

Focused checks distinguish application failures from unavailable test infrastructure.
Recognized missing selected toolchains and offline Cargo dependency-cache failures are
environment-unavailable, never passing checks; compiler errors, assertions and missing
application imports remain code failures. A command that executes zero tests supplies
no passing behavioral evidence, including in the persisted behavior-check cache.
Round settlement retains verification statuses and bounded receipt history.

Optional [requirement evidence contracts](requirement-verification.md) map operator-declared
requirements to configured checks and current rendered pixels. Their coverage is separate from
the execution outcome and never implies undeclared behaviors are covered. Parent and worker
settlement provide cause-specific safe next actions without automatic installation, permission
bypasses or replay of uncertain effects.

Tool failures carry a single recovery-state summary from shared parent/worker settlement:
the observed tool status, effect certainty, count of recorded committed paths, retained output
coverage, and the cause-specific next action. Zero recorded paths does not assert that nothing
changed; interrupted output remains partial evidence. The original diagnostic stays intact.
These summaries neither replay operations nor grant permission. Unknown effects still stop
automatic continuation, and earlier completed work is not rolled back by a later failure.
Provider-generation failures separately explain that no tools were dispatched from that failed
response, while earlier operations in the turn remain retained. The original error chain and
provider retry classification are preserved; recovery guidance does not automatically replay tools.

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
Rust source checks select the owning package manifest, including non-default workspace members;
manifest, lockfile and shared-root changes intentionally broaden to workspace compilation.
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
Behavioral outcome credit uses the latest controller-observed result for each command at the final
effect revision (native edits, opaque shell attempts, and extension effects). A failing pre-edit
baseline is preserved separately and can become a passing repair; it does not permanently poison
credit. Conversely, a pass before a later effect is stale and cannot earn final-revision credit.
Bounded exact diagnostic fingerprints distinguish an unchanged nonbehavioral baseline failure
from new diagnostics. Unchanged unrelated failures stay disclosed but do not request automatic
scope expansion; changed/absent/oversized fingerprints remain conservative. Declared behavioral
checks and registered reproductions remain actionable. Configured runner declarations and opt-in
strict outcome settlement/selective review are described in [requirement-verification.md](requirement-verification.md).
Recognized manually executed checks are bound after their own call settles. Cancelled, unavailable,
zero-test, or incomplete-process results never supply passing credit.
`check` `repro` registers a model-written reproduction (name plus shell command, at most four per
turn, same name replaces and clears its history) and runs it at once. It is selected by the
completion gate like any focused check and is lost at turn end. Its command is untrusted model
text: it runs through the same approval path as a model `bash` call (Ask prompts, Write prompts for
destructive forms, Readonly and deny rules refuse, `pre_tool_use` hooks may block, never run with
hook-rewritten text) and with the bash default deadline. Each run is a disclosed opaque shell effect
and is kept out of the evidence revision, so rerunning a reproduction neither stales other evidence
nor stands in for a fix. A reproduction earns one behavioral credit only when a controller-run
`code_failed` result was observed at an earlier effect revision and it passes at the final one.
One that never failed (or failed only through a model `bash` call, an unavailable environment, or a
denial) is reported `not_shown_to_reproduce` and earns no behavioral credit, though its pass still
counts as an ordinary current passing check. A pass with no effect since the failure earns nothing.
A failing or stale reproduction withholds all behavioral credit, as a failing configured behavioral
check does. Completion evidence lists each reproduction's state under `checks.reproductions`.
Executable repair records use that same classified, typed execution evidence, not printed exit
markers. Unverified runs invalidate a matching failure baseline rather than pairing an old failure
with a later pass. Related-test planning reuses JS package/runner metadata and Go directory scans
only within one planning invocation; later invocations observe changed manifests and tests.
Behavioral provenance still hashes fresh bounded source state, but no longer loads dependency
outlines that its record does not use. See [Core harness review](audits/harness-core-review.md) for the
comparison, rationale, and measurement limits.
Approval-denied writes are not treated as successful verification. `todo` is optional progress bookkeeping, not a verification checkpoint. `done` closes the current item and starts the next; findings and references are optional notes, including for legacy investigation entries. No separate tool-activity, citation or text-delivery gate applies. Accepted completion reconciles pending progress entries, while explicit blockers and running workers remain completion prerequisites. Failed checks remain failures independently of todo state.

Plain final replies and `finish` use the same completion prerequisites and final configured verification/repair pipeline. Empty replies, action-only promises and explicit admissions of unfinished work share bounded parent/child reply recovery. Invalid todo arguments still receive a simple corrected call and bounded recovery; there is no separate budget of todo-closing reminder turns. Productive work remains unlimited with `max_steps = 0`.

Default guidance starts with focused source inspection and moves to implementation once the path is known. It does not force a plan after an arbitrary number of reads. The optional reasoning-recovery plan gate still applies when explicitly enabled. Progress lists help substantial multi-step work; concise investigations and informational answers need no checklist. Optional source notes survive resume, and ordinary coding does not automatically harvest them into project learning. Identical successful observations remain subject to the shared read/investigation guards; changed sources and new user steering permit fresh observations.

### Completion evidence is not execution status

Every settled turn publishes additive `completion_evidence` in session control, includes it in the
efficiency event, and exposes it as `evidence` in the completion event. `TurnReport` and its existing
execution outcomes remain compatible. The versioned evidence distinguishes execution outcome,
unassessed deliverable coverage versus explicitly open obligations, current/stale check results,
missing/observed/not-required pixels, and effects outside checkpoints. Baseline and latest check
observations remain separate. Passing checks establish what ran, not independent proof of all
requirements. A simple answer needs no todo list and is not forced into verification ceremony.

Accepted reports close only the current progress item outside goal mode, as before. Pending items
stay open and are disclosed in the report/evidence rather than implicitly called complete. Missing
visual evidence remains an honest limitation, not a fabricated screenshot or a bypass of browser
approval. Automatic behavioral learning credit additionally requires no outstanding obligations or
missing required pixels. Native, shell, and extension effect revisions invalidate old visual evidence.

Recovery transitions have explicit lifetimes. Automatic and directed continuations reset transient
reply/narration recovery but retain evidence restrictions and committed-content edit-cycle history;
they cannot retry an unchanged refused edit by resetting the engine. New steering or worker
coordination can release obsolete call restrictions, but no transition releases an uncertain-effect
barrier. Inspection and a fresh turn are required after uncertain execution; completed effects are
never replayed or assumed rolled back.

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
  `assistance.lean_local` defaults to `true`, which enables a lean profile whenever the main model
  is served locally (no endpoint, or a loopback or LAN host), whatever its context window: a large
  window does not make a small model able to use the full prompt and tool offer. `"auto"` enables
  it only when the planned window is at most 32,768 tokens, and `false` turns it off; hosted
  routes and hermetic runs are unchanged.
  When active, it turns on discovery mode, caps the initial tool budget at 8 (an unlimited
  0 also becomes 8), and omits the visual-verification guidance unless `browser` or `computer` is
  offered and the skills catalog unless `skill` is offered. The resulting prompt stays
  byte-stable across turns. Measure it against the full profile with
  `dial benchmark --ablate-assistance`'s `ablate-lean_local` row.
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
    a fixed minimal tool set (`read`, `write`, `edit`, `bash`, `grep`, `glob`, `todo`, `finish`, `check`,
    `tool_search`, `tool_load`) plus loaded and memory tools; everything else loads on demand. A
    pinned `[harness] tools` list keeps its own offer. A lean route above that window gets the lean
    tier (the `lean_local` profile above). Other routes get the full tier.
  - Small-tier calls: the small tier works one tool call per response through the reliability
    one-call interface (`parallel_tool_calls: false` where the protocol accepts it, a per-request
    one-call contract, and a batch of several calls rejected unexecuted), and its prompt says one
    call per response instead of asking for batches. A saved `dial reliability check` record for
    this model that passed the two-call batch fixture keeps batching; `reliability.enabled = false`
    turns the one-call default off. The record is read once per session, so the mode and prompt do
    not change mid-session. Its offer is not simplified further.
  - Worked example: the small tier's built-in guidance ends with one compact example loop (a
    targeted `grep`, a ranged `read`, one `edit` in the active grammar's argument shape, a focused
    `bash` check, a short answer). It is fixed per grammar, so the prompt stays byte-stable, and is
    omitted when one of those tools is not offered or a replacement system prompt is configured.
  - Prompt scope: tool-specific sentences of the fixed guidance (`skill`, saved project maps for
    `codebase_memory`, the LSP) appear only when that tool is offered, like the preview and
    delegation sections.
  - Edit mode: local routes default to the `replace` grammar. Precedence is the `--edit-mode`
    override, the model profile `edit_mode`, `[harness] edit_mode`, `[edit] model_variants`, then
    `assistance.local.edit_mode`, then `[edit] mode`. `inherit` makes the local default follow
    `[edit] mode`. Hermetic evaluation ignores the local default.
  - Sampling: `model` (default) leaves sampling to the model. Dial's own runtime (no endpoint)
    applies the model's shipped `temperature`, `top_p`, `top_k` and `min_p` from its
    `generation_config.json` (a `do_sample: false` file requests greedy decoding with temperature zero) or GGUF
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
    the grammar is pinned or already `replace`). A reached step takes effect at the next user turn,
    so the prompt, tool offer and edit grammar never change within a turn (and at most twice per
    session). The reached step is remembered per model in the user directory file
    `local-profile.json` as soon as it is reached and applied at the first turn of the next session.
    0 disables escalation.
  - Benchmark lever: `ablate-local_profile` in `dial benchmark --ablate-assistance` turns the
    profile off (`assistance.local.enabled = false`) against the `dial` row.
  - Thinking (`assistance.local.thinking`, default `model`): leaves thinking to the model, server
    and explicit model profile, without automatic phase switching on main or side requests, or
    disabling thinking during loop-penalty retries. Set `phase` to opt into reasoning on a turn's
    first request, after failed tool rounds or a successful `todo` update, and off after successful
    tool rounds or intercepted generations. This opt-in requires a server thinking switch
    (llama.cpp and Dial's MLX runtime: `chat_template_kwargs.enable_thinking`; Ollama: `think`).
    LM Studio documents no request-level switch, so nothing is sent there. An explicit model-profile
    `thinking`, `reasoning_effort`, `thinking_budget` or `chat_template_kwargs.enable_thinking`
    always wins.
  - Model residency (`keep_loaded_secs`, default 1800, 0 = server default, max 86400): asks the server
    to keep the model loaded this long after each request (Ollama `keep_alive`, LM Studio `ttl`,
    which LM Studio applies to just-in-time loaded models). An explicit model-profile `keep_alive`
    wins. On Ollama, a model that was not loaded when the session started is unloaded when Dial
    exits (`keep_alive: 0`); a model that was already loaded is left alone.
  - Prompt cache (`prompt_cache`, default on, llama.cpp only): requests send `cache_prompt` and an
    `id_slot`. The main session uses slot 0. When the server has more than one slot (`/props`
    `total_slots`; for a Dial-launched server, the two-slot shared-pool layout it was started with
    when the installed `llama-server` supports it, see `docs/llm-transport.md`), side requests on
    the main model use the last slot and workers use the others, so neither evicts the main prefix.
    Prompt tokens served from the cache are counted in the evidence and shown in the status line
    (`N cached`) while a prompt is processed.
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
    after 20 samples each, clearing it is suggested unless it is at least 5% faster. When the main
    model is local, at least 6 tool rounds and 30% of all rounds failed, no `coach` role is assigned
    and automatic coach escalation is off, the `[escalation]` key that turns it on is suggested
    (`enabled`, else `auto_after_failed_rounds` or `max_auto_per_turn` at its default); the coach
    model itself is chosen in `/roles`, never by the suggestion. With `suggestions` off only the
    escalation step is recorded.
  - Time budget (`time_budget`, default on): when `reasoning_guard.max_pre_action_secs` is 0 and at
    least three decode-speed samples exist, the pre-action time limit for a request is derived from
    the measured prefill and decode speed (1.5x margin plus a minute, within 2 to 60 minutes). While
    a prompt is processed, the status line shows the time left (`~Ns left`) from the measured
    processing rate.
  Edit these keys in `/improve settings` under Local model assistance.
- Ambient codebase retrieval is on by default (`codebase.ambient_context = false` opts out); the
  model can still call `codebase_memory` for the complete view. It makes no model request: it
  selects task-relevant sources/findings whose cited sources still match their saved hashes, clips
  finding summaries (at most four, with source links), keeps only task-matching outline lines, omits
  the directory inventory, bounds stale notices to topic names, and states that the user's
  instructions take precedence and that the brief is not an edit grant.
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

Bundled image-archive renderer/font notices are in `crates/dial-agent/src/agent/compact/snapcompact/fonts/` and `THIRD_PARTY_NOTICES`:
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
