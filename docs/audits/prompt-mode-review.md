# Optional-mode instruction review

## Scope and ownership

Reviewed `crates/dial-agent/src/agent/prompts/modes/{prewalk,goal,advisor,tan,external_thinking}.rs` and their runtime consumers in `agent/modes/`. Architecture follows `ARCHITECTURE.md` and the prompt ownership index in `agent/prompts/README.md`: constants provide guidance; mode runtimes retain activation, budgets, permissions, completion, filtering and handoff state. No new abstraction or prompt framework is needed.

## Findings and changes

- **Prewalk:** The planning nudge asserted that enough was known before examining the task, demanded exact targets and a fixed 5–9 todos, and prohibited legitimate test maintenance. It now asks for evidence-grounded planning, targeted clarification reads, task-sized todos when offered, and relevant verification without weakening checks. The follow-up checklist avoids unrelated matching-site rewrites and redundant verification. Both planning and continuation allow a concrete blocked report instead of insisting on action at any cost.
- **Goal:** Initial and continuation prompts now reuse observed evidence while its supporting state remains unchanged, rather than demanding fresh reads/runs on every continuation. Missing evidence remains a completion gap, not authority to widen scope. Both routes explicitly describe blockers, objective stop conditions and the existing todo `block` operation. The continuation labels the objective as user task context, consistent with initial/budget-limit prompts. Full-objective success, current-state evidence, token placeholders and budget-exhaustion-not-completion rules are preserved.
- **Advisor:** Added an explicit evidence-as-data boundary to the live review question. Existing severity JSON, evidence-grounded blocker guidance, uncertainty distinction and inability to certify correctness remain intact. Runtime already bounds reviews/repairs and frames returned suggestions as unverified, not permissions.
- **TAN:** Clarified that dropping inherited task obligations does not drop safety or permissions. Shared-workspace conflicts should be reported rather than overwritten. Parent-side wording no longer forbids responding to later user instructions or accounting for shared-workspace conflicts, and distinguishes absent work reports from separately available lifecycle status/transcripts.
- **External thinking:** No wording or runtime change. The compact instruction matches the existing optional scratchpad filter and public re-export. Filtering is presentation behavior, not a new security boundary or broader authorization.
- **Goal rendering:** Found a caller bug: inserting the objective before replacing budget placeholders reinterpreted literal placeholder names inside user text. The renderer now replaces numeric fields first and inserts escaped user text last. TAN already inserts user work last, so it does not have the same substitution-order issue.

## Runtime boundaries retained

- Prewalk switches only after a successful `edit`/`write` and the existing todo gate; the gate already allows operation without an offered todo tool (`modes/prewalk.rs`). Prompt wording does not change scheduling or force a handoff for read-only tasks.
- Goal `follow_up` stops when no todo is actionable and at least one is blocked. Interrupt and stalled-continuation exits remain unchanged (`modes/goal.rs`). A blocked report alone does not mutate runtime goal status; with no blocked todo, the existing stall rule may require an additional continuation. No pause tool or new protocol is introduced.
- Goal completion still passes through `Agent::completion_blockers`; neither low budget nor a blocker is completion. Budget-limited wrap-up semantics are unchanged.
- TAN clones still inherit approvals, clear inherited todos/goal state, and run the requested task against the shared workspace. Notices do not provide isolation or conflict enforcement.

## Verification and limitations

The sole runtime change is objective-placeholder preservation in goal rendering, covered by a behavioral regression test across initial, continuation and budget-limit templates (including XML escaping and real budget values). No prose snapshot tests were added. Updated the existing prewalk integration routing test's two short message markers; its handoff/model-persistence assertions are unchanged. Also refreshed two existing recovery-routing markers for the parent-owned recovery wording change. Scoped `git diff --check` and direct Rust formatting checks passed before the renderer addition; parent owns shared Cargo/format/integration checks. Source review cannot establish live-model success rate, latency or token improvements; those require controlled behavioral evaluation as described in `docs/agent-loop.md`.
