# Harness audit: workflow, prompting, tool exposure, batching (2026-11-06)

This is a read-only audit of the working tree, including uncommitted work, against current
frontier harness practice: Anthropic and OpenAI engineering guidance, Claude Code, Codex and pi.

Six parallel reviews covered prompting, tool surface, dispatch, transport, the agent loop and
external research. The findings marked **(verified)** were re-read in source while writing this
file. The rest are sourced to file:line but were not independently re-checked.

Nothing was built, run or measured. Every behaviour change below should go through
`efficiency_metrics`, `examples/replay_scheduling` and the live A/B route before it becomes a
default (see CONTRIBUTING's evidence rule).

Priorities: **P0** is a correctness bug or a silent cost leak. **P1** is high leverage. **P2** is
worthwhile. **P3** is an idea or cleanup.

---

## P0: bugs and silent cost leaks

| # | Finding | Where | Fix |
|---|---|---|---|
| 1 | **`bg` auto-load never fires in the default mode (verified).** `activate_tools` returns an error for any non-empty list when `stable_tool_offer()` is on, so the `is_ok()` guard silently fails. The `bash` description still says "Dial loads it when a command moves to the background", which is false. Every command running past about 15s costs an extra `tool_load` round plus a full cache rewrite. | `run/turn/round/settle.rs:371`, `run/tool_exposure.rs:183-189`, `:764` | Put `bg` in `CORE_TOOLS`, or fold job control into `bash` (job name plus wait/output/kill/stdin, as Codex unified exec does). Fix the description. |
| 2 | **Legitimate answers end `Incomplete` with no nudge (verified).** Any `validate_completion` error with no open steps or deliverables sets `hard_blocker` (`completion.rs:221-225, 271`). `failed_checks` includes EnvironmentUnavailable, PermissionDenied, TimedOut and pre-existing failures, because a baseline is captured only under `automatic_checks`, which is off. One already-broken crate makes every editing turn fail. `target_evidence=true` rejects "run X and tell me why it fails". | `verify/completion.rs`, `verify/checks.rs:1086-1100, 1326-1331`, `verify/evidence.rs:239-262` | Add an outcome *accepted with disclosed gaps* for environment, permission, timeout and baseline failures, and for runs whose failure the user asked about. Capture a baseline lazily on the first check before the first edit. |
| 3 | **Every tool load busts the entire Anthropic cache (tools, system and history).** Tools are sorted by name with the marker on the last one, so any change to `tools` invalidates everything after it. `exit_plan`/`goal` toggles, the planning-gate narrowing and `reliability::simplify` do the same. | `llm/anthropic_cache.rs:79-83`, `run/dispatch.rs:1617-1720`, `run/tool_exposure.rs:362-365`, `run/turn/step/prepare.rs:110,152` | Anthropic first-party: send the full catalog with `defer_loading: true` on non-core tools and return `tool_reference` blocks from `tool_search`. The tool-search beta is already in `USAGE_BETAS` but not in `MESSAGE_BETAS` (`anthropic.rs:157-158`). Newer models: `tool_addition`/`tool_removal` system-message blocks. OpenAI Responses: send the full list once and narrow with `tool_choice: allowed_tools`. Keep `exit_plan`/`goal` always offered and refuse them by state. |
| 4 | **The system breakpoint covers the dynamic bands (verified).** The API-key path joins every system band into one string with one marker at the end. The OAuth path splits them but `mark_last` still marks only the end. Per-turn notes, memory, plan-mode text, external-thinking text and "MCP still connecting" all sit inside the cached system prefix. Any change misses the system and all message breakpoints. | `llm/provider.rs:2666-2677`, `anthropic_cache.rs:85-91`, `base/lifecycle.rs:749-777`, `window/prompt.rs:713-721` | Send `system` as one block per band with the marker at the end of the **Stable** band. Use a spare marker after Dynamic. Deliver mode transitions as appended `role:"system"` or notice messages, not system rewrites. First count `system_changed` per user turn from the existing `changed_part` ledger to size the leak. |
| 5 | **Native compaction requests carry no `cache_control` (verified).** With no breakpoint there is no cache lookup, so the full prefix is billed at full price. That is roughly 20× the cached cost on a large Opus session. Soft summaries re-serialize history under a new system prompt and can never hit the cache. | `llm/provider/compaction.rs:717-813` | Call `anthropic_cache::apply` on the compaction body. For Soft, append the summarize instruction to the live byte-identical prefix, as Handoff partly does. |
| 6 | **Thinking-only streams can trip the 60s idle timeout.** Pings count as progress only while a `tool_use` block is open, and empty `thinking_delta` never counts. Current Claude models default to omitted thinking display, so long adaptive thinking streams only pings, which leads to stall retries and a blank UI. Responses has the same gap, since no reasoning summary is requested. | `llm/anthropic.rs:28-48`, `llm/responses.rs:83-103, 354-358` | Extend the ping allowance to open `thinking` blocks. Request summarized or `updates` thinking display, and `reasoning.summary:"auto"` on Responses. |
| 7 | **`--thinking off` sends `thinking:{type:"disabled"}` plus `temperature`.** Both are rejected (400) on current Opus, Sonnet and Fable 5.x. | `config/thinking.rs:106-135`, `profiles.rs:212`, `provider.rs:2775-2786` | Map "off" per catalog record: low effort, `between_tools`, or omit. Never send sampling parameters where the record says they were removed. |
| 8 | **Streamed early reads bypass `ReadReuse`.** Streaming dispatch is on by default, so overlapping re-reads return full text instead of evidence links. This is silent context bloat. | `run/streaming_dispatch.rs:170-185`, `run/dispatch.rs:176-187, 254-283` | Snapshot `engine.reusable_reads()` into `Admission` at stream start and route `read` through `reuse.execute`. |

---

## P1: high leverage

### Tool exposure and capability

9. **Calling a specialist tool costs three rounds: search, load, call.** A well-formed call to an
   eligible catalog tool is refused (`admit.rs:62`). Let `tool_search` load directly, via a
   `select:a,b` query or `load:true`, and return a parameter summary. Consider auto-admitting a valid
   call to an eligible, policy-allowed tool, as the legacy path did.
10. **Parameter descriptions are stripped from every Dial-owned schema (verified).**
    - Where: `bare_schemas`, `tool_exposure.rs:1076-1090`.
    - The model never sees defaults, maximums, units or per-action meaning. Examples: grep `limit`
      400 and `context` ≤ 10, bash `timeout` in seconds, `web_fetch` `max_chars`.
    - `bg` has 18 parameters documented only there; `lsp` loses `symbol`, `action_id` and `payload`.
    - Help arrives only after a failed call.
    - Fix: keep descriptions of 15 words or fewer for core and multi-action tools; there is about
      2 KB of headroom. Emit JSON Schema `default`/`maximum`, which survive stripping.
    - Contract identities ignore descriptions, so this doesn't affect contract compatibility.
11. **Discovery-mode description overrides drop important facts.**
    - Where: `tool_exposure.rs:712-727`.
    - grep loses its regex dialect (Rust, no lookaround), the `path:line:text` output format,
      `;`-scoped paths and the cursor footer.
    - bash loses the sandbox, network and `$TMPDIR` facts; glob loses its paging notes.
    - Fix: drop the overrides on hosted routes, or tighten the full text without removing facts.
12. **The 12 KiB core budget is sized for lean local models, but it also applies to frontier
    routes.**
    - `task`, `working_state` and `skill` are not offered, yet the system prompt prescribes
      `working_state` (`system.rs:38`) and the skills catalog says to "load it with the `skill`
      tool".
    - `docs/subagents.md:390` says task and skill "are offered in ordinary sessions", which is no
      longer true.
    - Fix: give hosted routes a larger budget (about 16-20 KiB) that includes `task` and `bg`. Gate
      every prompt sentence that names a tool on the actual offer.
13. **Edit grammar per model family.**
    - Hashline is the default everywhere (`config/tools.rs:45`, `model_variants` empty).
    - GPT-5.x/Codex are trained on `apply_patch` (V4A) and Claude on `str_replace`-style edits.
    - Fix: add default `model_variants`, gated on `edit-bench` plus a live A/B.
    - Separately, move the 717-byte notebook paragraph out of `edit`'s description (`tools/mod.rs:29`)
      into `.ipynb` read output and edit errors.
14. **Add `input_examples` and rewrite descriptions as "use when… / returns…".**
    - Targets: `edit` (hashline), `bg`, `lsp`, `todo`.
    - Several specialist descriptions lead with what the tool *never* does: `investigate`, `reality`,
      `workbench`, `perception`, `timeline`.
    - `finish` exposes harness internals ("one recovery nudge is shared per user request").

### Batching and parallelism

15. **Scheduling keys on "needs approval" instead of "has side effects" (verified).**
    - `web_fetch` and `web_search` return `needs_approval → true` unconditionally and run Exclusive
      (`net/web/mod.rs:151, 513`). MCP tools without `readOnlyHint` behave the same.
    - Five fetches therefore run one at a time even in `auto` mode, and they also block the reads
      that follow them.
    - Fix: add an *effect-free external read* class (`Concurrency::Shared`, empty workspace
      footprint). Authorize each call serially, as the wave path already does, then run them
      together.
16. **There is no total result budget per batch.**
    - Each result is capped at 4-48 KiB, but 32 parallel reads × 48 KiB is about 390k tokens.
    - Fix: split a per-reply budget of about 25-35% of the remaining window across results in
      `publish.rs`, through `BatchState`. Clip the largest results first, keep the full text as an
      artifact, and exempt errors and edits.
17. **Promote `eval` to programmatic tool calling.**
    - It already exposes `tool.<name>()` with results kept out of context (`exec/eval.rs:263`).
    - Callbacks are synchronous, serial, two journal flushes each, and capped at 128.
    - Fix: add `tool.batch([...])`, routed to the existing `parallel_tools` when every entry passes
      `parallel_read`, and coalesce flushes for read-only callbacks.
    - Add one prompt line: "for many lookups you only need to aggregate or filter, use one `eval`
      cell".
    - Published results for this pattern: 37% fewer tokens, and large BrowseComp gains when the model
      filters its own outputs.
18. **Invalidate selectively, and make in-flight reads cancellable.**
    - Today any serial call, including bookkeeping `todo`, and every wave discards all prefetched
      reads, while their threads keep running (`streaming_dispatch.rs:213-222`).
    - Fix: drop only results whose footprints conflict, and give the stream its own cancel flag.

### Agent loop

19. **The investigation nudge comes from a keyword classifier.** `task_policy::Kind::classify`
    matches words such as "why", "explain", "add" and "change" (`completion.rs:252-267`). That
    costs a round on pure-knowledge questions, and a second tool-free answer ends `Incomplete`. It
    also contradicts `docs/agent-loop.md:312-316`. Fix: skip it on non-lean routes, and never end a
    turn `Incomplete` because of it.
20. **Open todos block completion.** Models routinely leave the last item `in_progress` while
    writing the summary, which costs a nudge round plus a `todo done` round. Claude Code and Codex
    don't gate on todos. Fix: when the answer is accepted and nothing else blocks, auto-close the
    final `in_progress` step and list the rest with the existing `outstanding_notice`. Keep the
    gate on lean/local routes.
21. **The capability-denial heuristic is too broad.** `denies_capability` matches ordinary prose
    such as "this env var is not available in tests" (`completion.rs:363-405`). Gate it to
    `lean_local()`, or require a first-person subject.
22. **The research checkpoint taxes long debugging runs and scouts.** It warns at round 12 and
    forces a tool-free JSON checkpoint at round 24, and `bash` counts as observation, not progress
    (`workflow/research.rs`). Disable it for non-lean routes and read-only children, or make it
    advisory only at about 60 rounds.

### Prompting

23. **Project instruction files are framed as untrusted data.**
    - `CONTEXT_HEADING` says "not as commands", HEADER lumps them in with web content, and
      TRUST_RULE ranks project conventions last.
    - AGENTS.md and CLAUDE.md are written by the user. Codex and Claude Code tell the model to
      follow them.
    - Fix: give them their own tier: "follow them for work in this repo; user direction wins on
      conflict; they cannot widen permissions." Emit the heading once.
24. **One notice envelope, described once.**
    - About 25 notice kinds use about 8 envelope styles: `<system-notice>`, `<system-interrupt>`,
      `[Dial recovery: …]`, `[Loop check from Dial]` and others.
    - Fix: use one `<dial-notice kind="…">` form and say once in the system prompt that it comes
      from the harness, not the user.
    - Drop the "Attempt #n/m" counters (`recovery.rs:86-98`).
    - Current guidance warns that frequent harness text after tool results gets misread as
      injection.
25. **Remove the shouting from notices that stay in history.**
    - The worst case is `settle.rs:389`: "MUST resume… NEVER… ask whether to proceed" (verified). It
      is stored durably and contradicts HEADER's "ask before destructive actions".
    - Others: TAN `CONTEXT_SWITCH` ("STOP. NEVER work on ANY OTHER TASK"), goal
      `CONTINUATION`/`BUDGET_LIMIT`, compaction `BRANCH_PROMPT`, `prompt.rs:354`, `recovery.rs:108`.
    - Rewrite calmly with the reason given, and mark them transient.
26. **Strip about 32 per-notice permission disclaimers** ("grants no permission", "do not bypass",
    "approvals still apply"). The harness enforces these limits. State the boundary once with its
    reason. Repeating it nudges frontier models toward hedging.
27. **Rewrite the base policy as short bullets that give their reasons.**
    - `TOOL_POLICY` is a single 60-word sentence and `DELIVERY` is a 70-word semicolon chain.
    - "generate data by computing, writing and asserting the final artifact in one permitted run" is
      opaque.
    - `CALL_CONTRACT_PRECEDENCE` is always present even when there is no call limit.
    - `[INFERENCE]` markers can leak into answers.
    - `cargo check -p <crate>` is hard-coded for every language.
    - Add what's missing: git safety (no commit, push or force unless asked), "no features or
      refactors beyond the request", `path:line` citations, brief progress text between calls, and
      the notice-envelope line.
    - Consider adapting Anthropic's published, tested autonomy, scope and verification blocks rather
      than writing new ones.
28. **Shorten recovery text on failed edits.** Every failed edit or write appends about 700 bytes of
    `SOURCE_OR_CALL_CONTRACT` plus `FIRST_FAILURE` to history permanently. Put the general guidance in
    the `edit` description and give one cause-specific line on failure ("old_string not found:
    re-read lines X–Y").

### Transport

29. **Default OpenAI API-key reasoning models to Responses.** The `openai` protocol defaults to
    chat-completions, which discards reasoning between tool turns. Responses already handles
    `encrypted_content` correctly. Send `prompt_cache_key` unconditionally, since it is only a
    routing hint. *(OpenAI details are from memory; verify live.)*
30. **Make native compaction automatic on first-party routes.** `native_models` is empty by default
    (`config/compaction.rs:363`). Derive it from the catalog record. Scale `keep_recent_tokens`,
    currently a fixed 20k, to about 10% of the window. Re-attach current ranges of files edited this
    turn within a budget.
31. **Effort defaults per model.**
    - Anthropic catalog rows have no `d` field, so Opus 5.5 runs at the server default `medium`.
    - Set per-model defaults by eval.
    - Fix the Haiku 4.5 row, which lists efforts the model rejects.
    - Changing effort per turn (`thinking_auto`) invalidates the cache. Use the per-message effort
      system message where supported.

---

## P2: worthwhile

32. **Split cache TTLs.** Use 1h on the tools and system markers and 5m on rolling message markers
    (`anthropic_cache.rs:37-40, 69-73`). API-key `auto` uses 5m everywhere, but user think time often
    exceeds 5 minutes.
33. **Cache warmer.** It replays with `max_tokens=1`, which drops `thinking` on budget-mode models
    and invalidates message breakpoints. Use `max_tokens: 0` and copy the live thinking parameter
    (`cache_warmer.rs:207`).
34. **Preserved-thinking safety.** History must be append-only on 5.x. Nothing references
    `prefix_mismatch_behavior`, and the opt-in pruning modes and `experimental_context_management`
    rewrite history. Gate them on affected models, and add a CI prefix-diff check using the
    `request_prefix` tracker.
35. **Handle the `refusal` and `pause_turn` stop reasons.** They pass straight through
    (`provider.rs:3036-3042`). Surface the category to the user and resume `pause_turn`.
36. **OpenAI requests without a profile** send `max_tokens` and `temperature=0.2` to gpt-5.x
    reasoning models (`llm/mod.rs:905-923`). Take both from the catalog record, or add a recovery
    rung. *(verify live)*
37. **A pending approval prompt blocks later independent reads.** Streaming seals at the first
    non-read. When the barrier has a resolved footprint, keep starting non-conflicting reads and hold
    their results until it settles.
38. **One-call mode rejects the whole batch.** Run call 0 if it is a `parallel_read`, mark the rest
    Skipped, and save the round (`dispatch.rs:42-43`).
39. **Merge wave admission.** Parent and child each have their own logic and they diverge on
    bookkeeping and `verify` (`dependency_dispatch.rs:173-191` vs `workers/child/turn.rs:2040-2075`).
    Use one function with a role policy, per AGENTS.md "no parallel pipeline".
40. **Resource footprints are too cautious.** Dotfile paths (`.github/`, `.cargo/`) and any `@`
    (`node_modules/@scope`) become serial barriers (`resources.rs:51-58`).
41. **MCP fixes:**
    - Use stable hash suffixes for name collisions instead of registration-order `_2`
      (`mcp/names.rs:30-41`).
    - Pass image and audio content to vision models; it is currently dropped (`mcp/tool.rs:655`).
    - Stop appending `structuredContent` alongside the text it duplicates.
    - Cap input schema size.
    - Bring the 64 KiB result cap in line with the 48 KiB agent cap.
42. **Default subagent step caps.** Every built-in role has `max_steps: 0`, which is unlimited
    (`config/subagents.rs`). Set per-role caps, for example scout 30 and reviewer 40. Make subagents
    async (return immediately, results arrive later) to match current practice.
43. **Coach review ignores `min_changed_files`** (`modes/advisor.rs:20-38`).
44. **One pagination cursor.** grep and glob have both `skip`/`remaining_skip` and a structured
    `cursor`. Replace them with one opaque string. `web_fetch` has no offset.
45. **Schemas aren't strict-mode friendly.**
    - `todo` lacks `required:["op"]` and `additionalProperties:false`, so it is always non-strict.
    - `tool_load` has no `required`.
    - `read` uses `minProperties`.
46. **Turn drafter (off by default).** If kept, send the verbatim message as the authoritative user
    text with the draft as a labeled interpretation. Keep pasted material quoted. Skip short
    messages. Make diagrams opt-in.

---

## P3: new ideas, consolidation and cleanup

47. **Workspace-visible progress file for long tasks.** In goal mode, mirror `working_state` and
    todos to `.dial/progress.md` or an artifact at each continuation, and offer a commit checkpoint.
    This follows Anthropic's long-running-harness pattern (feature list, progress file, git).
48. **Task budgets instead of harness countdowns.** Replace the 80% budget notice and goal
    `BUDGET_LIMIT` with the provider-side `task_budget` where supported.
49. **Consolidate overlapping tools.** The catalog has about 70 tools.
    - Evidence: `evidence`, `evidence_search`, `workbench`, `investigate`, `reality`, `timeline` and
      `perception` → one `evidence` tool with actions, or a skill.
    - Search: `find` and `ast_search` → modes of `grep`.
    - Read: `read` already covers URLs, documents, media and archives, which makes most of
      `web_fetch`, `document`, `audio`, `visual`, `video`, `archive` and `notebook` redundant.
    - Delegation: `task`, `side_request`, `solve`, `escalate`, `message` and `peers`.
    - State: `memory`, `experience`, `codebase_memory`, `working_state`, `context_notes`, `todo` and
      `checkpoint`.
    - Fewer tools means better search ranking and less model confusion.
50. **Merge `todo` and `working_state`.**
    - `open_work` overlaps with todos, and deliverables overlap with steps.
    - Fix: one plan tool with status-only updates, and keep claims/decisions as the compaction
      carry. Drop the legacy `investigation` field and fold `finding`/`evidence` into `note`.
51. **The lean variants are fake.** Every `LEAN_*` constant aliases the full one (verified;
    `system.rs:12-80`).
    - Either delete the aliases and flags, or write a real roughly 8-bullet plain-word lean prompt
      for small local models. That is where prompt wording matters most.
    - Prefix the worked example with "Illustration only; use real paths and results".
52. **Retire experiments or put them behind one `experimental.labs` flag.**
    - Candidates: ratchet (about 1.6k lines), cleanse (about 2.75k), prewalk/plan_yolo (they
      overlap), external_thinking (counterproductive on frontier models), rule_author, snapcompact
      (about 2.5k), reasoning presets, turn_optimizer, outcome_gradient.
    - Delete `task_routing.rs` (about 1k lines of lexical routing) once the stable offer is proven.
    - Re-check whether `improve.background=true` and `repro_first=Advise` belong in the defaults.
53. **Add a gate table to `docs/agent-loop.md`.** At 1,890 lines, the hedging buries the real
    gates. One table of gate → trigger → rounds → tier would make the overhead visible and
    reviewable.
54. **Move recall context before the user text.** Long-context guidance puts reference material
    before the request (`workflow/evidence.rs:79-84`). Put the date in the first user message's
    recall, not the system band.
55. **Fix stale docs and config comments.**
    - `config/types/workflow.rs:33` says only two experiments are on, but `streaming_dispatch` is also
      on.
    - `docs/async-execution.md:3` says "disabled by default".
    - `dependency_dispatch` reads `self.config` while streaming reads `effective_config()`, so
      profile overrides are ignored for waves.
56. **Smaller transport items:**
    - Use `eager_input_streaming` instead of the 10-minute ping workaround.
    - Honour `x-should-retry`.
    - Move the web-search helper from Haiku 4.5 to Haiku 5.5.
    - Add Haiku 5.5 and Fable 5 to `documented_context_window`.
    - Use `count_tokens` near compaction thresholds instead of bytes/3.

---

## Keep: already at or ahead of current practice

- A lean stable core of 11 tools in about 10 KB, with tests that pin offer and system-prompt
  stability. `tool_search` never mutates the offer, and the system band doesn't list tools.
- Frozen, Stable and Dynamic bands. Transient notes go as tail user messages, and cache anchors
  never sit past the transient suffix. Recall is attached once to the stored user message, so
  history stays byte-stable.
- One advisory note per request, with priority, a repeat window and coalescing. The shared setback
  ledger has no growing corrective prompt.
- Streaming dispatch of read-only calls. A conservative shared prefix planner where declarations
  enable scheduling, never permission. Model-order results for the provider and live completion
  for the UI. Batch dedupe of identical calls.
- `read`: ranges, outlines, up to 16 paths per call, next-range footers, a context-scaled cap.
  Hashline: multi-file edits, preflight, re-anchoring, `already_applied`.
- Cache accounting with read, 5m and 1h writes and `changed_part`. Per-endpoint cache-refusal
  latches. Owner-scoped replay of signed thinking and encrypted reasoning.
- Retries that honour `retry-after`, treat 529 as a throttle, and never execute partial tool calls.
  Conservative tool-call repair.
- The completion gate runs the applicable focused check itself before spending a model round.
  Crate-scoped check selection.
- Exact critical carry through compaction. Subagent results condensed to 14 KB with an artifact
  link to the full report. Event-driven waiting. Broad hook coverage. A subagent-stop event is the
  missing piece.

## Suggested order

1. **P0 #1, #2, #5, #7, #8.** Small, local fixes; mostly `completion.rs`, `settle.rs`,
   `compaction.rs` and `streaming_dispatch.rs`.
2. **Measure cache invalidation per user turn** with the existing `changed_part` ledger. Then do
   P0 #3 and #4 together, since both are cache-prefix work in `anthropic_cache.rs` and
   `provider.rs`.
3. **Tool surface pass: #9-#14.** Description and budget changes don't affect contract identity.
4. **Batching: #15-#18.** Rank them with `efficiency_metrics.scheduling` first.
5. **Prompt rewrite: #23-#28.** Do it as one change and A/B it on the live slice.
6. **Consolidation and retirement (P3)** once the new defaults have held.
