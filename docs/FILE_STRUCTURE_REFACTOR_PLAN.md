# Core prompt organization

## Scope and invariants

Centralize built-in core agent instruction sources under
`crates/dial-agent/src/agent/prompts/`, grouped by purpose. Keep runtime state,
selection, safety enforcement, message assembly, and tool contracts with their
existing owners. Preserve exact prompt text, formatting, placeholder syntax,
visibility through existing public paths, custom overrides, and all existing work.
No dynamic prompt loading, dependency additions, model calls, or installation.

## Migration map

| Existing source | Central source | Integration boundary |
| --- | --- | --- |
| `agent/window/prompt.rs` fixed policy constants | `agent/prompts/system.rs` | Full/lean/concise/one-call selection and system-message bands stay in `window/prompt.rs` |
| `agent/compact/compaction.rs` summary instructions and `compaction/summary.rs` policy constants | `agent/prompts/compaction.rs` | Native/portable selection, folding, evidence and budgets stay in `compact` |
| `agent/safety/stream_guards.rs`, `agent/run/failure_guidance.rs`, `agent/ui/chat.rs`, `agent/verify/visual.rs` fixed recovery/visual instructions | `agent/prompts/recovery.rs` | Classifiers, retry limits, tool effects and visual evidence remain enforced by existing owners |
| Advisor, external-thinking, prewalk, goal and TAN mode instructions | `agent/prompts/modes.rs` and child modules | Mode activation, parsing, enforcement and tool schemas stay in `modes` |

Paths in the table are relative to `crates/dial-agent/src/`.
Existing self-contained domain prompts and lower-layer tool contracts are indexed,
not duplicated or moved across dependency boundaries.

## Verification

- Preserve source literal bytes during extraction; use one-off migration comparisons
  rather than permanent prose snapshots that obstruct later wording changes.
- Run agent library tests covering prompt assembly, overrides, gating, recovery,
  worker lifecycle and compaction; run architecture integration checks.
- Check formatting, Clippy correctness/suspicious diagnostics, and final diff.
- Review remaining references: intentional compatibility re-exports are not stale owners.

Implementation status and the permanent navigation index are documented in
`crates/dial-agent/src/agent/prompts/README.md`.
