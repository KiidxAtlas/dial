# Agent loop and context maintenance

## Automatic sleep prevention

Dial automatically requests idle-system-sleep prevention while a foreground agent turn or
delegated worker is running. Each turn/worker owns its own request, so a background worker
can keep the machine awake after the foreground turn ends. Requests are released on completion,
error, cancellation, or process exit; leaving Dial open at an idle prompt does not keep it awake.
An active turn includes tool execution and waits for approvals or answers within that turn.

This does not change saved power settings, inhibit display dimming or screen locking, or request
that manual sleep be blocked. OS policy, lid closure and critical-battery behavior still apply.
macOS uses an IOKit idle-system-sleep assertion, Windows uses a system-required power request,
and Linux uses systemd-logind's idle inhibitor. Linux requires a reachable system bus and logind
permission; environments whose desktop power manager does not honor logind idle inhibitors may
still sleep. If acquisition fails, Dial reports a notice and continues the work without claiming
sleep protection. Hermetic evaluations do not request host power changes. Standalone commands
outside agent turns and background servers left running after a turn are not covered.

## Latest user request and corrected targets

Non-hermetic parent requests include a request-only reminder of the latest admitted user message,
including ordinary new-turn corrections as well as live steering. It follows task packets and
tool results rather than relying on an older plan or a clipped packet excerpt. The reminder uses
runtime-recorded user provenance, not model-authored working state, an effective turn draft,
peer messages or monitor output. Long messages retain a bounded UTF-8-safe head and tail with an
explicit omission marker; the full original remains authoritative in session history. This
context is not journaled as a new user instruction, and failure advice cannot displace it.

The reminder asks the agent to inspect an explicitly named target directly before substituting
a similarly named example or fixture, and to verify the requested behavior rather than merely
the files it happened to change. Git-ignored directories can be inspected through an explicit
`read` or search `path` without changing workspace permissions or expanding a broad search.
This is request-context guidance, not a semantic path allowlist or proof of task completion.
Focused projection and discovery regressions do not establish a live-model success improvement;
the outcome effect is unmeasured. Rebuild/restart Dial to use source changes; an already running
agent is not patched in place.

## Efficient tool use

Built-in full and compact prompts set one order of work: frame the outcome, acceptance criteria and
unknowns; gather with one planned batch of the most precise offered tools; then decide, act, verify
and report. Evidence suffices when each claim about the project (that something exists, is missing
or works) rests on a lookup made for it, or is marked `[INFERENCE]`. Reuse returned paths and
excerpts; calls that need an earlier result stay sequential. One-call profiles and request-specific
limits still take precedence. This is prompt guidance, not forced scheduling or a measured
model-efficiency improvement.

## Streaming delivery and cancellation

Model-to-controller payload delivery applies backpressure with eight queued frames, each containing
at most 16 KiB of string data, fragmented on UTF-8 boundaries. This is a 128 KiB queued-string bound,
not a total request-memory bound: the provider's current callback, one blocked frame, the terminal
reply and the controller's current event are separate. Event reassembly is capped by the existing
model-stream byte limit and fails explicitly rather than silently truncating an event.

Cancellation discards queued payloads and partial reassembly before waiting for the provider;
late text, reasoning and tool events are not delivered. A separate single terminal slot preserves
provider completion/error ownership. UI/journal failures disconnect delivery before joining so a
full queue cannot by itself prevent shutdown. These transport regressions use local fixtures;
they do not establish live-provider cancellation latency or measured memory/performance gains.

## Launch-context recovery

Native shell execution carries typed controller-owned launch facts across foreground commands,
named sessions and managed pipe/PTY jobs. Shared parent/worker feedback uses these facts to
distinguish an invocation without a terminal or open stdin from an unavailable capability.
Bounded terminal/input diagnostics are advisory signals, including applications that exit zero;
they are not status, authorization or behavioral verification. Eligible deferred tools are named
with loading instructions; policy, unsupported routes, cancellation, uncertain effects and
incomplete output retain their boundaries. Recovery never silently replays a command, changes
its target, enables PTY globally or interprets a shell command as a tool call.

Repeated-read recovery uses exact file content identities and the existing visible directory
listing. A directory listing change (including a visible nested addition/removal) releases only the
matching read feedback; unrelated or excluded/deeper-than-listing changes do not. Directory
identities are recovery bookkeeping, never edit witnesses, authorization or verification credit.

Existing managed jobs are inspected and controlled by their identity, not duplicated. Both direct
starts and auto-background adoption require an observation before named-target completion credit.
Requested-target evidence remains tied to the current effect epoch and records behavior as
`not_assessed`. Offline and real-local-model launch/input cases are in
`examples/benchmarks/recovery.toml`; scripted results establish harness behavior, not model quality.

## Failed prerequisites and tool receipts

Within one parent or worker tool batch, a failed native mutation withholds unstarted actions
whose source dependencies overlap or cannot be established, including checks and opaque launches.
Diagnostic reads, job observations and proven-disjoint native actions remain available. Results
already prefetched or executed retain their actual outcomes. The next model request may inspect
or correct the prerequisite within the same permissions; no operation is replayed automatically.
An observational native no-op does not advance the source-mutation revision solely because its
tool is named `edit`, `write` or `ast_edit`.

Tool-loading receipts describe effective next-request availability, not just requested pins.
Loading and unloading the same tool in one request is refused before mutation; removing a pin
does not necessarily remove a core offered schema. Persistence failure remains an error. Search
readiness breaks relevance ties only, and never hides an eligible exact blocked name or widens
a worker's tool allowlist.

## Context maintenance

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
The built-in overflow environment keeps deferred tools listed by exact name without expanding
their capability previews. Ordinary lean-local prompts retain their bounded capability hints;
operator replacement prompts and templates retain their existing inventory behavior.
With efficiency enabled, non-hermetic sessions use compact built-in engineering, execution,
workflow, delivery and delegation prose, including hosted models. Full specialized-tool rules,
visual-verification guidance and the callable schemas stay in place; this does not enable the
lean local tool profile. Deferred MCP tools use a bounded, untrusted per-server index when
`tool_search` is available, instead of repeating every tool's name and description. Discovery
still searches the entire eligible catalog and loads schemas for the next request. Native tool
previews, operator instructions, custom templates/replacement prompts and project context are
preserved. Without search, the full MCP inventory remains. Disabling efficiency restores the
full built-in prose and inventory. Prompt-byte savings alone do not prove a live-model speedup.

The compact built-in environment also refers to the supplied callable schemas instead of repeating
their tool-name inventory. The schemas, deferred discovery catalog and capability-scoped rules are
unchanged; custom templates and replacement prompts retain their existing environment inventory.
This removes duplicated input text, not tool access, checks or user/project instructions.

An earlier shared-policy compression retained profile selection, tool schemas, permissions,
completion checks and operator-supplied content. Its rendered fixture (17 offered tools, empty
project/operator context, default reasoning strategy) measured the historical comparison below;
later wording changes, including answer-delivery guidance, are not reflected in these numbers:

| Presentation | Policy bytes before → after | Total message-text bytes before → after |
| --- | --- | --- |
| Full | 12,805 → 9,528 | 13,709 → 10,432 |
| Compact | 8,535 → 6,245 | 9,311 → 7,021 |
| Lean | 8,535 → 6,245 | 9,439 → 7,149 |
| One-call | 8,357 → 6,177 | 9,261 → 7,081 |

Reproduce current sizes with
`cargo test --locked -p dial-agent --lib compact_policy_stays_within_its_byte_budget -- --nocapture`.
These are UTF-8 text bytes, not tokenizer counts, serialized provider requests or live Harbor
prompts. Tool schemas and task context are excluded. Compact mode omits the optional diagram
style suggestion; one-call, trust, disclosure and capability-scoping rules remain. Model quality,
request counts, elapsed time and token-use effects are unmeasured pending controlled evaluation.
Rebuild/restart Dial to use the source changes; an existing binary is not patched in place.

