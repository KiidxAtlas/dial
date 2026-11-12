# Opt-in asynchronous execution

These controls are experimental. Most are **disabled by default**; `dependency_dispatch` and
`streaming_dispatch` are on by default (see below). Updating Dial does not change saved
settings, enable them, or create permission grants. Configuration declarations are exposed through
the ordinary settings UI and validation.

## Finite approval batches

`experimental.batch_approvals` lets an Ask-mode parent controller present up to 32 admitted native
`edit`, `write`, and `ast_edit` calls in one approval interaction. This is a finite selection of
**exact call IDs, tool names, and effective arguments**, not an “always allow” rule. Approval is
consumed once. Changed arguments cannot consume the original choice. Explicit denies, plan mode,
current approval policy, native computed-path authorization, schema validation, and cancellation
still apply at execution time.

The default UI adapter offers all-or-nothing approval; adapters can return individual selections.
Missing selections deny. Persistent-grant choices on the batch prompt do not authorize the batch
or create rules. Existing individual reusable-grant prompts are unchanged. Hooks, shell commands,
application watches, image generation, and future unknown child calls are not batch-approved.
Children continue to ask their live parent under ordinary per-call policy.

A batch is **not transactional**: approved operations may partially succeed, and completed effects
are not erased if a sibling fails. Native workspace edits already avoid prompts in Write mode;
this feature does not claim every edit previously needed approval.

## Focused checks after edits

With `experimental.background_checks` and `verification.automatic_checks` enabled, a committed
parent edit can start one configured focused command without waiting for another model request.
The command must have an existing complete `verification.inputs` contract. Commands without a
complete, capturable input contract continue through synchronous final verification. By default
one check is started at a time; `experimental.background_check_slots` (1–4) starts up to that many
eligible configured checks together, each authorized separately. They run in parallel with each
other but hold one shared workspace serial-operation boundary, so they must tolerate running side
by side (separate outputs, build-tool locks). The normal final gate handles the remaining selected
commands.

Configured commands retain ordinary shell authorization, explicit denies and sandbox policy.
Denied commands start no process. The controller owns the process until its actual outcome is
settled. Native admitted read operations may progress while it runs; shell calls, hooks, unknown
effects, writers, and completion drain the pending check first. The process also participates in
the shared workspace serial-operation boundary. This is not permission to run arbitrary checks
concurrently with shared-workspace mutations.

Synchronous and asynchronous results share freshness capture, classification, evidence binding,
and counterfactual verification. External edits, incomplete snapshots, or changed effect revisions
withhold pass and code-failure repair credit. Required checks are drained on turn exit, including
errors and interruption, and before evidence resets. The completion gate reuses only evidence
whose declared inputs remain fresh; stale obligations are rerun. Unfinished processes never earn
success by inference. Background check execution is not restart-surviving verification evidence.

## Dependency scheduling boundary

`experimental.dependency_dispatch` is on by default. The parent and child controllers share one
wave admission (`plan_wave`) with a role policy, and a trusted tool resource resolver. Waves read
the effective (profile-overridden) configuration, like streaming and read groups. Consecutive independent native
operations run together, bounded by `task.max_concurrency` (at most 32). Multi-path `read`,
`write`, `edit`, bounded applying `ast_edit`, and directory-scoped `grep`/`glob` participate.
Directory reads cover membership and descendants, including absent matches; they conflict with
any writer in that scope. Read/read overlap is safe. A conflict closes the prefix; calls never
jump over a conflicting or unknown operation. Git mutations remain exclusive.

Read-only shell commands (the parallel-read form: no `session`, `pty` or `background`),
`dep_source`, and `git status`/`diff` without `path` read every workspace root, so they join
waves with reads, external reads and bookkeeping but never run beside a write. `git
status`/`diff`/`blame` with `path` read only that path. `git log`/`reflog`/`show`/`branches`
(object-database reads) and the `pr_view`/`pr_checks`/`issue_view` actions, `github` read ops and
`glab_mr` read ops touch no source file and are effect-free external reads (below). In a jj
repository every jj-backed read snapshots the working copy and reads every root.

