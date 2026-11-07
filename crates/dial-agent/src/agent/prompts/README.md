# Agent prompts and core behavior: start here

This is the source-of-truth directory for shared built-in agent policy text.
Use the index below to find both the wording and the runtime owner that applies it.
**Prompt text is guidance, not enforcement or authority.** Changing wording must not
silently change permissions, budgets, tool contracts, or completion criteria.

## Central prompt sources

| Edit here | Purpose | Applied by |
| --- | --- | --- |
| [system.rs](system.rs) | Engineering, instruction trust tiers, tool policy, workflow, focused work, delivery and delegation, as short bullets with their reasons; lean and compact routes reuse the same sections | [window/prompt.rs](../window/prompt.rs) selects offered-tool/profile sections and assembles cacheable message bands; workers reuse focused-work policy |
| [compaction.rs](compaction.rs) | Initial/update, handoff, turn-prefix and branch summary policy, plus file-target provenance wording | [compact/compaction.rs](../compact/compaction.rs) and [summary.rs](../compact/compaction/summary.rs) select, escape and fold inputs within budgets |
| [recovery.rs](recovery.rs) | First/repeated failure advice, failure categories, stalled/empty/action-only replies, duplicate-call, research-synthesis and thinking-loop notices, initial visual guidance | [run/failure_guidance.rs](../run/failure_guidance.rs), [workflow/research.rs](../workflow/research.rs), [safety/stream_guards.rs](../safety/stream_guards.rs), [ui/chat.rs](../ui/chat.rs), [verify/visual.rs](../verify/visual.rs) |
| [modes/](modes/) | Advisor, external-thinking, goal, prewalk and TAN instructions | Corresponding modules under [modes/](../modes/) retain state, activation, rendering and enforcement |
| [turn_drafter.rs](turn_drafter.rs) | Experimental turn optimizer: restates a user message as a structured prompt (Mermaid diagrams only with `experimental.turn_drafter_diagrams`) and labels the `turn_draft` interpretation notice | [run/turn_draft.rs](../run/turn_draft.rs) decides when drafting runs (not for short or already structured messages), frames the message as data, fails closed and attaches the draft beneath the authoritative typed text |
| [reasoning.rs](reasoning.rs) | Experimental selectable reasoning-strategy guidance; the default adds no text and presets never request private reasoning disclosure | Built-in [window/prompt.rs](../window/prompt.rs) policy assembly and [workers/child.rs](../workers/child.rs) initial/resumed prefix |

The Rust constants are compiled into the binary: no working-directory-dependent file
loading, runtime reload, or installation asset copying is required. Existing public
instruction paths remain available via imports/re-exports where needed. This directory
is crate-internal, not a new public prompt-registry API.

## Shared policy and bounded adjustments

There are no built-in model-family prompt forks. `system.rs` owns shared plain-language policy in
priority order: active goal, hard boundaries (scope, git safety, no unrequested features), the
instruction trust order, investigation before acting or asking, coherent edits, scoped checks and a
short honest report. The base-policy byte test guards a 4 KiB ceiling; capability-specific sections
and every sentence that names a tool (`todo`, `working_state`, `skill`, `tool_search`/`tool_load`)
remain conditional on the offer. Runtime owners enforce completion and permissions independently of
wording.

Trust tiers: the latest user direction, then operator guidance, then project instruction files
(AGENTS.md, CLAUDE.md), which are followed for work in the repository under one heading. File
contents, tool results, web pages, MCP text and fetched content stay evidence, not instructions.
The system policy states once that Dial enforces approvals, the sandbox and budgets, so individual
notices do not repeat permission disclaimers.

Harness notices use one envelope, `<dial-notice kind="…">…</dial-notice>`, described once in the
system policy. Use `prompts::recovery::notice` (or the same literal form in a `{{name}}` template),
give a calm reason instead of capitalized commands, and do not add attempt counters. Quoted
untrusted text inside a notice is escaped (`prompts::recovery::quoted`) so it cannot close the
envelope; when a notice carries data, such as research observation IDs or application-watch
context, the data goes inside the same envelope rather than after it. Session modes that can
change mid-session (plan mode, external thinking, MCP servers still connecting) are restated as a
transient request-tail notice by `base/lifecycle.rs::mode_notice`, so the cached system band stays
session-constant.

Deterministic `llm::fake` lifecycle tests exercise one state gate for plain replies and `finish`,
zero-observation investigation recovery, explicit-only specialist loads, unchanged request prefix
bytes, check reuse and no default self-review. The live benchmark slice measures provider usage
and behavior counters separately; passing fake tests does not establish improved model quality.

Required deliverables belong in `working_state`, execution steps in `todo` (`kind=step`), and
optional follow-ups in `kind=follow_up`. Open execution obligations cannot be bypassed with an
answer or automatically closed by final prose. Configured/user gates and sandbox/approval
boundaries remain authoritative. Operator replacement prompts and appended guidance are retained.