Request-efficient execution is guided at the shared policy owner: inspect named targets directly,
combine related shell inspections, and compute/serialize/check permitted generated artifacts in
one run rather than transcribing the result in another call. Stop after the requested requirements
and required gates are satisfied; rereads, version probes and repeated checks need a concrete uncertainty.
This does not waive required gates, missing runtime disclosures or source/config edit contracts.
Generic specialized-tool/edit preferences apply to source/config files; generated-data files
follow the single-run rule rather than a competing preference to transcribe them with `write`.
The existing `verify` tool revalidates declared inputs before reusing a pass; the shell pace
guard remains advisory because command identity alone cannot prove an external environment fresh.
The shell pace guard compares complete commands, working directories and named sessions rather
than shared pipeline heads. Different filters, shell prefixes and presentation separators do not
make an inspection redundant. Its advice refers to captured output only: a downstream filter may
have discarded producer output before artifact capture.
Normal, nonminimal default offers prepare the mandatory containment gate before including `eval`;
failed preparation omits its automatic offer, including retained task routing. Passive readiness
only reads the cached result, while explicit loads, pinned and child selections retain control
operations. Cell execution still fails closed. Repositories offer `git` without a loading round.
Pinned selections and model/mode restrictions still apply; registration policy is unchanged.
Python checks reporting a located `RuntimeWarning: coroutine ... was never awaited` are unverified,
even with exit zero; the coroutine did not establish behavioral coverage. Testing guidance calls
out queued work and awaited cancellation cleanup, not syntax-only or synchronous cleanup stand-ins.
Final verification guidance also checks deliverable file sets without temporary build outputs and
uses the expected caller/environment rather than test-only overrides (for example, a temporary
PATH export cannot prove an executable is installed on the caller's PATH). These changes target
observed extra turns and missed requirements; unit tests establish contracts, not a model speedup
or a successful fresh benchmark.

Built-in guidance asks agents to scale effort to uncertainty, impact and reversibility, reuse
visible evidence, and implement once the path is supported. New or contradictory evidence can
reopen investigation; successful edits alone do not prove correctness. Isolated tests with test
doubles are permitted but cannot substitute for live integration evidence. Failed checks require
diagnosis, not weakened assertions. Preview instructions follow the configured browser/server
capabilities rather than naming unavailable tools. Delegation handoffs carry the outcome, owned
scope, shared interfaces, evidence references and checks with an owner. Shared builds/checks wait
for relevant edits to settle; repairs or changed inputs still need rechecking. These are prompt
policies, not enforced scheduling or proof of live-model compliance; they do not add model
requests, enable delegation automatically, or change tool and approval permissions.
Delegation is selective by default: coupled implementation and focused testing stay in the
main loop; independent parallel work, fresh-context review and context isolation can use workers.
`task.prefer_subagents = true` (Settings → Tools → Subagents → Prefer subagents) instead adds
specialist-first and tester-owned verification guidance. Neither setting enables withheld tools,
changes worker permissions/concurrency, or bypasses configured completion checks. Both full and
lean prompt variants use the preference; custom replacement prompts remain operator-owned.
No live-model quality, latency or cost improvement is implied by this prompt-policy change.

Full and compact built-in parent prompts share the same testing-decision policy. Agents are
asked to inspect owning coverage first and reuse it when it catches the regression: a code
change alone does not require a new test. New tests address a specific uncovered behavior,
invariant or failure boundary, at the lowest sufficient layer, without duplicating input
matrices. Worker/test-runner role guidance and the example test-runner skill follow the same
decision rule. Reports briefly identify reused coverage or the gap requiring new tests.
Checks are optional unless required by configured/user gates; when warranted they start focused
once relevant edits settle, not after each edit. Diagnose errors before a rerun and disclose gaps
rather than inventing required optional checks. When three model rounds in a row each make one lookup (`read`, search or safe
shell, ignoring a `todo`/`working_state` call beside it), the turn adds advice to batch known
lookups in one response — read every relevant search hit together with the next searches — and
repeats it every three such rounds. Broader runs need affected interfaces,
unresolved risk, an explicit request or a required project gate. Passing evidence is reusable
only while relevant code, command, configuration and environment remain unchanged; uncertain
inputs need rechecking, and reuse is not a new execution. This guidance does not relax
completion checks or project gates. Test-authoring time, run time and live-model compliance
have not been measured; no efficiency or outcome improvement is claimed.

For an unlimited foreground output setting, the model's advertised output ceiling is bounded by
the context remaining beside the current prompt; it is not reserved in full before admission.
Explicit output limits and the compaction policy's response reserve remain authoritative.

Both foreground and worker admission preserve the configured response reserve, using the effective
model/profile context cap. The lean fallback runs when the prompt would consume that reserve,
not only after it fills the entire window. Lean discovery keeps exact loadable names and omits
their optional descriptions; discovery still supplies those on demand. If immutable instructions
and pinned schemas still cannot fit, the request fails before inference without silently unloading
explicit tools or discarding the session's unfinished work.
The lean presentation also shortens built-in engineering and execution guidance while retaining
approval, trust, preservation and verification rules; operator-supplied instructions are not shortened.

Ambient provenance recall and saved-codebase records are appended only when absent from the current
visible branch, even with optional efficiency settings disabled. Exact note records and source/finding
rows are deduplicated individually, so a changing neighbour does not repeat unchanged evidence.
Revisions, content, source digests and provenance must still match; changed records reach the model.
Earlier messages and cacheable prefixes are not rewritten. Compaction or branching that removes a
record permits retrieval again. This is exact-record admission, not semantic merging or a combined
ambient byte budget. Automatic semantic/Hindsight recall also admits facts against the latest user
subject, not arbitrary words from prior assistant discussion; explicit recall remains broad.

Ordinary coding tasks no longer carry unrelated `web_search`/`web_fetch` schemas. Explicit web
research or URL intent routes the relevant tools locally before inference; missing matches remain
discoverable with `tool_search`/`tool_load`, and explicit pins, configured tool restrictions and
hermetic offers retain their meaning. Routing does not itself call a model or execute a tool.

With `thinking = "auto"`, the turn's effort judgment waits until an effective request profile
declares selectable reasoning efforts. Requests with no profile or no declared efforts skip the
otherwise unused judgment. A supported fallback can still trigger it; subsequent steps and repair
rounds reuse the result (including a failed attempt) rather than asking again. External-thinking
and hermetic exclusions, judgment output budgets and automatic-compaction defaults are unchanged.
These changes have focused offline coverage, not a matched live-model token/cost benchmark.

Streaming accounting retains the latest provider-reported counters from each failed/retried
attempt, without adding cumulative snapshots together or counting the accepted final reply twice.
`partial_reported_attempts` distinguishes these observations from complete responses; their
usage remains incomplete and unpriced. Missing counters and unreported attempts are never
filled from prompt estimates. Model, purpose, worker and turn attribution remain attached.

Overflow recovery uses a fit-only compaction target: the request only needs to fit the model's context window and output reserve, even when the normal auto-continue threshold would pause the turn. Compaction preserves complete assistant/tool groups while tolerating orphan or duplicated tool-call records in irregular imported history.

Portable summaries update the previous committed summary with newly summarized history instead of summarizing the summary as transcript. A cut within a user turn gets a distinct turn-prefix summary, while the recent suffix remains verbatim. Every subsequent turn-prefix or branch window explicitly integrates the prior output using the original format. Update instructions prioritize active requests, approval boundaries, unrelated work, unresolved failures, running tasks, current edit-witness distinctions and verification gaps; resolved incidents retain their supported resolution and retrieval pointer rather than repeated diagnostic transcripts. File-operation inventories prioritize recent mutation targets, with at most 16 targets per list and 4,096 combined path bytes; older read inventories do not accumulate across summaries. These lists are attempted targets from call arguments, not edit witnesses or evidence of successful reads or modifications; legacy tags remain readable, and full calls/results remain in session history. Summarizer input is divided at message boundaries, conservatively discounted against the selected summarizer window, and a rejected window is halved and replanned. Single messages too large for a window are clamped with an explicit omission marker.

Parent and child conversations share transport reply recovery in `agent/run/turn_recovery.rs`:
empty or malformed output has finite recovery, while nonempty prose goes to the state completion
owner. Candidate wording, quotes, optional future work or repetition never establish completion.
The shared completion gate owns the single instruction-scoped work/investigation nudge described
below; productive work remains unlimited when `max_steps = 0`.

### Turn reliability boundaries

One turn state covers a whole user turn: verification-repair and stop-hook rounds resume it and
count only their own steps. One-shot notes stay queued until a model reply is actually received,
so a request discarded by overflow, failover or regeneration does not lose them. These boundaries
end a turn with a stop report and keep completed work, instead of failing it:

- 24 consecutive steps without a model reply (compaction, fallback or context retries making no
  progress). Context-overflow retries count consecutively, and an overflow that compaction cannot
  fix stops the turn.
- Recoveries of any kind without productive progress (unusable replies, rejected previews,
  regenerations, length stalls and stalled tool rounds) share one ledger and stop after 16
  setbacks. There is no growing corrective prompt or redirect-at-six request. Configured local
  escalation still applies at its existing authorized, cost-bounded steps; no stronger model is
  selected implicitly. Parent and worker tool results settle once per model round, before a
  terminal answer is published, not once per failed call. Novel returned observations, novel
  committed mutations and new actual verification observations can reset the ledger; successful
  bookkeeping, reused checks, repeated observations and no-op/reverted edits cannot. A mixed batch
  retains real productive work even when another call fails. Bookkeeping-only rounds are neutral:
  they neither consume nor erase recovery debt. Automatic continuation and context rewriting do
  not erase that debt. This is an execution-fact boundary, not proof that an observation is relevant
  or that the requested task is correct.
- `--max-time` and `[budget]` cost caps are checked when a step is admitted, between the calls of
  one tool round, before settlement makes further model calls and while a reply streams; a
  request's timeout is clamped to the time left. A capped model that reports no price gets a
  one-time notice that the cap cannot stop it.
- With a finite step limit, the last step asks for a report and runs no tools. At 80% of the
  `--max-time` window or a cost cap, the model is asked once to prioritize finishing.
- The `portable_rules.max_regenerations` limit and a refusal to continue after uncertain effects.

### Answer delivery and work completion

Plain replies, `finish`, and explicit clarification use one controller completion decision. Open
execution steps or deliverables, live jobs, stale or failed required evidence and unverified edits
cannot be accepted as completed or bypassed with `outcome=answered`. A refused candidate is kept
beside its observed unmet prerequisite and the turn ends `Incomplete`. Final prose does not close
todos. Optional follow-ups remain separate, and explicit blockers and necessary questions retain
their non-success outcomes after investigation.

A fresh workspace work request with no observed tool attempt gets one instruction-scoped
investigation nudge before a question or completion is accepted. Self-contained general questions
need no tool. Peer and monitor callbacks carry an existing task rather than creating a fresh
investigation obligation. Missing work and post-edit evidence share this one nudge allowance;
inner-loop retries cannot renew it, and repeating the candidate never grants completion. Phrase
recognition can choose recovery wording, but does not decide acceptance. Controller observations
are a prerequisite, not independent proof that arbitrary model claims are correct.

Planning distinguishes three records: user-required durable deliverables belong in
`working_state`; execution steps are `todo` items with `kind=step` (also the default for omitted
kinds and older snapshots); optional experiments and future checks use `kind=follow_up`.
Pending follow-ups are not automatically started, and blocked follow-ups do not gate completion.
Outstanding follow-ups remain recorded and visible in notices rather than being silently closed.
Required deliverables must not be relabeled as optional todos to bypass completion safeguards.

Full and compact prompts prioritize the active request, hard boundaries, investigation before
asking, coherent action, scoped checks and a short honest report. Optional experiments, broader checks and unapproved external or
live-service testing are not delivery prerequisites. Missing evidence and prerequisites must be
disclosed; configured/user gates, approval boundaries and durable obligations remain in force.
These semantics do not establish a live-model compliance or quality improvement.

Automatic focused-check repairs keep their existing turn-wide allowance (three by default), but
can stop earlier for no progress: after a repair, the same outstanding confirmed code-failure
diagnosis with unchanged complete declared check-input snapshots records `repair_no_progress`
and leaves the turn failed with its diagnostics, without rerunning an unchanged failure. A no-op or reverted edit cannot buy another
round merely by advancing a mutation counter. If a check lacks a complete input witness, the
fallback compares effect revisions; unknown dependencies are not certified unchanged. This is
an early-stop safeguard, not proof that a changed input is a useful repair. Infrastructure,
permission, cancellation, timeout and unverified problems never become automatic code repairs;
their existing failure obligations remain visible. No extra reviewer-model request is added.

When `[verification].automatic_checks` is off and applicable checks are missing for current edits,
the completion gate runs them through the same approval and freshness owner as `verify`. A
passing current check adds no review request. An unresolved check receives at most the shared
completion nudge, then remains incomplete with its diagnostic. Read-only and already-verified
work has no default completion self-review. Configured independent coach/advisor review remains
available under its own explicit policy. A deadline or cost boundary prevents further recovery.
An interrupt during a tool round keeps finished parallel results and records
calls that never started as not run. A panicking read-only tool is a failure with no effects
(except `bash`, or a tool that committed before panicking). A timed-out or killed check counts as
failed; a cancelled one settles nothing. Calls repeating in an exact short cycle (period 2 or 3)
get an advisory note. A file changed by six committed edits (counted once per call) with no
passing check in between gets an advisory edit-churn note asking the model to step back and verify
or re-plan; the note repeats every six further edits, a passing `verify` or recognized shell check
clears the counts, and the turn always continues.

Both the foreground turn and child workers decide how to recover from a reply in
`agent/run/reply_triage.rs`. A reply cut off at the output limit with no text or tool call uses the
single per-turn reasoning-gate recovery (a second hit stops). Otherwise the limit is raised, the
partial answer is kept with a continuation request, or recovery stops as incomplete. A dropped tool
call with invalid JSON, or a tool call written as plain text, is a form failure. It is never run,
and it gets one shared correction. Each loop keeps its own UI, journaling and message placement.
Each request's tail is assembled once by `run::guidance::assemble`: advisory notes are curated to
one by priority, then plain harness notes merge into one message. Marked context stays separate.

Completion guidance honors a user's requested final-response format when work is complete and no
blocker, unverified check or safety-relevant gap requires disclosure. This is model guidance, not
an enforced output guarantee; completion gates and inference labels remain unchanged.

When a configured fallback is ready, failures it accepts (`retry.fallback_on`) get two attempts
and at most a 10 s wait on the primary before failing over. Immediate reasoning-parameter recovery,
OAuth refresh and credential/account rotation share that per-class attempt budget; rotation cannot
extend it to the larger `retry.max_attempts` limit. Without an applicable fallback, the ordinary
attempt budget remains in effect. A primary that cannot be rebuilt when the fallback's cooldown
ends keeps the fallback and is retried 5 minutes later. With `retry.fallback_revert = "never"` the
session does not return at cooldown end at all; see [model routing](model-routing.md).

Different successful lookups also have a shared foreground/worker synthesis boundary. After twelve
observation-bearing model rounds without a novel committed change or accepted task resolution,
Dial requests synthesis; after 24 it reserves a tool-free checkpoint request. Batches count as one
round. Successful skill, memory (`recall` and `codebase_memory` read actions), capability discovery
(`tool_search`) and peer-list lookups count too; remembering findings, loading tools or sending peer
messages do not. The checkpoint must return JSON with a new nonempty `finding`, `evidence` containing
actual returned observation tool-call IDs, and a nonempty `next_action`. It does not complete the
task or verify the claims. Referenced IDs establish provenance only; Dial cannot prove that a
paraphrased finding is substantively new or that it follows from the cited result.

A valid synthesis renews the research window, including for read-only agents with no working-state
tool. Dial keeps the accepted finding and concrete next action in request-tail follow-through
guidance until a novel committed change, accepted task resolution or new instruction. Advisory
work should answer from the established evidence; further retrieval should address a specific
remaining question or contradiction rather than restart a broad survey. These are model claims
and guidance, not verification or a semantic proof that a lookup is necessary. There is no total
research cap and no requirement to edit files or manufacture todos.

Focused full and lean prompts also connect discovery to action: once a likely target is supported,
state the concrete defect or hypothesis and run the smallest check that distinguishes it before a
reversible implementation. Reopen discovery for contradictory evidence or a specific missing fact,
not simply to reconfirm the same target. A focused question is appropriate when ambiguity changes
scope or correctness; advisory questions still require an answer, not an edit.

Checkpoint text and reasoning are not streamed as an answer. Accepted JSON is retained in the
session's internal `research_checkpoint` control record with `verified: false`, rather than as an
ordinary assistant message, so it does not replay as a user-facing answer. Tool schemas remain
for provider-history compatibility, but structured calls, speculative streaming dispatch, inline
edits and completion handling cannot execute on a checkpoint response. A valid checkpoint is
accepted even when the reply also requested tools (the calls are discarded, never run), and one
surrounding Markdown code fence is tolerated; small local models often do both. A reply that only
calls tools gets a correction naming those tools and showing the expected JSON shape with a real
evidence ID. Three invalid checkpoint
responses leave the work incomplete with earlier effects preserved. No-op edits, todo polling,
state bookkeeping, failures, automatic continuation and compaction do not reset the window; new
user steering/worker coordination does. A worker's final reserved summary request still takes
precedence over research synthesis.

Completed edit-preview rejections also consume bounded no-progress recovery, even with an unlimited
task budget. Exhaustion returns an incomplete result with the preview diagnosis and executes no
tools from the rejected generation. Request attempts remain visible in reports, while finite
continuation budgets use charged steps: refunded replies and controller-induced retries do not
consume the allowance reserved for verification repairs or stop-hook continuations.

Worker coordination is checked while waiting for provider capacity and again immediately before
dispatch. Messages arriving during generation cancel only that inference scope and are retained
in order for the replacement request. Superseded replies cannot dispatch tools; physical provider
usage remains accounted for, while the existing coordination retry allowance is preserved.

After successful worker inference, delivered request-only harness notes are retired even when
buried in history; failed requests retain them for retry. Operator instructions, coordination,
marked context/evidence and safety guidance remain. A worker's reserved final summary request
may retain tool schemas for provider-history compatibility, but cannot dispatch structured tool
calls, text-form calls or recoverable inline edits. Such attempts end incomplete with an explicit
unexecuted-call diagnostic and any available findings; they do not gain extra execution budget.

Background worker findings arrive once in the parent's next request with status, a bounded report and a reference to the complete artifact. Typed assignments, rule state and result data remain in the execution journal rather than being repeated in the model-facing summary. Use the delivered findings before repeating a worker's investigation. Todos are optional progress bookkeeping. `done` needs no finding/evidence payload and does not run a verification round. Accepted completion reconciles pending progress; explicit blockers and running work remain completion prerequisites. Plain final replies and explicit `finish` share these prerequisites and the final configured verification/repair pipeline. A failed check cannot become a completed turn, and unfinished replies share bounded reply recovery rather than a separate todo-reminder budget. Routine inspection does not force a separate todo-only request; the configured reasoning-recovery plan gate still applies when it is needed.

Worker waits for the shared workspace-operation lock observe cancellation before dispatch. A
batch launch reports individual persistence-lease failures without hiding workers already started.
Artifact-retention and transcript-finalization errors are explicitly labelled in the model-facing
summary, separately from the worker's candidate answer; a failed persistence operation is not
silently presented as successful work.

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

Request telemetry counts controller phases (`requests_by_phase`), admitted requests before the
first observed workspace change, prefix invalidations, and stable/changed serialized message
bytes. These observations do not establish that a request was avoidable. Prefix identity is
provider-neutral after hooks; system and developer messages share an instruction identity.
Changed developer instructions now count as `system_changed` rather than `history_changed`, so
historical invalidation categories are not directly comparable. Schema-array bytes include JSON
separators. Stability is not a provider cache-hit claim.
Decoded reply text and serialized argument byte counters exclude hidden reasoning and failures
without a decoded reply. Recovery counters do not change recovery limits.

`projection_bytes.backend_operations` reports calls and inclusive elapsed microseconds for check
input capture, refresh/reuse, verification source hashing and native-edit preflight. `storage_io`
reports process-local write/sync counter deltas from inner-turn admission to efficiency-report
construction, excluding publication of the report itself and outer-turn cleanup. Nested costs
overlap and must not be added as a wall-time partition. Equal-state control publications flush pending runtime boundaries without appending a state record
or advancing memory authority; durable acknowledgements and input freshness remain intact.

Session history is append-only; tree navigation selects an abandoned branch without deleting it, and branch summaries are optional context for navigation.

With legacy `assistance.compact_tools=false`, task-tool routing is local and deterministic. Tools routed on one turn stay offered on later turns
(up to twelve, oldest dropped first; four in lean and minimal local profiles) so the system prompt and
tool array, and with them a provider's prompt cache, do not change back and forth between turns;
a tool that stops being an eligible candidate, or that is explicitly unloaded, is dropped. The first request selects native specialists by workflow intent and MCP tools by indexed capabilities/domain relevance. Unique tool-name capabilities can also match explicit inspection requests without a server name; ambiguous and generic matches remain conservative. Complete single-target reads of `package.json`, `Cargo.toml` and `pyproject.toml` can add bounded dependency-domain signals; unsupported requirement forms are skipped. Successful MCP discovery can expose matching same-server consumers for the following model request, including one additional tool slot when the count budget is full. Both serial and parallel results feed the same observer. Selection never executes tools or changes permissions; pinned harnesses, child allowlists and ordinary approval policy remain authoritative. Automatic additions are monotonic within the turn, capped at six tools and 24 KiB of schemas (seven tools after a successful discovery). Lean and minimal local profiles route MCP tools and the native web pair, capped at two tools and 8 KiB (three tools after discovery). Clear web/research intent prioritizes eligible `web_search` and `web_fetch` ahead of incidental MCP matches; ordinary coding turns still omit them. The byte caps remain unchanged and overflow fallback disables automatic additions to preserve headroom. Explicit unloads suppress reactivation until the next external request. Selection introduces no extra inference or project reads.

Intent rules read request prose, not the names it mentions: code spans and identifier-like tokens (`$request_method`, `server.py`, `/usr/bin`) do not count, while a URL still signals URL intent. Tool names that are ordinary words (`task`, `message`, `goal`, `monitor`, `data`, `verify`, `find`, `finish`, `memory`, ...) route only when marked as tool names, so "Your task is to" does not offer `task` and "verify the result" does not undo the print-run deferral of `verify`. A web server, app or page to build is not web-search intent, a page needs a visual term (not just "verify") before `browser` is added, and asking to keep a service running is not runtime watching for `monitor`. In the Terminal-Bench task instructions this keeps the nginx setup task from adding seven specialist schemas (48.0 KB of schemas per request down to 24.9 KB in the offline real-registry offer). Fewer billed tokens per live run are expected, not measured.

Source-format routing distinguishes input inspection from output creation: for example, analyzing
plain-text logs and writing `summary.csv` does not by itself select the CSV reader or evidence tools.
This also holds for comma-separated instructions such as `Analyze text logs, write summary.csv`;
commas within filenames remain part of the name rather than globally splitting the request.
Output target phrases after creation verbs (`write`, `export`, `save`, `create`, `generate`,
`produce`, `output`) do not supply input-format signals. Other words and genuine input clauses
remain available for source routing, including `Write a script to parse sales.csv and summarize it`.
Plain CSV/JSON transformations with a generated CSV/JSON target, such as `From orders.csv,
create paid-summary.json`, favor the existing computation workflow rather than automatically
adding `data`. This applies in interactive and print runs. Explicit inspection/analysis,
workbook inputs and stated large-data work retain the specialist; `Analyze sales.csv and write
summary.json` still selects it. Routing does not open files or infer their actual size. Explicit
tool requests, catalog discovery and retained selections remain available. The
`evidence`/`evidence_search` pair is routed only for citation intent (evidence, citations, quotes,
cited sources), no longer alongside every routed format tool.
This is conservative lexical routing, not natural-language understanding: explicitly named tools and
ordinary discovery remain available. Schemas, validation, permissions and routing budgets are unchanged.
Reduced schema bytes are an offline measurement, not evidence of fewer live requests or better answers.

Lean and one-call prompts list each native loadable tool on its own line with the first sentence of its description (at most 80 characters, about 1.2 KB in total), so a small model does not have to guess what a bare name does. Tools past that budget, placeholder families such as `dap_<adapter>` and every MCP tool stay on the `Names:` line, so each loadable name is still listed exactly. Lean and one-call prompts also include a bounded set of short, untrusted MCP capability descriptions. Some descriptions can be omitted when the hint budget is full; `tool_search` can discover other eligible tools. A server being connected does not guarantee selection or permission to execute a tool.

In legacy automatic discovery, when an offered tool rejects a call because of an argument it does not declare, and exactly one to three unoffered eligible catalog tools declare every rejected key, those tools are loaded for the next request. The refusal names them (for example, an argument only `bg` accepts sent to another tool). The call is not executed or rewritten, and constrained repair is skipped for it. Explicitly unloaded tools, `tool_search`/`tool_load` and ambiguous matches are never loaded this way. When `bg` is eligible but not offered and `tool_load` is, the `bash` description and the automatic-background notice say that `bg` is a Dial tool to load, not a shell command.

If a final answer says a capability is unavailable ("cannot", "not available", "unable to" in the same sentence as a tool, terminal, browser, network, display or similar), the instruction has not called `tool_search` or `tool_load`, and unoffered eligible tools exist, Dial sends the answer back once with a request to search the catalog. If the model then gives the same answer, it is accepted. Code blocks are ignored, and hermetic runs, stop boundaries and new instructions reset or skip the reminder. This is a lexical heuristic for one extra request, not proof of a capability or a permission grant.

When tool discovery is available, model guidance prefers relevant eligible native or MCP specialists already offered over generic shell/web substitutes. If a needed capability is missing, it asks for task/domain discovery and use of the returned next-round schemas, not rediscovery of offered tools. This guidance is not gated on an MCP inventory, does not require calls for tool-free answers and grants no execution permission. It is prompt guidance, not evidence of increased live-model tool use; routing limits and approval policy are unchanged.
Discovery entries retain `enabled_next_round` and add `status` plus `next`. `tool_search` and
`tool_load` report `already_offered` for a tool that was in the offer before the call,
`loaded_next_request` for one the call activated, and `not_loaded` otherwise; read-only `*`
listings report `offered` or `not_loaded`, since they cannot tell an earlier load in the same
request from the current offer. An eligible result below the automatic activation cutoff explicitly
points to `tool_load`; an activated result points to its exact tool name on the next request,
never the current batch. Explicit loads survive compact/minimal offer budgets, subject to the
existing mode and allowlist restrictions. Tools outside the eligible catalog are not advertised.

Catalog and discovery entries also report advisory `readiness` and `operation_readiness` from
current configuration and owner-held metadata, without starting processes or probing services.
Automatic routing defers known missing-prerequisite or unsupported candidates and prefers ready
candidates when relevance is otherwise tied. Unknown readiness is not unavailability; eligible
tools remain explicitly discoverable/loadable. A ready tool still needs ordinary authorization,
and readiness neither proves service health nor guarantees a successful call.

An empty search keeps its original result and receives a controller note: no tool was loaded;
use an already offered tool or an exact loadable name if known, otherwise narrow the capability
query with new information or report the current configuration limit. It is not permission to
fall back to an unapproved shell command or invent research results.

If a simple Bash command invokes a registered Dial tool name and a completed process returns
exit 127 with a matching `command not found` diagnostic, the result explains that the name is
a tool rather than an executable. The same applies to `bg`, which Bash runs as its job-control
builtin, when it exits non-zero with `bg: no job control`. The guidance points to the offered schema or an exact `tool_load` call
for the next request. Dial does not reinterpret or execute the shell arguments as a tool call,
install a similarly named program, or release the unchanged-call retry barrier. Missing programs
not in Dial's catalog, incomplete processes, compound commands, and policy denials retain their
ordinary diagnostics; this coaching never grants filesystem, network or execution permission.

Duplicate recovery withholds only the exact repeated call, including reads. Different ranges and files remain available, changed source identities clear the affected restriction, and new user steering resets the turn's recovery state. A withheld call is reported explicitly as unexecuted; its cached observation still contributes to the shared repetition bound rather than being misclassified as a new execution failure.

Read restrictions track every target in `read.paths`, not just `read.path`; a changed or removed
target releases that batch's restriction without releasing unrelated calls. Unchanged reads use
short pointers when their earlier full results remain visible, including adjacent or overlapping
ranges jointly covered by several reads, successful native edits, or `glob`/`find` source previews.
Only complete numbered rows count; missing lines, changed snapshots and pruned results still return
full content. Identical-read and covered-range pointers share one compact format and are used only
when shorter than the model-visible source after file-handle projection. Pointers do not repeat file
labels. Tiny successful reads retain their source text rather than longer duplicate coaching;
duplicate accounting and retry barriers still apply.
After a successful fresh reread, the pointer identifies unchanged source rather than a failed read
or stale snapshot. It directs the model to reuse visible text without shell/artifact rereads, and
to finish only when acceptance is verified and no blockers remain. This does not release retry
barriers or prevent a legitimate read after a source change or loss of the earlier full result.

For explicit native text selections, parent and worker read dispatch can reuse complete rows
before invoking the native read renderer. Coverage is turn-local and requires a matching full
raw-file SHA-256 plus an original result still visible in full. Fully covered selections return
an evidence link; partial overlaps read only the missing ranges. The structured `read_reuse`
receipt identifies the source hash, covered ranges and their original call IDs, executed ranges,
and whether the renderer was skipped entirely. Hash validation still reads the source; reuse is
not a fresh observation or passing verification check. If an external edit races a partial read,
Dial reads the original selection of the new version rather than combining versions.

This optimization does not infer containment between different searches or shell commands.
Raw/outline/document/notebook reads (including aliases), multi-path requests, implicit structural
summaries, unavailable identities, and calls with tool hooks keep ordinary execution. Missing or
clipped rows, presentation-reduced results and pruned evidence never establish coverage. Lost
edit witnesses also force a real read, so reuse cannot obstruct edit recovery. Exact read-only
batch deduplication and input-validated check reuse retain their existing owners and authorization rules; no discovery
quota or limit on legitimate verification is introduced.

Busy input defaults to **Let Dial decide**: at the next safe request boundary, a bounded auxiliary model request classifies the exact user message as steering or independent work. Changes of approach, corrections, dependent or overlapping work, and ambiguity steer the current task through its existing steering journal. Clearly independent requests start a background worker in an isolated project copy using the existing task authorization, spawn hooks, concurrency limits and result delivery. Splitting leaves the parent's current instruction and completed inference intact. Worker findings are not new user instructions; any copy integration follows the existing worker workflow. Routing failures, invalid decisions or failed admission visibly fall back to steering; cancellation restores unhandled input in order. When spawning is unavailable, steering is the only admissible route and no classifier request is made. Steering discards queued advice about the previous task's failures, repetition and completion, preserving protocol data, worker findings and safety boundaries. **Queue after this turn** remains an explicit opt-out. Automatic input does not cancel in-flight inference or tools while awaiting a decision, so a long operation can delay routing. No classifier is run when there is no pending input.

The busy-message chooser also offers explicit **Steer now** and **Independent worker** routes.
Steer now bypasses classification and cancels stale inference through the existing steering path;
started edits and tools retain their effects and only interruptible waits are stopped. The composer
shows pending, received, then applied after the steering journal accepts the direction. If the turn
ends before application, the direction is preserved as follow-up input. Independent worker bypasses
classification and starts at once, during streaming or tool execution, without changing the foreground instruction; failed admission preserves the message
as follow-up input rather than silently steering the foreground task. These choices do not grant
worker permissions or bypass isolation, approval, or concurrency limits.

Any routable queued message (not only Steer now) also stops the current wait early: `task wait`,
`side_request wait`, `bg wait`, and a foreground `bash` command that has not yet moved to the
background. The awaited work keeps running and its result still arrives at a later request
boundary; the wait reports that it stopped for a new message, and the message is routed at the
next step boundary. Input deferred until after the turn does not stop waits.

### Event-driven continuation

When a live foreground agent yields without tool calls, or requests `finish completed`, Dial
suspends inference if session-owned completion-blocking work is still running. This includes
shell/eval work adopted by `bg`, live workers and tool-free side questions. The next model
request waits for a terminal result or worker message, new routable user input, cancellation,
or a turn deadline/cost boundary. The final step-limit wrap-up request is never suspended.
Explicit blockers and requests for human input still end normally. Hermetic evaluation keeps
its existing step behavior.

The wait uses the existing platform wake channel. Background output/exit, worker control
events and side-question completion notify it; registration precedes state inspection to avoid
losing a completion between checking and sleeping. Spurious wakes and ordinary progress do not
trigger inference. A local check at most every 250 ms keeps legacy host input, UI heartbeats,
approvals and budget checks responsive; it does not send model polling requests. Explicit
`task wait` and `side_request wait` also use this wake channel and preserve their timeout
meanings. `bg wait` retains its existing event-driven exit/regex/readiness conditions.

Completion only resumes the existing turn: results keep their ordinary delivery and trust
framing, and must be incorporated before completion is accepted. Waiting is not verification
or proof that a child succeeded. New input is handled by ordinary step admission, not consumed
by the wait. Explicit `bg start` services do not block task completion. Waits are process-local,
do not grant permissions, and are not automatically replayed after session restart.
These runtime guarantees do not establish measured token or latency savings with live models.

Live-steering receipt settlement keeps its original acknowledgement deadlines but polls while
servicing heartbeats, workers, aborts and newer steering. Replanning journals the accepted prefix
and uncertain suffix once, in submission order, before later input. Cancellation is not rollback
and does not replay completed tools.

Explicit evaluation/learning runs capture the actual inference request and target. Normal coding launches disable trajectory and recovery-sample recording, learned candidates, automatic Laya/learned-skill capture, project-learning updates and startup evaluations without rewriting saved learning preferences. When exporting a verified example to the training template, valid tool-result error flags are removed as execution metadata while the evidence text is preserved. Malformed flags and unknown fields remain rejectable. An HTTP 200 response to a WebSocket upgrade establishes SSE-only transport for that session, so an unsupported upgrade is not retried at every request.

Stop hooks can resume a completed candidate within their existing three-round bound. Every resumed candidate returns through focused verification before another stop-hook decision; hook resumptions do not reset the repair allowance or turn step budget. Persistent blocking ends with a blocked outcome, and a hook cannot restart a turn that needs user input, has failed, has unknown effects, or was interrupted. Rejected completion reports keep the candidate work summary separately labelled beneath the final verification or hook diagnostic, so callers retain useful work details without mistaking them for accepted completion.

## Evidence-driven verification and escalation

Focused checks distinguish application failures from unavailable test infrastructure.
Recognized missing selected toolchains and offline Cargo dependency-cache failures are
environment-unavailable, never passing checks; compiler errors, assertions and missing
application imports remain code failures. A command that executes zero tests supplies
no passing behavioral evidence, including in the persisted behavior-check cache.
Round settlement retains verification statuses and bounded receipt history.
New-turn admission clears checkpoint association and current checks before validation or prompt
hooks can fail. A rejected prompt therefore cannot inherit the preceding turn's changes, checks or
code-history label. Opt-in historical verification receipts remain separate and still require
freshness validation before reuse.

Optional [requirement evidence contracts](requirement-verification.md) map operator-declared
requirements to configured checks and current rendered pixels. Their coverage is separate from
the execution outcome and never implies undeclared behaviors are covered. Parent and worker
settlement provide cause-specific safe next actions without automatic installation, permission
bypasses or replay of uncertain effects.

Tool failures carry a single recovery-state summary from shared parent/worker settlement:
the observed tool status, effect certainty, count of recorded committed paths, retained output
coverage, and the cause-specific next action. A typed recovery value is derived from execution
facts; the same decision is rendered in model-visible text because provider adapters can omit
internal annotations. It distinguishes correction before retry, inspection before retry, waiting
for user direction after interruption, and respecting a withheld permission/policy boundary.
Zero recorded paths does not assert that nothing changed; interrupted output remains partial
evidence. The original diagnostic stays intact. These summaries neither replay operations nor
grant permission. Unknown effects still stop automatic continuation, and earlier completed work
is not rolled back by a later failure.

An executed failed tool result immediately arms an execution-side **failed-operation retry barrier** in
the shared parent/worker engine. The same tool with the same normalized arguments is refused
before execution; changing a call ID, intent annotation or bash timeout does not evade it.
Corrected arguments and different diagnostic calls remain available. Todo updates, compaction
and automatic/directed continuation do not release this barrier, and neither do successful reads,
with one exception: a successful `read` of a file that a failed `edit` or `write` targeted
releases that call, because the read supplies the line witnesses the call lacked (for example a
tag from an earlier session). When directory access is approved, a `read` that failed at the
workspace access boundary may be retried if the workspace now resolves its target. This does not
release unrelated failures or bypass tool approvals and protected-storage restrictions. A retry
that fails again re-arms the barrier. A successful novel committed source mutation releases only
failed file operations that depend on a changed path; unrelated file failures and access barriers
stay withheld. Operations without known file inputs (including arbitrary shell commands) retain
the whole-workspace mutation/tree-change fallback, not guessed command dependencies. New steering
or coordination starts a new recovery context. Failed post-commit operations retain their effects
but do not release the barrier. This is normalized-call guarding, not semantic equivalence checking:
changed scripts can still repeat the same underlying mistake.
Already-started parallel operations keep their real results; the guard does not retroactively
claim they were unexecuted. Successful results containing background warnings (for example,
Vue Router missing-route warnings) do not become failed executions by text matching.
Typed preflight, schema and permission refusals do not arm it: they were not executed, and
their existing admission requirements still govern the next call.
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

Four verification aids are always on (not experimental):

- **Caller obligations.** When a turn changes the header of a named declaration (function,
  method, type), up to 20 files that mention that name as a whole word and were neither read nor
  edited this turn are listed under `caller_obligations` in completion evidence. Mentions are
  lexical, not resolved symbols, so the list is a review prompt, not proof that a caller is broken.
  Short or very common names are skipped. Under `verification.strict_requirements`, open
  obligations block completion; hermetic runs skip the scan.
- **Requested run targets.** A request that starts with a run verb and names an existing
  workspace path is not completed until a run naming that path was observed to stay up or exit
  zero with no later effect after its launch; see
  [requirement-verification.md](requirement-verification.md#requested-run-targets).
- **Empty-search memory and declaration recovery.** A local `grep` or `glob` with no matches
  names up to three earlier empty searches in an overlapping scope. Two distinct empty patterns
  against the same explicit file queue one advisory declaration-read correction, rather than
  encouraging another representation guess. Intentional absence checks remain valid and are
  never refused. The correction uses the next request's actual call policy: only batch-permitted
  requests suggest gathering independent evidence together. It reuses ordinary advisory priority,
  delivery/retry tracking and repeat suppression; safety and failure guidance take precedence.
  Reading the targeted file retires its earlier probes. Workspace changes clear the memory.
  Paged, remote, incomplete and failed native searches do not count. A literal single-file
  `grep -c` with an attributable zero count and exit 1 also counts; ambiguous compound shell
  scripts and pipelines are deliberately not classified as empty searches.
- **Request call policy.** Each foreground request records `tool_call_policy` in the existing
  workflow event stream and saves `reliability.call_policy` in session control and the existing
  request-policy history. `/reliability` shows `batch` or `one_call` and its causes: declared model
  capability, saved reliability evidence, contract-failure recovery, or a small local tier without
  passing batch evidence. No one-call constraint is not a promise that arbitrary shell commands
  execute concurrently. The shell output-trimming hint no longer gives policy-blind batching
  advice. These are diagnostics and advisory recovery, not changes to approvals or scheduling.
  The regression suite is `examples/benchmarks/registry-discovery.toml`, run through the existing
  cross-agent runner for joint answer and ordered-trace grading. See
  [`registry-discovery.md`](../examples/benchmarks/registry-discovery.md) for offline commands and
  the optional live-model tier; offline passes do not establish model-quality improvements.
- **Worker evidence audit.** A delegated job's handoff envelope gains `evidence_audit`: each cited
  `path[:line[-end]]` reference is checked in the parent workspace as `valid`, `stale` (the file
  exists but the lines do not), `invalid` or `unparsed`. An acceptance item reported `met` whose
  every cited reference is invalid is downgraded to `not_verified` (`cited_evidence_invalid`).
  This checks existence only, not content.

## Shared execution ownership

`agent/engine.rs` owns the lifecycle driver used by parent and worker adapters, result settlement, output-length recovery, repetition checkpoints, unchanged-read reuse, narration progress, reply recovery, committed-content edit-loop steering, and the uncertain-effect stop boundary. `tools/execution.rs` owns native dispatch, pre-start cancellation, panic containment, and callback effect propagation. A callback tool cannot hide an uncertain nested result by catching its error or returning success; subsequent callbacks are refused.

`agent/turn.rs` is the parent adapter for durable session state, live steering and learning policy. `agent/child.rs` is the worker adapter for private history, assignment permissions and worker request budgets. Both adapters advance the same lifecycle driver and share inference transport, authorization helpers and the parallel scheduler. Their preparation and persistence callbacks retain explicit role responsibilities. New common execution policy belongs in the shared engine, not copied into both adapters. `agent/tool_runner.rs` handles foreground UI events and routes callbacks back to their owner.

Read-batch admission applies recovery restrictions to every candidate, not just the first call.
The presence of pre-tool hooks conservatively routes calls through serial admission so rewritten
arguments can be validated and authorized before execution; hook-free reads remain parallel, and post-tool-only
hooks do not disable concurrency. Parent and worker hook rewrites both recheck their executable
schema and path authority. Mutable authorization and recovery decisions are never cached for
an entire turn.

Parallel dispatch publishes each completed call and its timing immediately, correlated by call ID
even when several calls use the same tool name. The return vector and persisted provider-facing
settlement retain original call order. Presentation does not change schema/approval admission,
hook authorization, effect accounting or journal-before-publication requirements.

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
| Terminal backpressure | `dial-ui` input/output pumps and frame pacing own bounded transport, paint admission and scrollback work; refused frames remain pending. |
| Streaming presentation | Transcript reveal and call-ID argument previews are observational; incomplete arguments never enter tool execution. |
| Side conversations | App side controller owns independent requests; storage owns checkpoints and session-bound sidecar lifecycle. |
| Worker controls | Live runtime incarnation, retained worker editors and per-request settings revisions; pools retain only compatible logical backend/history state. |
| Extension forms | SDK bounded declarative panels and app native dialogs, with typed outcomes and fail-closed generation handling. |
| Identity matching | `llm/identity.rs` shares declarative scoped axes without widening unrelated detector/provider match sets. |
| Lifecycle regression gate | CI runs all four real-terminal probes in `examples/benchmarks/` against the built binary and local scripted providers. |

The terminal gates exercise resize, cancellation, busy-turn commands, file review, retained image
pixels after deletion, SIGKILL/reopen without replaying a completed write, worker restoration and
focused controls, pool conversation reuse, draft-safe questions, side-stream recovery and lifecycle,
concurrent-writer forks, native tree previews, extension panel submission/cancellation, quiet-stream
reveal and incomplete write previews. Captures are terminal text, not pixel screenshots. These
fixtures establish behavior, not real-model quality, cache savings or relative harness performance.
Provider-backed performance measurements belong in the efficiency benchmark described in
[benchmarks.md](benchmarks.md#measuring-efficiency).

Execution metadata stays separate from tool payload and display text. Document transactions report actual committed paths; progress guards never parse diff prose. Presentation hooks cannot clear execution failures or effect uncertainty. Configured subprocess tools share cancellation and started-process uncertainty handling.

## Coding workflow

The [prompt and core-behavior index](../crates/dial-agent/src/agent/prompts/README.md)
is the navigation entry point for instruction changes. Shared built-in wording lives in
`agent/prompts/{system,compaction,recovery,modes}`; runtime owners still select and assemble
it and enforce permissions, budgets and completion. Existing operator overrides and public
instruction paths are preserved. Tool-specific contracts and domain prompt renderers stay
with their owners and are linked from that index.

Dial's default system guidance is Dial's own design: engineering principles, tool policy, scope/research/implementation/verification, and delivery.
It reuses current evidence, groups coherent edits, discourages repeated baseline checks and
routine narration, and verifies after the change rather than after each micro-edit. Dial retains
its own approvals and untrusted-context boundaries.
Custom operator guidance, fixed-prompt overrides and appended prompts remain supported.
The built-in policy is shared across models, with no model-family prompt forks. Identical
write, reporting and role-boundary rules are compile-time fragments in `prompts/system.rs`;
engineering, trust and focused execution also have single shared sources. Existing lean,
one-call, worked-example, concise-reporting, capability-scoping and delegation-preference
adjustments are indexed with their selection and assembly owners in the prompt README.
The shared-fragment and explicit workflow-composition refactors preserve prompt bytes. Separate
wording changes make the active request contract override batching and todo pairing (including
reliability recovery on full-tier routes), and make verification follow the selected delegation
preference and existing check ownership rather than mere availability of delegation. Tool gating,
context-overflow fallback, escalation and request enforcement are unchanged. New model-specific
wording requires measured outcome differences, not a family-name assumption.

Custom templates deduplicate complete normalized lines/blocks, including note bullets, rather
than embedded substrings. Admitted notes are filtered individually, including when only some
notes are rendered. Transformed or inline-wrapped values may be repeated conservatively; operator
templates, replacement prompts and appended guidance remain supported. These selection and wording
changes have assembly coverage, not measured model-quality or performance gains. Escalation decay
or success-driven step-down remains deferred pending an explicit policy and outcome evidence.
Prompt assembly tests and scripted harness checks do not establish reduced looping in a model;
use the [efficiency benchmark](benchmarks.md#measuring-efficiency) for live-model comparisons. Rebuild/restart
Dial to use source changes; existing custom/admitted operator guidance is not overwritten.

The prompt/orchestration audit added regression coverage for independent browser/server tool
offers across full, lean, concise and one-call profiles; delivered-note retirement across requests;
reserved-summary zero dispatch; three-window summary folding; and failed/denied file-operation
provenance. These establish request assembly and runtime admission behavior, not model reasoning
quality. For model comparisons, hold model, tool permissions, fixtures and budgets constant and
repeat the existing `efficiency.toml` and `tool-effectiveness.toml` suites. Gate on independent
task outcomes and preserved constraints before comparing latency, tokens, failures or redundant
calls. Do not optimize call counts at the expense of correctness or infer a quality improvement
from prompt length alone.

The centralized-policy review keeps guidance proportional to the task: one-call mode
limits tool use rather than requiring it, and recovery can end in a direct answer or a
concrete blocker without manufacturing work. Repeated-call detection establishes call
identity, not equal results or unchanged external state. Its notices identify quoted data
as untrusted and render it with native Rust formatting, so placeholder-looking content
in a tool name, argument, call ID or result is not recursively substituted. Reused results
are explicitly not fresh observations. These wording changes do not relax duplicate-call
admission, approvals, retry caps or verification requirements.

Optional planning/goal prompts should reuse current evidence, scale plans to actual
deliverables, permit legitimate regression-test changes without weakening acceptance,
and stop honestly for missing permission or prerequisites. Summary prompts preserve
failures, uncertainty, unresolved requests and approval restrictions while discarding
obsolete detail; a summary or worker claim is not promoted into verified evidence.
There is no extra prompt framework or model call in this path. Behavioral tests cover
assembly and literal evidence preservation; real-model outcome/cost improvements still
require the controlled comparisons above.

## Evolving intent and goal progress

For foreground turns, only admitted user turns and steering update the current instruction.
Goal continuation, budget, peer and monitor messages do not become user requests. Session control
records original user text (including pre-draft text), its journal entry ID and whether it
arrived as a turn or steering. Slash-goal commands submit the unescaped objective/idea (or the
resume/budget command); rendered goal and interview guidance attaches as ambient context, never
as the original user text. The latest original is exact; earlier context retains at most eight
user messages within 16 KiB and explicitly counts omitted messages. Session history remains
the recovery source, not an inferred or model-rewritten task contract. Saved legacy effective
instructions have unknown provenance (they may be coordination text) and stay separate from
admitted user messages. Oversized legacy context is explicitly omitted with its journal reference.

Goal continuations carry this provenance beside the stored objective, deduplicating unchanged
notes against visible history and reattaching them after compaction removes them. Guidance distinguishes
refinements from replacements, cancellations and separate tasks; compatible constraints remain,
but a historical objective cannot override an explicit newer request. Material ambiguity calls
for focused clarification, not an interview for every ordinary request. No semantic classifier
or additional model call decides what the user meant. Critical compaction and notes-backed
context rollover carry the same runtime-owned provenance separately from model working state;
the existing critical-carry size gate still fails closed. Identical latest original/effective text
is stored once with a lossless in-contract JSON pointer; distinct drafts and originals stay separate.
Fresh worker handoffs carry a bounded snapshot with explicit overflow and a request to recover
omitted context from the parent.

For substantial work, optional `working_state.deliverables` records concrete outcomes, status
and evidence references. Notes-only `update` preserves these obligations; `patch` preserves every
omitted field. Declaring an obligation records the current runtime user-message provenance without
claiming that the model captured the whole request. Removing, cancelling or redefining one requires
an explicit human scope confirmation through the existing question surface. Refusal, timeout or an
unattended surface leaves the whole update unchanged. Confirmed redefinitions reopen the obligation
and discard old evidence; legacy cancellations without a runtime receipt still block completion.
`/contract <task>` starts a turn that asks the model to record the task as a contract first:
`goal`, `constraints`, and one `open` deliverable per observable "done when" condition. It asks
about ambiguity before starting, and if blocked it stops, records what it tried and marks the
deliverable `blocked`. It adds no new gate: the existing deliverable and evidence rules below
decide completion.
Simple answers need no declaration. A `satisfied` record requires `evidence_refs`, but is still a
model claim. Explicit `check:<name>` references must resolve exactly one existing selected/configured
check with a current passing observation; missing, stale, failing, unavailable or unknown links block
completion. Resolution does not execute checks or grant permission. File/artifact refs do not certify
freshness, coverage or correctness. Completion evidence and the existing advisor review reconcile
each declared outcome with its claimed status, current linked-check results and unverified references;
undeclared requirements remain unassessed. Existing check and strict-requirement gates remain authoritative.

Goal-mode boundaries compare recorded deliverables, findings/blockers, open work, claims and
controller-observed check statuses, ignoring working-state revision churn. An unchanged boundary
adds advisory feedback to identify a concrete remaining gap. This is not an extra stop rule:
unrecorded observations and edits may still matter, changed bookkeeping is not proof of progress,
and the existing activity/stall, blocker and budget limits are unchanged. The recovery ladder's
successful-tool signal remains operational recovery telemetry, not goal satisfaction.

These are provenance/completion correctness changes with deterministic harness coverage, not
a measured improvement in model reasoning, completion rate, token use or latency. Evaluate the
existing opt-in drafter before expanding or enabling it: compare matched model/thinking,
permissions, fixtures, state isolation and budgets, with repeated runs for both agents. Include
multi-turn cases that (1) refine one deliverable while retaining a denylist, (2) replace a request,
(3) cancel work mid-turn, (4) ask an unrelated question during a goal, (5) resume after compaction,
and (6) produce successful but irrelevant tool activity. Score full deliverable satisfaction,
constraint preservation, evidence freshness, premature completion and correct stopping before
comparing cost. Scripted doubles test runtime behavior, not these live-model outcomes.

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
credit. Conversely, a pass before a later effect is stale and cannot earn final-revision credit,
except where operator-declared dependency-aware reuse (below) proves the check's inputs unchanged.
Bounded exact diagnostic fingerprints distinguish an unchanged nonbehavioral baseline failure
from new diagnostics. Unchanged unrelated failures stay disclosed but do not request automatic
scope expansion; changed/absent/oversized fingerprints remain conservative. Declared behavioral
checks and registered reproductions remain actionable. Configured runner declarations and opt-in
strict outcome settlement/selective review are described in [requirement-verification.md](requirement-verification.md).
Recognized manually executed checks are bound after their own call settles. Cancelled, unavailable,
zero-test, or incomplete-process results never supply passing credit.
Semantic result classification uses complete presentation only when marked exhaustive; otherwise
it reads the complete retained artifact through the bounded byte API (at most 1 MiB). Missing,
partially retained, oversized or unavailable raw evidence stays unverified rather than earning test
credit from clipped/minimized text. Ordinary exit-code checks still need no positive test count.
Subset verification reconciles obligations by command: passing B cannot erase a retained failure
of A; only a current successful execution or valid declared-input reuse of A clears it.

**Dependency-aware verification reuse.** Reuse is enabled by default for the exact built-in
isolated Python AST check on recognized Unix CPython installations. Dial tracks the source,
environment and installed runtime identity; unsupported layouts and unknown dependencies fall
back to execution (see [configuration.md](configuration.md) for limits and host-runtime assumptions).
The runtime identity is observed afresh before and after each check run; one revalidation pass
observes it once for all of its checks. Its files are identified by metadata alone (no content is
read), so capturing a stdlib of about 8,500 entries costs tens of milliseconds.
Other built-in checks remain conservative. Operators can enable reuse for configured
`[[verification.commands]]` checks with `[verification.inputs."check-name"]` (`paths`, optional
`environment`). This explicitly asserts that the check is deterministic and side-effect free,
and that the listed paths are its complete workspace inputs. Directories are hashed recursively,
including ignored files; symlinks and special files are unsupported. A configured command without
a declaration is never reused. This does not cache arbitrary shell commands.

Reuse is turn-local and applies only to a prior passing, controller-run result of the same check with
the exact same command and configuration. Dial takes fresh bounded snapshots of the supported or
declared inputs before and after the earlier run and compares them with the
current state; reuse also requires that no shell or extension effect epoch changed since that
result. Reproductions (`verify` `repro`) are never reused. Any missing, unreadable, oversized or
changed input disables reuse for that check and the real check runs; a failed snapshot never
suppresses a check. A reused result is explicitly labeled reused with `ran: false`, keeps the
original observation revision, and makes no extra model call. A manual `verify` naming `targets` always
executes; the default `verify` and the completion gate may reuse. Toolchains and external resources
must be stable: do not declare checks that depend on network, time, randomness or other inputs you
cannot list. This covers verification only; other subsystems do not yet use it.

`verify` `repro` registers a model-written reproduction (name plus shell command, at most four per
turn, same name replaces and clears its history) and runs it at once. Once the turn has changes, a
registration without `targets` also runs the rest of the gate's selection in the same call, so one
call reports every applicable check; before any change, or with `targets`, only the named checks run.
It is selected by the completion gate like any focused check and is lost at turn end. Its command is untrusted model
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
A post-fix pass without a pre-fix failure also explains that repeated reads or unchanged reruns
cannot establish that missing history. The model must not undo the fix to manufacture credit;
it reports any required fail-then-pass evidence gap and still satisfies remaining acceptance checks.
Registration validates every requested check name before changing reproduction state. A rejected
selection preserves the previous reproduction, its observations and retained failure obligations.
An empty `targets` array without an explicit `repro` is rejected: it cannot establish a passing
result or prediction. Omit `targets` to select all applicable checks, or name a nonempty subset.
Do not combine `repro` with the `failed` target: registration must run the initial attempt, whereas
`failed` selects previously observed code failures. Register first, then rerun with `targets`
(check names, test files or `file::test`, or `failed`). Unresolvable targets are errors even when
no focused checks currently apply.
Executable repair records use that same classified, typed execution evidence, not printed exit
markers. Unverified runs invalidate a matching failure baseline rather than pairing an old failure
with a later pass. Related-test planning reuses JS package/runner metadata and Go directory scans
only within one planning invocation; later invocations observe changed manifests and tests.
Behavioral provenance still hashes fresh bounded source state, but no longer loads dependency
outlines that its record does not use.

Automatic post-edit related-source hints are best effort, not part of edit settlement or verification.
Each cache owns at most one hint worker, using an independent read-only database connection;
busy admission skips optional work. Turns poll without waiting and cancel pending hints on effects,
new steering or turn exit. Ready hints are revision-bound advisory notes for a later request, not
edit witnesses. Automatic hints use source-validated lexical neighbors; explicit impact retrieval
retains the richer saved-architecture context. Mandatory pre-edit source checks are unchanged.
Approval-denied writes are not treated as successful verification. `todo` is optional progress bookkeeping, not a verification checkpoint. `done` closes the current item and starts the next; findings and references are optional notes, including for legacy investigation entries. No separate tool-activity, citation or text-delivery gate applies. Final prose never closes execution steps; the model must explicitly settle them. Open execution obligations, explicit blockers and running workers remain completion prerequisites. Failed checks remain failures independently of todo state.

Todo-only updates receive a one-time batching reminder only when the active request allows
multiple calls. The small local tier's one-call contract, and any later one-call reliability
recovery, permit a standalone todo update without contradictory advice to pair it with real work.
This does not force a plan or change which tools are eligible.

Plain final replies and `finish` use the shared state gate and final configured verification/repair
pipeline. Missing work and post-edit evidence get at most one instruction-scoped corrective nudge;
repeated rejection remains incomplete with its candidate and diagnostic. Empty/malformed replies
have separate bounded transport recovery. Invalid todo arguments retain their correction and
recovery limits; neither bookkeeping nor an inner-loop rebuild renews completion recovery.
Interruption and uncertain effects never grant continuation.

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
Explicit requests for visual inspection or verification also mark visual evidence as required; a
capture counts only after its pixels are delivered to a subsequent model request. This records
observation, not proof that the surface is correct.

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

- Ordinary hosted and local routes default to a stable core under 12 KiB of serialized schemas.
  Core priority is `tool_search`, `tool_load`, `finish`, `read`, `edit`, `write`, `bash`, `grep`,
  `glob`, `todo`, `verify`; eligibility, the byte ceiling and `initial_tool_limit` can narrow it.
  Lean local routes retain their eight-tool count limit. MCP, memory and specialist tools load
  only through explicit `tool_load`; ordinary searches, inferred task routing, argument recovery
  and missing-capability hints do not change the default offer. Search/listing still reports
  readiness and the exact loading action without executing tools. Explicit pins survive resume.
  The cached system band uses generic schema authority rather than enumerating offered or
  loadable tools. Built-in loading preserves that policy band; explicit MCP loads may add bounded
  server instructions. Mode changes,
  configured policy/profile changes and explicit loads can still change request bytes; provider
  cache hits must be measured. Set `assistance.compact_tools=false` to retain the older automatic
  discovery/routing behavior. Pinned harnesses and sealed evaluations keep their explicit offers;
  schema loading never widens allowlists, sandbox boundaries or approval authority.
- Local model profile (`[assistance.local]`) applies only to locally served main models (the same
  routes as `lean_local`). Hosted routes and hermetic runs are unchanged, and explicit pins win
  over every local-profile policy below. Its keys are `enabled` (default `true`), `small_window`
  (default 16384; 0 disables the small tier, otherwise 1024..=1048576), `edit_mode`, `sampling`,
  `reasoning_share` (default 50), `escalate_after` (default 2), and the keys below.
  - Tiers: a lean local route whose planned window is at most `small_window` gets the small tier:
    a fixed minimal tool set (`read`, `write`, `edit`, `bash`, `grep`, `glob`, `todo`, `finish`, `verify`,
    `tool_search`, `tool_load`) plus explicitly loaded tools; everything else loads on demand. A
    pinned `[harness] tools` list keeps its own offer. A lean route above that window gets the lean
    tier (the `lean_local` profile above). Other routes get the full tier.
  - Small-tier calls: the small tier works one tool call per response through the reliability
    one-call interface (`parallel_tool_calls: false` where the protocol accepts it, a per-request
    one-call contract, and a batch of several calls rejected unexecuted), and its prompt says one
    call per response instead of asking for batches. A saved `dial reliability check` record for
    this exact configuration keeps batching only when its two-call batch fixture and all recorded
    fixture cases pass; `reliability.enabled = false` turns the one-call default off. The record is
    read once at each turn boundary, so the prompt and contract use the same snapshot and a record
    replaced or invalidated during a turn takes effect only on the next turn. Binding includes the
    configured context window and sampling policy. Fixture success is a contract/execution
    measurement, not a coding-quality score or proof that a larger offer is optimal.
    This small-tier default and its passing-batch exception are independent of the broader
    `measured_interfaces` opt-in. Base-model fixture evidence is not reused with LoRA or steering.
  - Worked example: when the small tier uses its one-call default, built-in guidance ends with one
    compact example loop (targeted `grep`, ranged `read`, one `edit` in the active grammar's argument
    shape, an edit with `verify` when offered or otherwise a focused `bash` check, then a short answer). It is fixed
    per grammar and offer. Passing saved batch evidence or disabled reliability omits it, as do
    missing required capabilities, custom templates and replacement system prompts.
  - Prompt scope: tool-specific sentences of the fixed guidance (`skill`, saved project maps for
    `codebase_memory`, the LSP) appear only when that tool is offered, like the preview and
    delegation sections.
  - Edit mode: local routes default to the `replace` grammar. Precedence is the `--edit-mode`
    override, the model profile `edit_mode`, `[harness] edit_mode`, `[edit] model_variants`, then
    `assistance.local.edit_mode`, then `[edit] mode`. `inherit` makes the local default follow
    `[edit] mode`. Hermetic evaluation ignores the local default. When the local default replaces an
    explicitly set `[edit] mode`, a one-time notice per session names both modes and `inherit`.
  - Sampling: `model` (default) leaves externally hosted local models to their serving host's API
    defaults, including LM Studio and Ollama. Dial omits `temperature` instead of substituting a
    learned harness value or guessing the host's numeric default. An explicitly configured
    `[harness] temperature` (including 0.2), a model-profile temperature, or enabled adaptive
    sampling/pinning wins. A thinking switch alone does not pin unrelated sampling settings.
    `harness` opts into Dial's harness temperature. This host-default policy also applies when the
    local assistance profile is disabled. Dial's own runtime (no endpoint) still uses shipped
    `generation_config.json` / GGUF sampling metadata when available.
  - Output allowance: an unlimited/unset harness `max_tokens` (0), with no model-profile limit,
    omits `max_tokens` / `max_completion_tokens` (OpenAI-compatible hosts) or `num_predict`
    (Ollama), letting the host choose. Explicit Dial limits and bounded recovery requests remain
    explicit. Dial's internal planning allowance is not a host configuration change.
    Independent workers use the same policy at each request boundary, including after a worker
    model switch; finite role/task/recovery output budgets remain explicit.
  - Reasoning share: on lean tiers, streamed reasoning per request is capped at `reasoning_share`
    percent of the planned window, at about 4 bytes per token. The cap applies only when
    `reasoning_guard.max_reasoning_bytes` is 0 (unlimited) or larger than that cap; a stricter
    explicit limit still applies. When the cap is hit, the existing reasoning guard discards that
    generation and retries once; requests to Dial's own runtime also carry the `dial_reasoning`
    budget (3/4 of the cap). 0 disables it.
  - Escalation: `escalate_after` failing rounds (malformed tool calls, or failed `edit`
    or `write` calls) step the session up. Step 1 switches to the minimal tool set and lean prompt;
    step 2 switches to the `replace` edit grammar unless a pin chooses otherwise (it is skipped when
    the grammar is pinned or already `replace`). When the latest edit or write failed, one successful
    intervening round preserves the streak; a second clears it. A successful edit or write allows the
    next successful round to clear it. This is not tracked per file. A reached step takes effect at
    the next model-request boundary, even within the same user turn, after settled calls. The prompt,
    tool offer and edit grammar refresh together without replaying effects or resetting the turn's
    budgets. The reached step is remembered per model in `local-profile.json` in the user directory
    and applied at the first turn of the next session.
    A llama.cpp server whose `/props` reports that the model's chat template cannot render tools
    natively (`chat_template_caps`) starts every session at step 1 instead of waiting for failures;
    that step comes from the server, not from the record, so it is not saved. Ollama (`capabilities` without `tools`) and LM Studio (a `capabilities` list without `tool_use`) start there the same way. When replies come back with tool calls as plain text twice in a session, a notice names the parser setting for the detected server (vLLM `--enable-auto-tool-choice --tool-call-parser`, SGLang `--tool-call-parser`, llama-server `--jinja`); the count is also kept in `local-profile.json`. vLLM and SGLang are recognised from `owned_by` in `/v1/models`, receive the thinking switch and penalties, and their `max_model_len` is the planned window. 0 disables escalation.
    With `reliability.enabled` and `reliability.measured_interfaces` enabled, a bound fixture record
    with a failed native-tool case (including a call recovered from plain text) also seeds step 1,
    even above the small-window threshold. It does not select another edit grammar or override pins.
  - Route diagnostics: `dial reliability` separates declared capabilities, server-reported facts,
    saved fixture measurements and the selected interface. Configured context and sampling are
    policy, not measurements; effective context is shown only when a session supplies it. The CLI
    may probe local-server metadata, never inference. `/reliability` uses the turn's existing
    snapshot without disk reads or server probes; an unmeasured CLI interface is unknown, not a
    guessed capability score.
  - Benchmark lever: `ablate-local_profile` in `dial benchmark --ablate-assistance` turns the
    profile off (`assistance.local.enabled = false`) against the `dial` row.
  - Thinking (`assistance.local.thinking`, default `model`): leaves thinking to the model, server
    and explicit model profile, without automatic phase switching on main or side requests, or
    disabling thinking during loop-penalty retries. Set `phase` to opt into reasoning on a turn's
    first request, after failed tool rounds or a successful `todo` update, and off after successful
    tool rounds or intercepted generations. This opt-in requires a server thinking switch
    (llama.cpp and MLX-LM: `chat_template_kwargs.enable_thinking`; MLX-VLM: top-level
    `enable_thinking`; Ollama: `think`).
    LM Studio documents no request-level switch, so nothing is sent there. An explicit model-profile
    `thinking`, `reasoning_effort`, `thinking_budget` or `chat_template_kwargs.enable_thinking`
    always wins.
  - Model residency (`keep_loaded_secs`, default 0 = server default, max 86400): by default Dial
    neither extends the host's residency time nor unloads its model at session exit. A positive
    value opts into keeping it loaded after requests (Ollama `keep_alive`, LM Studio `ttl` for
    just-in-time loaded models). An explicit model-profile `keep_alive` wins. With that opt-in,
    an Ollama model this session loaded can be unloaded at exit; an already loaded model is left alone.
  - Prompt cache (`prompt_cache`, default off, llama.cpp only): leaves cache/slot selection to
    the server unless enabled. When enabled, requests send `cache_prompt` and an `id_slot`.
    The main session uses slot 0; with multiple slots, side requests use the last slot and workers
    use the others. Prompt-cache hits reported by the server are still counted and shown in the
    status line (`N cached`) without an explicit cache override.
  - Side models (`side_models`, default `warn`): a side role (`small`, `coach`) assigned to a
    different model on the main model's local server makes the server swap models for summaries,
    ranking and edit repair. `warn` keeps the assignment and says so once per session, with the
    measured load time when known. `reuse_main` sends those side requests to the loaded main model
    (thinking off where switchable). `allow` keeps the assignment without a notice.
  - Context fit (`context_advice`, default on): the server's reported loaded window informs
    compaction and the context meter. Auto does not send Ollama `num_ctx`; an explicit
    `context_window`, `local_context_window`, or model-profile context limit still does. A memory
    estimate can produce advice, but never silently changes the host's loaded context or replaces
    its reported capacity. Missing loaded metadata remains a planning fallback, not a verified host limit.
  - Loop recovery (`loop_penalty`, default off): keeps host sampling unchanged unless enabled.
    When enabled, a repetition-triggered retry carries presence/repetition penalties of 0.5/1.1;
    explicit model-profile penalties win. Thinking remains governed independently by its setting.
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
- Ambient codebase retrieval is off by default (`codebase.ambient_context = true` opts in); the
  model can load `codebase_memory` for the complete view. It makes no model request: it
  selects task-relevant sources/findings whose cited sources still match their saved hashes, clips
  finding summaries (at most four, with source links), keeps only task-matching outline lines, omits
  the directory inventory, bounds stale notices to topic names, and states that the user's
  instructions take precedence and that the brief is not an edit grant. Automatic codebase and
  persistent-note lookup abstain on conversational follow-ups; longer queries require more than
  incidental word overlap. Architecture sections do not expand automatically just because they
  cite the same source. Explicit recall/navigation keeps its broader scope.
  Relevance is enforced for parent and child automatic codebase context even with efficiency disabled.
  Long prompts need several distinct matching terms unless they name a file explicitly; a single
  incidental path component is not sufficient to admit an entire subsystem.
- Tool results are shown whole up to the per-model limit (48 KiB for large windows, about an eighth
  of the window for small ones), with the complete output retained in artifacts.
  `efficiency.tool_preview_bytes` can reduce successful inspection results further; errors, read/edit
  snapshots, explicit artifact retrieval and unretained output are never reduced.
  Settled background pipe output and completion notifications also use the shell output minimizer
  when the full, previously unread capture fits the retained ring and has a complete artifact.
  Recognized Cargo progress and passing Rust test lines are omitted on success or failure while
  diagnostics, failing tests, totals and raw artifacts remain. Partial/delta, tail-selected and PTY
  results remain unfiltered; `shell.minimize_output` controls it.
  Homogeneous sequences of recognized Cargo checks can share the same filter; mixed arbitrary
  stdout, pipelines, and executable `cargo run`/`cargo bench` output remain unfiltered.
  Successful retained hashline edits show one fresh anchored representation to the model rather
  than repeating an equivalent diff. UI/raw evidence keep the diff; absent, stale, truncated,
  empty or otherwise non-equivalent projections fall back to the complete representation.
  Large Git status inventories show exact status counts and bounded path samples with the complete
  raw listing in an artifact; `git status` accepts the existing `path` filter for focused inspection.
  Skills can use author-maintained compact entrypoints and on-demand bundled references. Arbitrary
  instruction bodies are never silently summarized or truncated; oversized/invalid bodies refuse
  loading rather than supplying partial policy.
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
  Exact successful read-only duplicates (same call arguments and text, with retained originals)
  consolidate under pressure even below the pruning batch size; the newest copy remains in full.
  Short observation text and harness notices stay verbatim when a pointer would be larger.
  Observations are results of search/read tools (`read`, `grep`, `glob`, `find`, read-only `lsp`,
  `ast_search`, `dep_source`, web search/fetch) and the bounded read-only investigation tools
  (`data`, `database`, `document`, `network_inspect`, `runtime_inspect`, `timeline`,
  `evidence_search`, `audio`, `visual`, `video`); tools that also record state keep their results.
  Compaction still uses configured token/percentage/model/agent policy with output/reserve headroom.
  With `compaction.drop_useless`, an old artifact-backed read-only failure can condense only after
  a later exact tool-and-arguments retry has explicit successful metadata and known effects.
  The placeholder retains the failed attempt ID, matching success ID and diagnostic artifact.
  Recent or unresolved failures, mutations and uncertain effects remain intact; no shell/build
  resolution is inferred from prose or an unrelated later pass.
- Side-request answers and failures have one canonical persisted conversation message. Poll, wait
  and cancel results contain lifecycle/delivery references rather than repeating answer bodies.
  Reporting guidance answers the active question without repeating a delivered completion report;
  this is model guidance, not runtime censorship or a guarantee of concise prose.
- Very large windows are planned against `efficiency.history_token_cap` (default 262144) rather
  than their full size, because every step resends the whole history. With `threshold_*` on `auto`,
  the trigger is the planning window minus its reserve (15%, at least 16k): about 223k on a
  1M-token model instead of about 850k, and about 170k on a 200k model. Windows up to 65536
  tokens retain an additional 20% margin on usable space. Background summaries prepare before
  the trigger and are held until due; the response-headroom safety check is unchanged. The prefix
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

**Experimental workflows** are under `/settings` → **Experimental**. All default to off except
`repro_first` (`advise`); set `repro_first = "off"` to opt out of that advice. Fault witnessing is
opt-in, independent of normal verification. Enable or disable each independently:

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
- `experimental.fault_witness` (default off): opt in to comparing a passing configured command
  named in `verification.behavioral_checks` against this turn's checkpointed starting files in
  an isolated copy. Automatically discovered related suites and registered `check.repro`
  reproductions are excluded: reproductions already track their own fail-then-pass evidence.
  Only the first eligible passing command with checkpointed changes is compared. There is at
  most **one attempt per user turn**, including inconclusive results; later edits do not reset
  the budget. Its command deadline is the configured timeout capped at **60 seconds** (also
  60 seconds when the normal check has no deadline); copying the workspace is additional overhead.
  No per-suite `NOTICE` cards are emitted. The compact diagnostic is appended to the check output
  and retained under `counterfactual_checks` in completion evidence with its compared revision
  and timeout. It is historical diagnostic evidence, not a reusable pass for later edits.
  Passing without the edits shows no change sensitivity in that comparison; it does not invalidate
  normal verification or require another test. Failure is not automatically proof of the intended
  regression either (for example, reverted test files may no longer compile). Neither verdict
  earns observed pre-fix reproduction or repair credit.

  To opt in, explicitly enable `fault_witness = true` under `[experimental]` and declare a small,
  deterministic regression command by name in `[verification].behavioral_checks` and
  `[[verification.commands]]`. Merely configuring a command, or running all related suites, does
  not select it for comparison. Existing explicit `fault_witness = true` values remain enabled,
  but now follow these narrower selection and budget rules. No private settings are rewritten.
- `experimental.edit_bisection_runs` (0–16, 0 = off): when a check that passed earlier this turn
  fails after edits to two or more files, halves of those files are reverted to their content at
  the last pass in isolated copies to name the file whose change broke it, within this many runs
  (the first run confirms that reverting everything restores the pass).
  Both counterfactual settings are evidence only: they never change check status or the real
  workspace. Copies exclude dependency and build directories, so compiled checks rebuild from
  cold, and shell effects outside checkpoints are not reverted.
- `experimental.repro_first` (`off`, `advise`, `gate`; default `advise`): on a debugging task whose
  request explicitly asks for verification (a word starting with "verif", such as "verify" or
  "verification"), the first edit to a non-test, non-documentation source file before any
  reproduction failed this turn (a failing `bash` test command or a `code_failed` `verify`) gets a
  note (`advise`) or is refused once (`gate`). The `advise` note follows an edit that already
  applied, so it asks for a focused check of the reported behavior rather than a reproduction;
  `gate` asks for the reproduction before the edit. It fires at most once per turn; the next
  attempt proceeds. A fix request that does not ask for verification is not steered toward a
  reproduction.
- `experimental.outcome_gradient_rounds` (0–10, 0 = off): after this many consecutive checked
  rounds with edits in which the error count (failing checks plus failing libtest, pytest, Go or
  Jest tests) does not drop below its best, a loop note suggests changing hypothesis.
- `experimental.check_predictions` adds an optional `expect` (`pass` or `fail`) to `verify`. A
  mismatch between prediction and outcome is reported in the result text and in
  `data.prediction`; it does not change the check status. Sessions recorded with or without the
  property resume under either setting.
- `experimental.streaming_dispatch` (default on) starts parallel-safe reads while the reply is
  still streaming. Anthropic and Bedrock content-block stops and Responses finished output items
  explicitly close a call, allowing even the last or only read to start before the reply ends.
  Other streams retain the next-tool-index fallback; merely parseable partial JSON never starts
  a call. Reads require the normal parallel-read admission (no hooks for the tool, no deny rule,
  valid offered form, at most `task.max_concurrency` per request). Results are reused only for
  identical calls after ordinary admission runs again, and only while preceding calls were grouped
  reads or admitted disjoint native work; otherwise they are discarded. Forced single-call mode remains excluded: an invalid
  multi-call reply must execute nothing. Hermetic runs are also excluded. Concurrent file changes
  can make early results older than serial reads; set the option to `false` to disable it.
  Child loops share this observer and require live owning-controller authorization before starting
  early reads. An observed effect seals the early-read prefix; later calls wait for dispatch.

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
With no per-call overrides or summary focus, an already-safe history entirely within the configured
keep-recent budget is a successful no-op, even if adaptive small-window budgeting would retain less.
No summary request is made and no compaction is counted.
A manual compaction that has history to work with but where no method can make the next request
smaller is also an unchanged result, with a notice naming each method's reason, rather than an
error. Soft, Remote and Handoff skip the summary request when the history they would replace is
under 512 estimated tokens, because a summary of that little text is not shorter than its source.
Automatic compaction that fails this way keeps its retry floor into the next user turn, so a new
message does not rerun the same failing method chain until the request has grown.
CLI overrides include `--compaction-strategy`, `--compaction-threshold-tokens` and
`--compaction-reserve-tokens`; the numeric flags accept positive integers or `auto` to clear
an inherited value. Omission follows persisted configuration.
All editor and compaction policy fields, including typed model/agent maps, are available through `/settings`.

Automatic method selection skips native Remote compaction when the route has no declared
compatible capability (unless a custom summary endpoint is configured). Explicit `/compact remote`
still reports an unsupported route honestly. Portable Soft/Handoff requests send an explicit empty
`tools` array to OpenAI-compatible servers. On eligible stateless cloud routes, parent Handoff first
tries the actual live request prefix, retaining its system messages, tool schemas and cache namespace
and forcing `tool_choice: none`; it never dispatches a returned tool call. An alternate `small` role,
archive migration, active portable-rule guidance or a request-rewriting hook keeps the portable path.
Requests that cannot fit or receive a non-control provider failure use portable windows instead.
The summary can overlap the retained verbatim tail; the appended handoff instruction asks the model
to prioritize earlier context. Retaining raw recent groups and exact critical carry does not depend
on the model obeying that lossy-summary preference.
Keeping the payload prefix and namespace does not guarantee a provider cache hit: some providers
can invalidate cached blocks when request controls (including tool choice) change.
Portable foreground and background summaries use the `small` role when assigned, otherwise the
current model, including inside subagents. Internal summary budgets never inject output-limit
overrides into ChatGPT-plan requests; explicit unsupported sampling or output-limit settings in a
selected profile still produce a validation error.
Shake only elides recoverable tool evidence: if the
result still exceeds the safe request budget it tries the next configured method, never drops
conversation messages or commits an oversized replacement.
The compaction banner compares the previous and replacement **final projected requests** using
the same provider calibration. Automatic continuation requires a reduced request below the
compaction trigger and within the safe prompt budget, with `auto_continue` enabled and no
durability warning. The stricter recovery band still gates speculative results with live growth;
it does not pause an otherwise safe foreground continuation.

Preparation reserves estimated space for regenerated evidence notes, exact session-control carry
and the positive provider-token calibration offset before requesting a summary. Superseded exact
carry is replaced, not counted again in the portable history tail. Portable summaries retain whole
assistant/tool groups according to the remaining budget (including image-token policy), rather
than a fixed fraction of the context window. If the emitted summary needs more space, one bounded
incremental pass folds only newly hidden groups into it. An indivisible newest group that cannot fit
with required context is refused before a summary request is spent. These are planning estimates;
commit still validates the exact live projection, output reserve, strict reduction and snapshot
freshness before replacing history.

Compaction request accounting and whole-group tail selection use bundled offline OpenAI tokenizers
for explicitly recognized official model IDs (`gpt-4o`, `gpt-4.1`, `gpt-5`, `o1`, `o3`, `o4-mini`,
`gpt-4`, `gpt-3.5-turbo`, on the `openai` provider). Other IDs, snapshots and provider IDs retain
the byte estimate; tokenizer selection is not inferred from a suggestive model name. Explicitly
overriding an `openai` route's endpoint still uses its declared provider/model tokenizer; configure
a distinct provider ID for an incompatible gateway or local alias.
The bounded token-count cache retains digests and counts, not prompt text. Structured-message
counts still estimate provider framing; images, opaque replay and compaction-output floors remain
conservative. Provider usage calibration and exact final-projection fit checks still apply. The
context category breakdown remains byte-based telemetry, not an exact tokenizer or billing report.

Oversized individual transcript messages are split into ordered UTF-8-safe summary-input fragments,
not silently truncated. Every fragment is folded into the carried summary; canonical stored
assistant/tool groups are unchanged. A lower window learned from an overflow also applies to
remaining sibling fragments. Exhausting the bounded fragment/request work budget fails explicitly
with incomplete coverage rather than committing a partial summary. This establishes input coverage,
not that a model will remember every detail in its lossy output.

Parent split-turn summaries may run their independent history and turn-prefix folds concurrently
on separately owned cloud backends. Incremental windows within each fold remain sequential.
Local/shared inference, an active worker or provider-request hook, and `task.max_concurrency = 1`
keep the sequential path. Each request takes the existing `task.provider_concurrency` slot and
observes cancellation and a bounded deadline; both folds settle before a result is accepted.
Failure in either fold requests sibling cancellation through a split-local flag, never by setting
the user's interrupt flag. Both folds are joined before returning the original failure; a normal
method fallback can proceed unless the user actually stopped the operation. Cancellation remains
cooperative with the existing bounded provider-request machinery.

Recoverable pre-summary projection remains opt-in with `supersede_reads` and `drop_useless`
(both default `false`). Unique observation pruning requires `drop_useless`; mutations, skills,
unresolved failures and unknown effects are not disposable observations. The protected recent tail
stays expanded. New prune batches must free at least 8,000 **net estimated tokens**, after counting
the concrete artifact pointers; pointers that would expand context are never credited or applied.
Stored evidence is unchanged and projected pointers remain stable. A smaller projected request can
postpone ordinary compaction, but a cache-prefix rewrite can cost more than it saves: compare actual
uncached input, cache reads/writes, retrieval calls and total request cost before changing defaults.
No live-provider latency, cost, recall or cache-hit improvement is claimed for these mechanisms.

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

[compaction.promotion]
enabled = false # switch to a larger-window model before compacting or retrying an overflow
[compaction.promotion.targets]
# default = ["@long"]  # keys: default | provider/model | provider/*; see docs/model-routing.md

[compaction.native_models]
# "provider/model" = "responses-v2" # explicitly declared compatible endpoint/model
# Other values: responses-v1, responses-v1-v2, anthropic, none

[compaction.snapcompact]
shape = "auto" # model-aware OMP preset; wire family is the fallback
system_prompt = "none" # none | agents-md | all; protected instructions remain outside
tool_results = true
overlay = false # opt-in: one distinct 50%-opacity color layer per compaction
# frame_size = 1568 # override auto size (64..2576); explicit shapes keep their native default
# max_edge = 1568 # optional cap for a host that resizes images (64..2576)
image_detail = "auto" # auto | high | original; high for older/unknown routes

[compaction.snapcompact_models]
# "provider/model" = { kind = "fixed", tokens = 4096 }
# Alternatives: { kind = "pixel_ceiling" },
# { kind = "patches", patch_size = 32, tokens_per_patch = 1, base_tokens = 0 },
# { kind = "area", pixels_per_token = 750, base_tokens = 0 }
```

Native compaction requires an explicit compatible route declaration; protocol selection alone is
not evidence that an endpoint supports it. A custom summary endpoint resolves credentials for its
own URL. Image archives use actual bundled fonts, PNG dimensions and declared model billing;
unknown billing uses a conservative pixel ceiling, which never comes out smaller than the text it
replaces, so automatic method selection skips Snapcompact for a model without a
`snapcompact_models` entry; `/compact snapcompact` or `strategy = "snapcompact"` still tries it. All 17 font/
layout variants are selectable in settings. Characters a shape cannot draw (no bitmap or Silver ink,
including Arabic in every bundled font, controls and bidi/format marks) are rendered as exact JSON
`\uXXXX` escapes (surrogate pairs above U+FFFF), with literal backslashes doubled to distinguish
existing escapes. Decode that rendering layer before interpreting transcript JSON. The durable
source keeps the original characters; escaped text consumes normal cell and frame budget, and
overflow fails instead of truncating. Opaque native payloads and non-replayable image formats
still produce errors, not silent source loss.

Auto uses OMP's tuned black-ink layouts: **11on16-bw** (natural 8×13 glyphs, 11px
advance, 16px row pitch) for Claude; **8on22-bw** (8px advance, 22px pitch) for
Gemini, GPT/Codex and Kimi; **8on16-bw** for GLM. Known model identities take
precedence over the gateway's wire family. Unknown models fall back to 11on16-bw
on Anthropic-family wires and 8on22-bw elsewhere. Opus 4.7+ and Fable/Mythos use
1932px-wide frames; Gemini uses 2048px; other lines use 1568px. Frame heights hug
the rendered rows, with a 64px floor. Explicit shapes retain their original size
(2576px for 5x8, 1568px otherwise); `frame_size` overrides either choice and
`max_edge` caps it without rescaling glyphs. Set the cap in a route/model override
when a gateway downsizes images; Dial does not infer host image limits.

`image_detail = "auto"` uses `original` for GPT 5.4+ on OpenAI-family wires and
`high` otherwise, including older GPT models and unknown compatible endpoints.
This is deliberately more conservative than OMP's blanket OpenAI `original`
hint. Explicit `high` always applies to new text frames; `original` still requires
the supported reading-route tier above, not merely a configured override.
Generated `original` hints are clamped to `high` with a notice in outbound request
copies on unsupported reading routes (including after a model switch); retained
pixels and source records are not changed. The same resolved geometry/hint applies
to new archive frames and clean layer views. Source PNGs remain at high detail;
changing a preset does not rewrite old layers or reflow their retained masks.
Size bounds apply to newly rasterized text, not preserved source PNGs or stored
masks. A mixed-generation overlay keeps the largest retained page dimensions
instead of resizing old glyphs to a new preset.

Preset source: [OMP snapcompact](https://github.com/can1357/oh-my-pi/blob/aac9ffc1fe8f7a5cc9da905bc1f58e111faf440e/packages/snapcompact/src/snapcompact.ts).
The bundled X.org bitmap fonts and Silver font match that upstream revision.
Matching fonts/settings is **not** proof of equal efficiency or recall: Dial keeps
its exact-source serialization, no-truncation guarantee, conservative image billing
and full next-request fit gate. Upstream's reported recall and provider costs have
not been reproduced here. No model-name-based billing discount is applied; use
`snapcompact_models` only for a measured route's image-tokenization rule.
`cargo run --locked --example snapcompact_demo` writes synthetic native-renderer
previews to `scratch/snapcompact-tuned-preview/`; it makes no model requests.

With `overlay = true`, each Snapcompact generation draws a different colored text layer at
the same page coordinates over earlier generations, using 50% opacity. Layers are retained
separately with their exact sources; the composite is not an invertible encoding, and software
recovery never guesses text from blended pixels. Overlapping ink from later layers progressively
obscures earlier colors. Existing compaction methods and defaults do not change. Turning the flag
off does not discard layers already present in a session. In layered mode, `tool_results = false`
replaces visual tool-result bodies with explicit omission markers; exact records remain available
through software recovery. Original PNGs are also retained with the exact source records, so
layer metadata and separately attached source images both consume bounded storage.

The existing `session_search` tool exposes `view:"layers"` for the current session (or a child's
own private journal). Use `query:""` to list layers, select `layer:<id>` to recover one, or supply
query terms to retrieve matching and explicitly linked records across generations. `format:"text"`
is the default exact-text recovery path; `format:"image"` generates a clean, high-contrast view
from preserved source, not from the composite. Results are bounded and include continuation
information; follow that information rather than treating a page as the entire archive. Only
the two latest successful views remain expanded in model-facing requests. Earlier tool results
and source layers remain in the canonical journal. Tool permissions still apply: layered
compaction refuses if the active tool set withholds layer recovery. Archive size, image/frame
and prompt-budget limits fail closed rather than silently dropping a generation.

Overlay readability, model recall and token savings have not been established by a blind
provider/model benchmark. Colors are a visual presentation, not a reliable bit channel.

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

## Experimental reasoning strategies

Open `/settings` → Experimental → Reasoning → **Reasoning strategy** to select an advisory
workflow preset. The frontier-inspired choices are Dial interpretations of public guidance and
research, not vendor modes, model identities, or replicas of private reasoning. They work with
any configured backend: selecting a GPT-inspired preset does not select a GPT model. No preset
edits or exposes private chain of thought, changes weights, or changes provider thinking/effort
controls. **Default (unchanged)** adds no guidance. Existing saved strategy values keep their meaning.

| Strategy | Approach |
|---|---|
| GPT-inspired: Goal directed | Work directly toward explicit goals, constraints and observable success criteria; avoid unnecessary process. |
| Claude-inspired: Grounded | Inspect relevant source context, ground claims in evidence, and keep implementation and verification focused. |
| Gemini-inspired: Synthesis | Combine relevant context and examples, resolve bounded subproblems, and reconcile their results in the requested format. |
| DeepSeek-inspired: Cross-check | Develop a candidate solution and make one bounded check of its decisive claim; report evidence limits. |
| Evidence first | Establish observations and distinguish them from assumptions before acting. |
| Hypothesis driven | Use a bounded hypothesis and a discriminating check to resolve uncertainty. |
| Plan then act | Establish a proportionate plan before implementation and adapt to results. |
| Decompose | Divide a complex task into coherent parts and verify their integration. |
| Compare alternatives | Compare a small number of viable approaches against the task's constraints. |
| Critique and refine | Review a candidate approach for concrete flaws before finalizing it. |
| Verification first | Decide what evidence would demonstrate the requested outcome before changing it. |
| Minimal change | Prefer the smallest complete change at the existing owner, not a partial fix. |

The public sources behind these interpretations are [OpenAI's reasoning best practices](https://developers.openai.com/api/docs/guides/reasoning-best-practices)
(direct prompts, goals and constraints), [Anthropic's prompting guidance](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices)
(context and grounded responses), [Google's prompt design strategies](https://ai.google.dev/gemini-api/docs/prompting-strategies)
(context, examples and output requirements), and [DeepSeek-R1's model summary](https://github.com/deepseek-ai/DeepSeek-R1#2-model-summary)
(self-verification and reflection). These are bounded workflow adaptations, not copies of each
vendor's full prompting recipe: they do not inject thinking tags, require reasoning transcripts,
change sampling settings, or require multiple model samples. DeepSeek's research describes
trained model behavior; this prompt preset does not reproduce that training or its measured results.

The same bounded guidance is used by built-in main-agent prompts (full, lean and concise)
and new or resumed workers. A setting saved during a turn applies after that turn; existing
live workers retain their launch configuration. Main-agent custom replacement prompts and
system-prompt templates opt out, and hermetic agent runs retain their unchanged prompt.
All presets remain subordinate to user scope, role boundaries, approvals, tool availability,
budgets and verification requirements. They do not force extra model calls or disclose private
reasoning; user-facing output should remain concise findings and supporting evidence.

The setting can also be persisted through the ordinary configuration path:

```toml
[experimental]
reasoning_strategy = "evidence_first"
```

Other stored values are `default`, `hypothesis_driven`, `plan_then_act`, `decompose`,
`compare_alternatives`, `critique_and_refine`, `verification_first` and `minimal_change`.
Presets are experimental guidance, not a guarantee that a model follows the approach or that
reasoning quality, latency or token cost improves. Compare outcomes on representative tasks.

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
version. The live session report is the record of what that session requested, not confirmation
that the backend enforced it. Unset numeric overrides are labelled `unset`, rather than claiming
a measured server default or proving a field was absent from the wire request. Sampling selection
and server enforcement are labelled separately; capability-unverified sampling stays unverified.

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
improvement is implied by these counters. A call reuses a prepared result only when its arguments match the warmed call exactly; a divergent call neither consumes nor disturbs it, and a changed source file discards it. Arguments that cannot be serialized are never prepared. Dependent (child) preparation, preparation of partially streamed arguments and preparation under hooks are intentionally absent: dependent or streamed calls would need commit and discard ordering across tools this batch owner does not have, and hooks may observe or rewrite the call, so preparation is disabled while any `pre_tool_use` hook is configured.

## Turn optimizer (experimental)

`experimental.turn_optimizer` (Settings → Experimental → Agent routing) defaults to false. When it is
on, the `turn_drafter` role appears in the model picker's Roles section, and every user message,
both a new turn and steering sent while a turn runs, is rewritten by that role before the main model
sees it. The drafter treats the message as data, never as instructions to follow. It produces a
structured Markdown prompt (objective, context, requirements, constraints, acceptance criteria,
assumptions, open questions) and, where they help, Mermaid diagrams that restate the request. File
paths, `@references`, code and quoted text are kept verbatim. An unassigned `turn_drafter` uses the
main model in a separate request.

- It is a hard layer: the initial main request receives the drafted prompt, not the typed message.
  Originals remain the durable semantic reference for goal continuations and critical carry.
  Files attached through
  `@references` are expanded from the typed text and stay attached. Interactive Dial shows the
  proposed instruction in the transcript and a scrollable review panel: **Accept and send** (`a`,
  or Enter on that choice) submits it; **Accept all draft messages** (`s`) submits it and
  automatically accepts pending and future drafts, including steering, until Dial exits. This
  consent is not saved and does not change tool approvals or other confirmations. Drafts still
  appear in the transcript. **Cancel** (`c` or Esc) sends nothing. Page Up/Page Down scroll through
  long drafts. Non-interactive clients retain automatic delivery of valid drafts; interactive
  adapters without review support fail closed.
  The session keeps the typed text under `dial_turn_draft`. The transcript, resume, session
  previews, search and memory show what the user typed. Provider requests never carry the key.
- `/drafter` toggles the feature; `/drafter on`, `/drafter off` and `/drafter status` set or inspect
  it. The preference is saved for the project through `experimental.turn_optimizer`. During a
  running turn it is saved immediately and adopted after that turn. When off, original user
  messages go directly to the agent, without a drafter request or draft review.
- The drafter preserves the mode of work: implementation requests become direct instructions to
  inspect, implement and verify; questions and read-only reviews do not become edit requests.
  It must not manufacture permission gates or open questions for already actionable tasks.
- It fails closed: if the drafter errors, is cancelled, hits its output limit, returns an empty
  prompt, fails the verbatim-material check, or the user cancels review, nothing is sent to the
  main model. For a steering message, any steering still queued behind it is dropped too.
- Before review, a deterministic check requires explicit code/quoted spans, path-like tokens,
  `@references` and numeric tokens from the typed message to remain in the draft. The check
  does not echo missing text in errors, make another model request, or send raw text as a fallback.
  It checks presence, not semantic equivalence: changed negation, reordered constraints and added
  requirements still need human review. Unquoted commands and arbitrary identifiers are not
  comprehensively recognized. This is a correctness guard, not evidence of improved model quality.
- Steering is drafted at the next step boundary, because the main stream holds the model connection
  while it generates. Codex live mid-stream steering is therefore not used while the optimizer is
  on, and steering takes the local replan path. Steering left over from an interrupted turn is
  drafted before the next turn's message.
- Messages the harness writes itself (goal-mode continuation and budget prompts) and sealed
  evaluation agents are not drafted.

Each draft is an extra model request per user message, so it adds latency and cost.

## Request preparation and tool concurrency

`task.max_concurrency` remains the worker-branch admission budget (1..=64). It also sets the
parallel read window for ordinary and streamed tool calls; dependency-aware native-call waves
retain their separate ceiling of 32. These are tool-call windows, not additional worker pools.
The policy is centralized in `TaskConfig::parallel_read_limit` and `dependency_wave_limit`;
validated configurations retain their previous scheduling limits.

Request-prefix telemetry hashes the final provider-neutral request after hooks. Serialization
uses request-local reusable scratch space and fixed-size hashes; the tracker retains no message
contents. Historical messages are measured afresh because projection, compaction and hooks can
rewrite them. The ignored `request_prefix_preparation_benchmark` provides an offline long-context
microbenchmark; it is not a provider-cache or end-to-end latency benchmark.
