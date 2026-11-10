# Surface inventory, November 2026

An inventory of experimental flags and optional modes, with the evidence available for each. It
supports item 52 of the [harness audit](harness-audit-2026-11.md) (retire experiments or put them
behind one flag). **Nothing was removed.** Removing a mode deletes a feature and can break recorded
sessions, so each removal needs an owner decision.

## What the evidence can and cannot show

- **Code references** are counted with `grep` over `crates/` and `src/`, excluding test files.
- **Tests, benchmarks, docs** are counts of files that mention the name.
- **Owner config** is one user's `~/.dial/config.toml`. It shows what one person turned on, not
  what users in general need.
- **Slash-command usage is not observable.** Session transcripts do not record slash commands: a
  search of 63 local transcripts finds no `/undo`, `/model` or `/compact` either, although those
  are used routinely. A zero count for `/ratchet` or `/cleanse` therefore says nothing. Deciding by
  usage needs a command counter in the stats ledger first (see [Next steps](#next-steps)).
- No flag below has a recorded live-model A/B result in `examples/benchmarks/`; only
  `adaptive_output_effort` is named in a benchmark file at all.

## Experimental flags (`[experimental]`)

Defaults are from `crates/dial-llm/src/config/types/workflow.rs`.

| Flag | Default | Code refs | Test files | Docs | Owner config |
| --- | --- | ---: | ---: | ---: | --- |
| `adaptive_output_effort` | off | 3 | 2 | 0 | unset |
| `video_analysis` | off | 9 | 3 | 3 | unset |
| `work_packet_contracts` | off | 7 | 4 | 1 | **on** |
| `verification_receipts` | off | 4 | 5 | 1 | **on** |
| `context_overlap_dedup` | off | 5 | 2 | 1 | **on** |
| `profile_matching` | off | 5 | 3 | 1 | **on** |
| `tool_speculation` | off | 5 | 1 | 2 | unset |
| `turn_optimizer` | off | 9 | 5 | 4 | off |
| `turn_drafter_diagrams` | off | 5 | 1 | 1 | unset |
| `reasoning_strategy` | `default` | 6 | 4 | 1 | `default` |
| `fault_witness` | off | 10 | 3 | 1 | unset |
| `repro_first` | `advise` | 10 | 2 | 2 | unset |
| `edit_bisection_runs` | 0 | 10 | 2 | 1 | unset |
| `outcome_gradient_rounds` | 0 | 12 | 1 | 1 | unset |
| `streaming_dispatch` | **on** | 7 | 3 | 4 | unset |
| `batch_approvals` | off | 3 | 1 | 1 | unset |
| `dependency_dispatch` | **on** | 4 | 4 | 3 | on |
| `background_checks` | off | 3 | 1 | 1 | unset |
| `check_predictions` | off | 9 | 3 | 1 | unset |

Reading the table:

- **Graduate, do not retire:** `streaming_dispatch` and `dependency_dispatch` are on by default.
  They are no longer experiments; they should become ordinary behaviour (or ordinary settings)
  and leave `[experimental]`.
- **Decide with a measurement:** the four flags the owner runs daily (`work_packet_contracts`,
  `verification_receipts`, `context_overlap_dedup`, `profile_matching`) are either worth making
  default or not worth keeping. The [release slice](../benchmarks.md#release-slice) with each
  flag on and off is the missing evidence.
- **Retirement candidates:** flags that are off by default, unset in the owner's config and
  unmeasured: `tool_speculation`, `turn_drafter_diagrams`, `batch_approvals`,
  `background_checks`, `adaptive_output_effort`, `edit_bisection_runs`,
  `outcome_gradient_rounds`, `fault_witness`, `check_predictions`, and the twelve
  non-default `reasoning_strategy` presets. This is absence of evidence for them, not evidence
  against them.

## Modes named by the audit

Line counts are of non-test Rust files whose path contains the name; shared code elsewhere is not
counted, so removal would touch more than this.

| Mode | Own source lines | Own test lines | Files mentioning it | Docs |
| --- | ---: | ---: | ---: | ---: |
| `snapcompact` | 3,804 | 2,298 | 43 | 4 |
| `cleanse` | 2,964 | 271 | 12 | 1 |
| `ratchet` | 1,949 | 133 | 6 | 1 |
| `rule_author` | 484 | 249 | 4 | 1 |
| `prewalk` | 468 | 0 | 19 | 2 |
| `plan_yolo` | 86 | 104 | 8 | 1 |
| `external_thinking` | 63 | 69 | 18 | 1 |

- `snapcompact` is one of the five default compaction methods
  (`remote, soft, shake, snapcompact, handoff`) and has eight settings. Retiring it changes the
  default compaction order, so it needs a compaction measurement, not only a decision.
- `ratchet`, `cleanse` and `prewalk` are slash-command modes
  (`crates/dial-app/src/app/commands/modes.rs`). `prewalk` has no tests of its own.
- None of these names appears in the owner's config.

## Next steps

1. Record a count per slash command in the stats ledger (name only, no arguments), so that the
   next inventory can use real usage.
2. Run the release slice with each daily-use flag on and off; make the winners default and delete
   the flag either way.
3. Move `streaming_dispatch` and `dependency_dispatch` out of `[experimental]`.
4. For each retirement candidate, the owner chooses: delete, keep, or move behind a single
   `experimental.labs` switch as the audit proposes.

Steps 2 and 4 are not done: step 2 spends provider quota and step 4 deletes features.
