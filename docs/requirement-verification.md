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

# Optional: needs current rendered browser pixels delivered to a model request.
# [verification.requirements.appearance]
# visual = true
```

Requirements name unique configured checks, not arbitrary commands or model-authored prose. A requirement needs at least one check or a visual obligation. Up to 50 requirements and 16 checks per requirement are accepted; invalid, duplicate or ambiguous check references fail configuration validation. Legacy configurations default to no declaration.

When automatic verification is enabled, a declared check is selected regardless of its extension filter: requirements are obligations, not just changed-file hints. Normal check authorization, cancellation, deadlines and repair policy still apply. With automatic checks disabled, model-requested configured commands can supply the same evidence; no process is automatically started merely by declaring a requirement.

`completion_evidence.requirements` records each requirement, its check names/statuses, visual status, and the exact effect revision. Every mapped check must have an observed passing result for the latest revision. Missing, stale, failed, cancelled, timed-out, zero-test and environment/permission-unavailable results leave the requirement uncovered. A later edit or uncheckpointed operation invalidates earlier evidence. Baselines cannot satisfy a later revision. A visual obligation needs current **browser** pixels delivered in a subsequent successful model request, not source/DOM text, an image attachment, or a pending capture.

Coverage is `declared_requirements_observed` only for this declared mapping. Open tasks still take precedence as `open_obligations`. Undeclared requests remain `not_independently_assessed`. The report does not claim that the contract covers the entire user request, that tests are sufficient, or that appearance is correct. By default uncovered declarations remain an explicit evidence gap; `strict_requirements = true` instead refuses completed settlement until every declared check and visual obligation has current observed evidence. Both explicit `finish` and plain replies use this guard, with a final freshness check after hooks. Missing/unavailable evidence produces an honest blocked or incomplete outcome, not unbounded retries or a renderer permission grant. Strict mode requires verification enabled; an empty contract creates no obligations.

Uncovered declarations also prevent positive behavioral outcome credit and successful executable
recipe capture/reuse credit, even when an unrelated behavioral check passed. Empty legacy
declarations retain the existing evidence rules; neither declarations nor summaries are new learning labels.

## Recovery by cause

Parent and worker settlement share bounded, cause-specific advice. Typed policy/permission refusals call for respecting the boundary, not changing tools to bypass it. Timeouts and interruptions call for inspecting completed effects, not assuming rollback. Skipped operations remain unexecuted. Recognized selected-runner setup failures are kept separate from observed code/assertion failures; only the latter should drive application repair. Stale/unseen source failures call for a targeted fresh range, not a new repository inventory. Unknown effects still stop automatic continuation before any recovery advice can authorize a retry. Unclassified extension errors retain their diagnostic rather than receiving a guessed cause.

Configured checks with arbitrary names can declare the runner separately:

```toml
[verification.runners.input-boundaries]
program = "python3"
module = "unittest"
```

Declarations must name an existing unique configured check. Only completed launches with a literal matching runner and narrowly recognized missing program/module diagnostics are classified as environment-unavailable. Application import/assertion failures and complex shell commands remain conservative code failures. Exact bounded diagnostics matching a nonbehavioral pre-edit failure outside declared requirements remain disclosed without automatic unrelated repair; changed, absent or oversized diagnostics remain actionable. Requirement-mapped checks, declared behavioral checks and reproductions still require repair within their authorized scope. Requirement membership does not invent behavioral credit. Rust source compilation selects each owning package, including non-default members; manifests, lockfiles and shared root changes intentionally broaden to workspace checks.

## Selective independent review

```toml
[verification.review]
enabled = true
min_changed_files = 4
sensitive_paths = ["src/auth", "crates/protocol"] # component prefixes, not globs
max_rounds = 1 # repair opportunities, 1..3
```

Off by default. Reuses the existing configured coach/advisor and escalation privacy/approval path, not another agent framework. Review is selected for the changed-file threshold, sensitive path prefixes, or uncheckpointed effects. It is disabled in hermetic and plan mode and cannot expand the launch tool allowlist. The repair/review allowance is persisted per external instruction so stop hooks cannot reset it. A new user instruction gets a fresh allowance. Refusals or malformed reviews are disclosed and do not retry automatically.

The reviewer receives a bounded proposed reply, recent evidence, declared-check coverage and up to two **partial scoped checkpoint diffs**. It does not run tools or certify the full patch. Findings remain unverified suggestions; local executable checks are authoritative, and unresolved findings stay disclosed. This is selective independent judgment, not measured proof of better real-model outcomes.

## Tool discovery

Discovery keeps exact-name priority, eligible-catalog boundaries, deterministic ranking, bounded activation and abstention for unknown queries. Common singular/plural capability terms share lexical evidence (for example `symbol definition` can match `symbols definitions`). This is deliberately not a semantic router or proof of an optimal tool choice. Measure selection and composition on independently checked tasks using [tool-effectiveness.md](tool-effectiveness.md); using more tools is not the objective.