The existing adjustments are named below by their selection inputs. They are not family
classifications, and each can be removed at its selection/assembly boundary without copying
the common policy into model code.

| Adjustment | Existing selection branch | Assembly/text owner |
| --- | --- | --- |
| Lean guidance (`lean`) | `base/lifecycle.rs::rebuild_system`: `lean_tools` (context-overflow fallback), `lean_local()` or `local_escalated()`; local tier/escalation decisions stay in `model/local_profile.rs` | `window/prompt.rs::full_guidance`: the shared sections, without visual verification unless a browser or computer tool is offered, plus the compact helper and delegation clauses. There is no separate lean wording; a shorter lean prompt would need its own outcome evidence |
| One-call operation (`one_call`) | `model/local_profile.rs::local_one_call`: small local tier with reliability enabled and no passing saved batch fixture | `window/prompt.rs::full_guidance`: `LEAN_ONE_CALL` plus `CALL_CONTRACT_PRECEDENCE` replace `LEAN_BATCHING`; the precedence line appears only while this presentation limits calls. Request-scoped limits from reliability recovery carry their own transient tool contract |
| Worked loop (`worked_example`) | `base/lifecycle.rs::rebuild_system`: `local_one_call()` | `window/prompt.rs::worked_example` / `example_edit`: active edit grammar, labelled as an illustration without invented check output; requires `grep`, `read`, `edit` and either `verify` (preferred, shown as an edit `verify` list) or `bash`; built-in guidance only |
| Compact guidance (`concise_guidance`) | `base/lifecycle.rs::rebuild_system`: efficiency enabled, non-hermetic run | `window/prompt.rs::full_guidance` / `scoped_guidance`: the same compact sections as `lean` (tool policy, frame → gather → decide → act → verify → report execution order, change/verify workflow, delivery, delegation) with tool-gated skill/project-map/LSP rules and `side_request`/`task` clauses; tool mechanics stay in tool descriptions and harness refusals. The default stable system band uses generic schema authority and does not enumerate changing offers; `tool_search` returns descriptions without loading. A byte-budget test in `window/prompt/tests.rs` guards the size |
| Capability scoping | Configured tool offer, not transient tool loading | `window/prompt.rs::full_guidance` / `scoped_guidance`: tool-specific rules, visual/preview sections and delegation sections |
| Delegation preference (`prefer_subagents`) | `base/lifecycle.rs::rebuild_system`: `task.prefer_subagents` | `window/prompt.rs::scoped_guidance`: shared selective/specialist policy, only when `task` is offered |

Request settings (thinking, sampling, context, edit mode and tool allow-lists) remain in
`dial-llm` model profiles and configuration, not in new family prompt files. Preserve
operator replacement prompts/templates and appended guidance. No new model-specific wording
is justified by this consolidation: future exceptions need repeatable outcome-based evidence,
an explicit selection condition and evaluation again after model upgrades. Assembly tests
alone do not establish a live-model improvement.

Presentation inputs are named in `window/prompt.rs::Presentation`; one-call mode implies lean
workflow. Compact guidance reuses existing short policy sections without narrowing the tool offer
or changing approval/check enforcement. Tool loading does not rewrite these sections; local
escalation can rebuild them at turn boundaries. Verification follows the selected delegation
preference and existing check ownership, not merely tool availability.

Custom-template deduplication recognizes complete normalized lines/blocks (including note bullets),
not embedded substrings. Notes are filtered individually in both inline and message-band assembly.
Transformed or inline-wrapped values may be repeated conservatively rather than silently omitted.
Templates, replacement text and appended guidance remain operator-controlled.

The composition refactor was checked against legacy rendered assembly before wording changes.
Request-contract precedence, verification wording, template deduplication and example eligibility
are intentional, independently located behavior changes; they are not byte-preserving tuning.
Their effects on model outcomes remain unmeasured. Escalation persistence/step-down is unchanged:
no replacement thresholds or success-driven decay policy have been established.

## Instructions deliberately kept with their domain owners

Not every message is shared policy. Keep self-contained domain renderers, schema-bound
tool contracts, and controller-generated evidence beside their types; do not copy them
here just to make a second catalog. This index is the common navigation entry point.

