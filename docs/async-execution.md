# Opt-in asynchronous execution

These controls are experimental and **disabled by default**. Updating Dial does not change saved
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
complete, capturable input contract continue through synchronous final verification. Only one
check process is owned at a time; the normal final gate handles the remaining selected commands.

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

`experimental.dependency_dispatch` enables consecutive independent native `read`, `write`, and
`edit` calls in the parent controller, bounded by `task.max_concurrency` (and at most 32). Native
edit parsing supplies all source, destination and rename targets. Reads may share a resource;
any read/write or write/write overlap closes the current wave. Results and ordinary per-call
evidence accounting retain model order. Waves are nontransactional: one denied or failed call
does not undo a completed independent sibling.

Admission is deliberately conservative. Directories, file handles that cannot be expanded,
selectors, symlinks, hardlinks, hidden metadata paths, missing parent directories, shell commands,
hooks, extensions and unknown effects are serial barriers. Existing-file admission currently
requires Unix hardlink metadata; other platforms conservatively retain serial execution for
those resources. Codebase pre-edit checks, repro-first gates and authorized previews remain in
the controller before execution. The live native mutation callback rejects any computed effect
outside its admitted bounded footprint before mutation; normal policy and portable-rule
interception still apply.

Worker preparation can overlap for disjoint files. Native transaction commits remain serialized
under the workspace's existing lock and checkpoint observer. All workers drain before outcomes
are accounted for or a later conflicting call starts. This is not speculative shell execution,
a saved permission grant, or authorization based solely on model-declared paths.
