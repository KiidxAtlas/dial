# Requirement-aware evidence

Dial can map **declared requirements to observed verification**, without interpreting an English completion claim as proof. This is opt-in operator configuration; ordinary answers still need no checklist.

```toml
[verification]
automatic_checks = true
behavioral_checks = ["input-boundaries", "error-contract"]

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

Coverage is `declared_requirements_observed` only for this declared mapping. Open tasks still take precedence as `open_obligations`. Undeclared requests remain `not_independently_assessed`. The report does not claim that the contract covers the entire user request, that tests are sufficient, or that appearance is correct. `TurnReport` and execution outcomes are unchanged; uncovered declarations remain an explicit evidence gap rather than introducing unbounded retries or making a renderer a permission grant.

Uncovered declarations also prevent positive behavioral outcome credit and successful executable
recipe capture/reuse credit, even when an unrelated behavioral check passed. Empty legacy
declarations retain the existing evidence rules; neither declarations nor summaries are new learning labels.

## Recovery by cause

Parent and worker settlement share bounded, cause-specific advice. Typed policy/permission refusals call for respecting the boundary, not changing tools to bypass it. Timeouts and interruptions call for inspecting completed effects, not assuming rollback. Skipped operations remain unexecuted. Recognized selected-runner setup failures are kept separate from observed code/assertion failures; only the latter should drive application repair. Stale/unseen source failures call for a targeted fresh range, not a new repository inventory. Unknown effects still stop automatic continuation before any recovery advice can authorize a retry. Unclassified extension errors retain their diagnostic rather than receiving a guessed cause.

## Tool discovery

Discovery keeps exact-name priority, eligible-catalog boundaries, deterministic ranking, bounded activation and abstention for unknown queries. Common singular/plural capability terms share lexical evidence (for example `symbol definition` can match `symbols definitions`). This is deliberately not a semantic router or proof of an optimal tool choice. Measure selection and composition on independently checked tasks using [tool-effectiveness.md](tool-effectiveness.md); using more tools is not the objective.