| Source | Why it stays there |
| --- | --- |
| [window/prompt.rs](../window/prompt.rs) | Environment/MCP/context trust framing, schema-shaped worked examples, template data and message ordering are assembly code |
| [workers/child.rs](../workers/child.rs), [workers/subagents.rs](../workers/subagents.rs) | Role/assignment envelopes and lifecycle-specific instructions depend on admitted permissions, coordination and execution state |
| [modes/cleanse/prompts.rs](../modes/cleanse/prompts.rs) | Already isolated prompt renderer, coupled to cleanse discovery and assignment types |
| [modes/resources/rule_author.md](../modes/resources/rule_author.md) | Existing compile-time mode resource, loaded by its owner |
| [commit/prompts.rs](../../commit/prompts.rs), [commit/prompts/](../../commit/prompts/), [commit/agentic/prompts/](../../commit/agentic/prompts/), [commit/changelog/prompts/](../../commit/changelog/prompts/) | Separate commit/changelog workflow with its own output formats and consumers |
| [dial-computer prompts](../../../../dial-computer/src/computer/prompts/computer.md) | Desktop capability contract belongs to the lower-layer computer crate |
| [tool surface](../../../../dial-tools/src/tools/surface/) | Tool descriptions, schemas and contract identities belong to the tool owner; do not duplicate them in system policy |
| [window/context.rs](../window/context.rs) | Repository instruction-file discovery and limits; [window/prompt.rs](../window/prompt.rs) frames the files under one `Project instructions` heading with per-file escaping and attribution |
| [window/prompt_template.rs](../window/prompt_template.rs) | Compatibility entry point for the shared template engine; operator replacement templates are not built-in policy |

## Core runtime map

| Change in behavior | Owner |
| --- | --- |
| Session setup and rebuilding prompts | [base/lifecycle.rs](../base/lifecycle.rs) |
| Main request loop and inference/tool settlement | [run/turn.rs](../run/turn.rs), [run/engine.rs](../run/engine.rs) |
| Advisory selection, priorities and delivered-note lifetime | [run/guidance.rs](../run/guidance.rs) |
| Latest admitted user context and corrected-target reminder | [run/intent.rs](../run/intent.rs) selects runtime-recorded provenance; [window/projection.rs](../window/projection.rs) appends bounded request-only context using [system.rs](system.rs) wording, independently of advisory-note priority |
| Stream retry limits and detection | [safety/stream_guards.rs](../safety/stream_guards.rs) |
| Tool admission and permissions | [safety/approval.rs](../safety/approval.rs), [safety/guard.rs](../safety/guard.rs) |
| Worker execution, report-only final step, isolation and persistence | [workers/](../workers/) |
| Verification and accepted completion | [verify/](../verify/) |
| Durable obligations and human-confirmed scope transitions | [workflow/working_state.rs](../workflow/working_state.rs), [run/dispatch/working_state.rs](../run/dispatch/working_state.rs); model notes cannot silently replace obligations |
| Gap-responsive capability offers and search-miss fallback | [run/tool_exposure.rs](../run/tool_exposure.rs), [run/dispatch.rs](../run/dispatch.rs); routing and catalog pages replace empty-search recovery prose, not permission policy |
| Context pressure and summary publication | [compact/](../compact/), [window/](../window/) |
| Planning, progress and working-state records | [workflow/](../workflow/) |

For layering and the wider system map, see [ARCHITECTURE.md](../../../../../ARCHITECTURE.md).
For runtime contracts, see [agent-loop.md](../../../../../docs/agent-loop.md).

## Editing workflow

1. Decide whether the change is wording, assembly, or enforcement. Edit the owner above;
   do not add a second copy of shared instructions at a call site.
2. For shared wording, consider full, lean, concise, one-call and worker routes.
   Preserve trust framing and the distinction between observations and instructions.
3. Preserve placeholders exactly. Retry/mode templates use `{{name}}` replacements;
   repeated-call notices use native Rust formatting so inserted evidence cannot be
   substituted again. Operator templates have a separate renderer. Never interpret
   placeholder-looking user/tool data as template syntax.
   Leading/trailing newlines in constants are intentional assembly separators.
4. Preserve operator overrides, custom templates and stable message ordering. Built-in
   changes must not overwrite saved user guidance or inject unavailable tools.
5. Run the relevant behavioral tests. Do not add prose golden tests just to freeze
   wording: test routing, rendering, evidence provenance and runtime boundaries.
   Wording and threshold changes follow the evidence rule in
   [CONTRIBUTING.md](../../../../../CONTRIBUTING.md#prompt-wording-thresholds-and-recovery-limits).
6. Rebuild/restart Dial to use changes. Source edits do not replace an installed binary.

From the repository root:

```sh
cargo test --locked -p dial-agent --lib
cargo test --locked -p dial --test integration architecture::
cargo fmt --check
cargo clippy --locked -p dial-agent --all-targets -- -D clippy::correctness -D clippy::suspicious
```

Prompt assembly, compaction, failure guidance, stream guards, mode and worker tests
remain beside their runtime consumers. A source-organization change should preserve
prompt bytes; substantive wording changes instead need outcome-based evaluation.
Neither passing unit tests nor centralization establishes better live-model reasoning,
latency or token cost. See the controlled evaluation guidance in `docs/agent-loop.md`.
