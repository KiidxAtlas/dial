# Agent prompts and core behavior: start here

This is the source-of-truth directory for shared built-in agent policy text.
Use the index below to find both the wording and the runtime owner that applies it.
**Prompt text is guidance, not enforcement or authority.** Changing wording must not
silently change permissions, budgets, tool contracts, or completion criteria.

## Central prompt sources

| Edit here | Purpose | Applied by |
| --- | --- | --- |
| [system.rs](system.rs) | Engineering, tool policy, workflow, trust, focused work, delivery and delegation; full and lean variants | [window/prompt.rs](../window/prompt.rs) selects offered-tool/profile sections and assembles cacheable message bands; workers reuse focused-work policy |
| [compaction.rs](compaction.rs) | Initial/update, handoff, turn-prefix and branch summary policy, plus file-target provenance wording | [compact/compaction.rs](../compact/compaction.rs) and [summary.rs](../compact/compaction/summary.rs) select, escape and fold inputs within budgets |
| [recovery.rs](recovery.rs) | First/repeated failure advice, failure categories, stalled/empty/action-only replies, duplicate-call and thinking-loop notices, initial visual guidance | [run/failure_guidance.rs](../run/failure_guidance.rs), [safety/stream_guards.rs](../safety/stream_guards.rs), [ui/chat.rs](../ui/chat.rs), [verify/visual.rs](../verify/visual.rs) |
| [modes/](modes/) | Advisor, external-thinking, goal, prewalk and TAN instructions | Corresponding modules under [modes/](../modes/) retain state, activation, rendering and enforcement |

The Rust constants are compiled into the binary: no working-directory-dependent file
loading, runtime reload, or installation asset copying is required. Existing public
instruction paths remain available via imports/re-exports where needed. This directory
is crate-internal, not a new public prompt-registry API.

## Instructions deliberately kept with their domain owners

Not every message is shared policy. Keep self-contained domain renderers, schema-bound
tool contracts, and controller-generated evidence beside their types; do not copy them
here just to make a second catalog. This index is the common navigation entry point.

| Source | Why it stays there |
| --- | --- |
| [window/prompt.rs](../window/prompt.rs) | Environment/MCP/context trust framing, schema-shaped worked examples, template data and message ordering are assembly code |
| [workers/child.rs](../workers/child.rs), [workers/subagents.rs](../workers/subagents.rs) | Role/assignment envelopes and lifecycle-specific instructions depend on admitted permissions, coordination and execution state |
| [modes/cleanse/prompts.rs](../modes/cleanse/prompts.rs) | Already isolated prompt renderer, coupled to cleanse discovery and assignment types |
| [modes/resources/omfg.md](../modes/resources/omfg.md) | Existing compile-time mode resource, loaded by its owner |
| [commit/prompts.rs](../../commit/prompts.rs), [commit/prompts/](../../commit/prompts/), [commit/agentic/prompts/](../../commit/agentic/prompts/), [commit/changelog/prompts/](../../commit/changelog/prompts/) | Separate commit/changelog workflow with its own output formats and consumers |
| [dial-computer prompts](../../../../dial-computer/src/computer/prompts/computer.md) | Desktop capability contract belongs to the lower-layer computer crate |
| [tool surface](../../../../dial-tools/src/tools/surface/) | Tool descriptions, schemas and contract identities belong to the tool owner; do not duplicate them in system policy |
| [window/context.rs](../window/context.rs) | Repository instruction-file discovery and limits; project content remains untrusted data |
| [window/prompt_template.rs](../window/prompt_template.rs) | Compatibility entry point for the shared template engine; operator replacement templates are not built-in policy |

## Core runtime map

| Change in behavior | Owner |
| --- | --- |
| Session setup and rebuilding prompts | [base/lifecycle.rs](../base/lifecycle.rs) |
| Main request loop and inference/tool settlement | [run/turn.rs](../run/turn.rs), [run/engine.rs](../run/engine.rs) |
| Advisory selection, priorities and delivered-note lifetime | [run/guidance.rs](../run/guidance.rs) |
| Stream retry limits and detection | [safety/stream_guards.rs](../safety/stream_guards.rs) |
| Tool admission and permissions | [safety/approval.rs](../safety/approval.rs), [safety/guard.rs](../safety/guard.rs) |
| Worker execution, report-only final step, isolation and persistence | [workers/](../workers/) |
| Verification and accepted completion | [verify/](../verify/) |
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

The migration mapping and preservation constraints are recorded in
[FILE_STRUCTURE_REFACTOR_PLAN.md](../../../../../docs/FILE_STRUCTURE_REFACTOR_PLAN.md).
