# Requirement-aware evidence

Dial can map **declared requirements to observed verification**, without interpreting an English completion claim as proof. This is opt-in operator configuration; ordinary answers still need no checklist.

```toml
[verification]
automatic_checks = true
behavioral_checks = ["input-boundaries", "error-contract"]
# Optional: refuse completed outcomes when declared evidence is missing or stale.
# strict_requirements = true

[[verification.commands]]
name = "input-boundaries"
command = "python3 -m unittest tests.test_boundaries"
file_extensions = ["py"]

[[verification.commands]]
name = "error-contract"
command = "python3 -m unittest tests.test_errors"
file_extensions = ["py"]

[verification.requirements.input-domain]
checks = ["input-boundaries", "error-contract"]
# Optional: enforce this contract when this working-state deliverable is claimed satisfied.
deliverables = ["boundary-handling"]

# Optional: needs current native rendered pixels delivered to a model request.
# [verification.requirements.appearance]
# visual = true
# visual_target = "url:http://localhost:3000/"
```

Requirements name unique configured checks, not arbitrary commands or model-authored prose. A requirement needs at least one check or a visual obligation. Up to 50 requirements and 16 checks per requirement are accepted; invalid, duplicate or ambiguous check references fail configuration validation. Optional `deliverables` lists up to 32 unique working-state deliverable IDs per requirement and requires verification enabled. `visual_target` requires `visual = true` and an exact native locator: `url:<canonical URL>`, `window:<native ID>` or `display:<native ID>`; ambiguous display selectors such as `active` or `all` are not exact targets. Legacy configurations default to no declaration.

When automatic verification is enabled, a declared check is selected regardless of its extension filter: requirements are obligations, not just changed-file hints. Normal check authorization, cancellation, deadlines and repair policy still apply. With automatic checks disabled, model-requested configured commands can supply the same evidence; no process is automatically started merely by declaring a requirement.

`completion_evidence.requirements` records each requirement, its check names/statuses, mapped
deliverable IDs, visual target/status, and the exact effect revision. Every mapped check must have
an observed passing result for the latest revision. Missing, stale, failed, cancelled, timed-out,
zero-test and environment/permission-unavailable results leave the requirement uncovered. A later
edit or uncheckpointed operation invalidates earlier evidence. Baselines cannot satisfy a later
revision.

A visual obligation needs native rendered browser or desktop pixels delivered in a subsequent
successful model request. Native capture receipts bind the actual image bytes and media type by
digest; a tool name, printed screenshot claim, DOM/source text, arbitrary image attachment or
pending capture does not suffice. With `visual_target`, the receipt must also match that exact
target at the current revision. Without it, any current native rendered capture can satisfy the
visual obligation, including one whose target identity is unknown. A URL/window/display locator
does not identify a build, prove that the latest code was served, or establish visual correctness.
Compound desktop runs discard captures invalidated by later or overlapping native mutations.
Eval retains capture receipts only from its final tool callback; a later callback clears earlier
receipts rather than assigning old pixels the compound call's final revision.

Coverage is `declared_requirements_observed` only for this declared mapping. Open tasks still take
precedence as `open_obligations`. Undeclared requests remain `not_independently_assessed`. The report
does not claim that the contract covers the entire user request or that the tests are sufficient.
By default uncovered declarations remain an explicit evidence gap; `strict_requirements = true`
instead refuses completed settlement until every declared check and visual obligation has current
observed evidence. Independently of strict mode, a working-state deliverable claimed `satisfied`
must satisfy every requirement mapped to its ID: unrelated passing checks, arbitrary citations or
omitted `check:` references cannot bypass the mapping. Its controller acceptance is `observed`,
`needs_verification`, or `unassessed` when no contract maps to it, separately from the model's
claimed status. The mapping does not create a missing working-state deliverable or bypass the
existing human confirmation needed to remove/cancel one.

Both explicit `finish` and plain replies use these guards, with a final freshness check after
hooks. Missing/unavailable evidence produces an honest blocked or incomplete outcome, not
unbounded retries or a renderer permission grant. Strict mode requires verification enabled;
an empty contract creates no obligations.