Parent `todo` steps and working-state views/ordinary progress patches can appear inside a group. They
settle on the controller in original order after native work drains. Deliverable-scope changes
remain barriers because they may need a human decision. An `edit`/`write` with `verify` can end
a group; verification uses the ordinary coalescing owner after source epochs settle, and results
remain fresh when the round later accounts for individual calls. Workers retain their existing
restriction on inline verification and run scoped checks through their permitted tools.

`web_fetch` and `web_search` are *effect-free external reads*: they share a batch slot and have an
empty workspace footprint, but still need approval. MCP tools whose server declares
`readOnlyHint: true` (and not `destructiveHint: true`) join the same class, whether or not the
entry sets `trust_read_only_hint`; the declaration selects the scheduling class only, so approval
is unchanged and a stale catalog revokes it. Each call in a wave is authorized in model
order before any of them starts; the authorized ones then run together, and a denied call does
not stop its siblings. Concurrent web calls share one HTTP client; a Stop reaches every running
call, and a sibling starting later cannot clear it.

Results and evidence accounting retain model order. Groups are nontransactional: a failed or
denied call does not undo a completed independent sibling. Failed native mutations block only
conflicting or unknown dependencies in that response; diagnostic reads and disjoint work remain
available. Stop and steer take effect at group boundaries; already-started calls finish and keep
their outcomes. Set `dependency_dispatch = false` to serialize native mutations again. Ordinary
read batching remains available with that setting.

File handles that cannot be expanded, selectors, aliases, symlinks, hardlinks, version-control
and Dial metadata directories (`.git`, `.hg`, `.svn`, `.jj`, `.dial`), host-shaped leading
segments (`user@host/...`), missing parent directories, shell commands other than read-only
ones, hooks and unknown effects
remain barriers. Ordinary dotfile directories such as `.github/` and `.cargo/`, and scoped package
directories such as `node_modules/@scope/`, are schedulable paths.
Existing-file resource admission currently requires Unix hardlink metadata; other platforms
conservatively keep those operations serial. Native editor parsing supplies every source,
destination and rename target. AST preview tokens supply their exact retained targets; directory
AST applies stay serial. Native preparation can overlap, while commits keep the workspace lock,
transaction validation and checkpoint observer.

An extension can opt in through the Rust `Tool::resources` contract, declaring all reads and
individual write files. Writers must own workspace commit serialization and invoke the computed
mutation authorizer before effects. Undeclared tools, callback-dispatch tools and unbounded or
external effects remain serial. Declarations enable scheduling, never permission: schema checks,
role limits, live approval, portable rules and computed-target bounds still apply in both owners.

`experimental.streaming_dispatch` is also on by default; set it to `false` to disable early reads.
Parent and child loops reuse the same streaming observer. Only the admitted read prefix starts
before the response ends; an observed effect seals it, while `todo` steps and ordinary working-state
bookkeeping do not. Child reads additionally require the live parent's authorization before
starting. Early `read`s use the same read reuse as the parallel group: rows already visible in an
earlier result come back as an evidence link. Exact results are reused only after ordinary final
admission. When a call runs outside the read group, only the early results it may have changed
are dropped: bookkeeping and hook-free reads keep all of them, a hook-free call with a resolved
footprint drops only conflicting reads, and anything else discards the stream and cancels its
in-flight reads through the stream's own cancel flag (which also mirrors Stop). Unused results
are discarded and their reads cancelled when the round ends. Forced single-call, hermetic and tool-free phases
remain excluded. Early reads observe the filesystem at their actual execution time, so an
external edit during generation can change what a later serial read would have seen.

A pending approval prompt no longer holds up independent reads. When a serial call, or a member
of a native wave, is about to wait for a human, and it is hook-free with a resolved footprint
(an effect-free external read has an empty one), the later hook-free reads in the same reply whose
footprints are disjoint from it start through the same stream. Admission matches the parallel-read
group, and the scan stops at the next effect. Their results are held until the barrier settles and
are used only when the round reaches those calls, in model order. A denied call never runs, so its
held reads stay valid. If the approved call's recorded changes reach a held read's resources, that
read is discarded and runs again in order; unknown effects discard the stream. Starting a read
grants no permission: the barrier is authorized exactly as before.

## Reply result budget and one-call batches

Each tool result is already capped at the per-model output limit, but a wide batch could still
fill the window. The parent and worker controllers therefore share a budget of
`efficiency.batch_result_percent` (default 30%) of the free history window (`budget_window`, or a
worker's own window, minus current usage) across one reply's inline results. Results already produced
by the same group are water-filled together, so the largest are clipped first, and none is cut
below 4 KiB. Only results whose full text is retained as an artifact are clipped, and the clipped
body links to it (`retrieve with read`). Errors, mutation receipts (`edit`, `write`, `ast_edit`,
anything that changed paths) and controller bookkeeping are exempt. The budget is off with
`efficiency.batch_result_percent = 0` or `efficiency.enabled = false`.

When a request is constrained to one tool call and the model still sends several, a leading
admitted read-only inspection runs and every later call is skipped unexecuted with the one-call
message; previously the whole batch was refused. If the first call is not a read-only inspection,
the whole batch is refused as before.

## Scheduling measurements

The existing efficiency report includes `scheduling` and separately aggregated `worker_scheduling`:
group counts, call counts, size histograms, wall/work microseconds, prefix barrier reasons, and
early-read starts/reuse/discards. Work time is inclusive tool time and overlaps other measurements;
its difference from group elapsed time is not a measured end-to-end speedup. Worker reports carry
their controller's metrics, including provider fallback attempts, and repeated delivery is counted
once.

Compare recorded call ordering without executing any recorded effects:

```sh
cargo run --locked --example replay_scheduling -- /path/to/workspace /path/to/session.jsonl
```

The comparison uses the same prefix planner and current workspace metadata with default config.
It compares read batching alone against resource scheduling and reports group sizes and barriers.
It cannot reconstruct historical files, approvals, dynamic preflights or provider streaming timing;
these are structural opportunities, not predicted wall-time savings. Session paths and call
contents are not printed. For actual local timing, the opt-in `scheduling_measurements` integration
fixtures compare real tool execution under a scripted provider; no model-quality or hosted latency
claim follows from those fixtures. A dependency-graph scheduler remains deferred until these
measurements justify replacing the prefix policy.

Local measurements on 2026-11-06 (five alternating-order samples per mode, debug build,
scripted localhost provider; no live inference):

| Workload | Baseline median | Grouped median | Model requests |
| --- | ---: | ---: | --- |
| Eight file reads, separate vs batched responses | 152.5 ms | 61.5 ms | 9 → 2 |
| Eight independent file writes, same response | 90.3 ms | 87.5 ms | 2 → 2 |

Write timings overlapped (baseline 84–107 ms, grouped 79–108 ms), so this does not establish a
reliable end-to-end write speedup. The grouped runs observed two native groups of four calls.
A read-only analysis of five recorded project sessions (213 call-bearing responses, 301 calls)
found 235 groups with read batching alone versus 231 with resource scheduling, using current
workspace metadata. Most barriers were unknown resources. The traces were not executed and
these counts do not reconstruct historical timing. Reproduce the local timings with:

```sh
cargo test --locked --test integration scheduling_measurements -- --ignored --nocapture --test-threads=1
```

## Stop receipts and job history

In Jobs, `s` requests Stop for the selected live job. A request is not proof of cancellation:
the job remains owned until the lifecycle owner observes completion. Jobs and Activity share
the native receipt: a signal after an explicit stop is **Stopped**, not inferred **Cancelled**;
missing termination evidence remains **Unknown**. Cooperative tool jobs keep their original
result and are still managed while cancellation and cleanup finish.

Stop retains terminal output and history in the existing bounded job table (eight finished
entries retained under admission pressure, with a 256 KiB display ring per job). Enter views
that output. `d` separately dismisses terminal history; it refuses live jobs, including jobs
whose stop has been requested but whose completion has not yet been observed. `bg kill` uses
the same owner and retention semantics; its parameter schema is unchanged.