Uncovered declarations also prevent positive behavioral outcome credit and successful executable
recipe capture/reuse credit, even when an unrelated behavioral check passed. Empty legacy
declarations retain the existing evidence rules; neither declarations nor summaries are new learning labels.

Executable-recipe reuse and isolated candidate verification use the same focused-check classifier:
only an observed completed process with exit code zero and no recognized zero-test report can pass.
Printed `[exit code: 0]` text is not process evidence. Cancellation, unknown effects, missing
completion and zero-test runs cannot earn success credit; clipped output without an available
complete artifact remains unverified. A wrapper's observed zero exit still does not prove that
every inner command succeeded, so verification commands must propagate inner failures.

## Requested run targets

When the latest admitted user request starts with a run verb (`run`, `launch`, `start`, `execute`, `play`) and names an existing workspace path, completion needs an observed run of that path. Filler such as "please", "okay" or "can you" may come first. A verb followed by `by`, `with`, `on`, `a`, `review` or a gerund ("start by…", "start editing…", "launch a review of…") does not count. A path span is up to four words and must contain a `/` or a file extension, so `local testing/` resolves and plain prose does not. Questions, explanations and requests that only mention a path create no obligation. Autonomous, peer and hermetic turns create none either. A steering message that itself asks to run named paths replaces the obligation, keeping runs already observed for paths it still names; other steering keeps the open obligation. `verification.target_evidence = false` turns the rule off.

A run counts when it names the path: directly, by absolute path, from a `cwd` inside the directory, or by file name from the file's directory. Each `;`, `&&` or newline segment of a shell command is read separately. `cd DIR` moves the working directory for later segments. Redirection operands (`> file`, `>> file`) and segments led by a plain inspection or a file utility (`ls`, `cat`, `grep`, `printf`, `sed`, `cp`, `chmod` and similar) never count, so writing or editing the path is not a run of it. Driving a tracked job with `bg` `input`, `resize`, `close_stdin` or `kill` does not make its run stale. Only controller facts decide:

- **Foreground `bash`.** The observed `ProcessOutcome` and the shell's launch facts.
- **`bg start`.** Structured `job` data (`name`, `pid`, `terminal`) and any readiness observation. A readiness failure still reports the job, which stays observable.
- **`bg wait` or `bg output`.** The job's current process state, attributed to the effect epoch and source snapshot at which the job was launched, never refreshed by a later observation.

Printed output is never interpreted. Process evidence requires that the latest observed run names the requested path, is still running, reached readiness, or exited zero, and has no newer effect after launch. It also binds to the requested source identity captured before dispatch: completion rereads that identity, so an external edit invalidates evidence even without a controller edit epoch. Directory identities use the existing bounded, confined content manifest, including ignored/hidden members and directory membership. Missing, inaccessible, escaped or over-budget identities remain unknown, not fresh. Hook-rewritten launches without a bound source snapshot remain unverified. A checkpointed edit, opaque shell command or possibly modifying extension call also makes a prior launch stale. Observing an older job does not refresh its launch identity. A `bg start` whose state was never observed, a nonzero exit, a signal, timeout or cancellation cannot satisfy the obligation. Directory argument matching remains lexical: passing a path inside that directory as an argument can count as a run, but cannot certify behavior.

Both plain replies and `finish` with `outcome=completed` share the guard. An unsatisfied completed candidate ends as incomplete (or blocked for explicit blocked obligations), retaining its summary and the exact missing prerequisite rather than repeatedly requesting a final answer. The report states the latest exit status and launch facts; eligible terminal recovery remains subject to tool availability, policy and approval, with no automatic replay.

`completion_evidence.requested_targets` records each path's status, launches, terminal launches, latest run and source freshness. Its `behavior` is always `not_assessed`, with `behavior_verified: false`. A run that stayed up or exited zero establishes process evidence only; neither it nor passing checks on a substitute target proves the requested application behavior. A program that exits zero without doing its work can satisfy the process obligation while behavior remains unverified.

## Recovery by cause

Parent and worker settlement share bounded, cause-specific advice. Typed policy/permission refusals call for respecting the boundary, not changing tools to bypass it. Timeouts and interruptions call for inspecting completed effects, not assuming rollback. Skipped operations remain unexecuted. Recognized selected-runner setup failures are kept separate from observed code/assertion failures; only the latter should drive application repair. Stale/unseen source failures call for a targeted fresh range, not a new repository inventory. Unknown effects still stop automatic continuation before any recovery advice can authorize a retry. Unclassified extension errors retain their diagnostic rather than receiving a guessed cause.

Recovery results can carry bounded `recovery_advisory` records alongside producer data. Each
record identifies the source call, evidence basis, required observation and capability state;
native launch suggestions include schema-validated arguments only when the recipient can use
the tool. Fixed worker allowlists never imply a parent loader. These records and their rendered
guidance are advisory, not retry authority, permission grants or an execution queue.

Configured checks with arbitrary names can declare the runner separately:

```toml
[verification.runners.input-boundaries]
program = "python3"
module = "unittest"
```

Declarations must name an existing unique configured check. Only completed launches with a literal matching runner and narrowly recognized missing program/module diagnostics are classified as environment-unavailable. Affirmative compiler, source-located diagnostic or assertion evidence can establish `code_failed`; a complete nonzero exit alone is `failed_unknown_cause`. Both remain failed checks, but an unknown cause does not authorize automatic application repair or earn reproduction credit: inspect bounded diagnostic/runner evidence to distinguish a source defect from a setup prerequisite, without weakening the acceptance criterion. Exact bounded diagnostics matching a nonbehavioral pre-edit code failure outside declared requirements remain disclosed without automatic unrelated repair; changed, absent or oversized diagnostics remain actionable. Requirement membership does not invent behavioral credit. Rust source compilation selects each owning package, including non-default members; manifests, lockfiles and shared root changes intentionally broaden to workspace checks.

## Selective independent review

```toml
[verification.review]
enabled = true
min_changed_files = 4
sensitive_paths = ["src/auth", "crates/protocol"] # component prefixes, not globs
max_rounds = 1 # repair opportunities, 1..3
```

When `enabled` is unset, review runs only if a `coach` model role is assigned and its route passes the escalation privacy gate (a loopback endpoint, or `escalation.allow_remote = true` for a hosted one). Without a usable coach it stays off and emits no skipped-review notices. An explicit `enabled = false` always disables it, and `enabled = true` always requests it. Reuses the existing configured coach/advisor and escalation privacy/approval path, not another agent framework. Review is selected for the changed-file threshold, sensitive path prefixes, or uncheckpointed effects. It is disabled in hermetic and plan mode and cannot expand the launch tool allowlist. The repair/review allowance is persisted per external instruction so stop hooks cannot reset it. A new user instruction gets a fresh allowance. Refusals or malformed reviews are disclosed and do not retry automatically. `strict_requirements` stays off by default.

The reviewer receives a bounded proposed reply, recent evidence, declared-check coverage and up to two **partial scoped checkpoint diffs**. It does not run tools or certify the full patch. Findings remain unverified suggestions; local executable checks are authoritative, and unresolved findings stay disclosed. This is selective independent judgment, not measured proof of better real-model outcomes.

## Tool discovery

Discovery keeps exact-name priority, eligible-catalog boundaries, deterministic ranking, bounded activation and abstention for unknown queries. Common singular/plural capability terms share lexical evidence (for example `symbol definition` can match `symbols definitions`). This is deliberately not a semantic router or proof of an optimal tool choice. Measure selection and composition on independently checked tasks using [tool-effectiveness.md](tool-effectiveness.md); using more tools is not the objective.

Catalog and discovery results include advisory `readiness` (`ready`, `missing_prerequisite`,
`approval_required`, `unsupported`, or `unknown`) and operation-specific prerequisites where
known. These are side-effect-free observations from configuration and owner-held metadata, not
service probes. Automatic routing defers known blockers. Explicit discovery preserves exact-name
priority and ranks readiness only after equal relevance; blocked exact names remain discoverable.
Unknown tools remain eligible, and explicit discovery/load does not hide tools merely for missing
prerequisites. Readiness grants no permission and provides no live-service proof.
