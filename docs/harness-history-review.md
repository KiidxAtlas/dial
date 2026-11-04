# Session-history harness review

Reference: [Arcos, “Harness Engineering: How to Build AI Agents That Never Make the Same Mistake Twice”](https://x.com/0xarcos/status/2115810770747339140).

The useful engineering standard is to capture an observed failure, locate its owning boundary,
encode the smallest guard, and replay the failure before and after the change. A passing regression
covers its tested conditions; it is not a guarantee that an agent can never make a similar mistake.
Article performance claims were not independently reproduced in this review.

## Scope and evidence limits

This review used the current project's available session inventory (requested limit 500), newest
500 deduplicated prompts, targeted cross-session conversation searches, prior audit reports, and
current source inspection. It did **not** read every historical transcript line by line. Other
projects' histories were not accessed. Session status labels such as pending or interrupted do not
by themselves establish an unfinished user task or a product defect.

The November 1 audits describe an older revision. Their findings are candidates, not current bug
proof. Their [follow-up register](audit-2026-11-01-follow-up.md) distinguishes implemented changes,
retained tradeoffs, external prerequisites and unverified acceptance. Historical passing tests are
not fresh verification of the current checkout. Other live sessions own UI/performance and
subagent changes; those changes are outside this review's repair scope.

## Findings and disposition

### 1. Rejected check selection can mutate reproduction obligations

**Class:** overconfident / state integrity. **Disposition:** repaired with a replayed regression.

`agent/verify/check_tool.rs::check_tool` originally called `register_repro` before validating
`names`. Replacing a reproduction clears its old observation and removes its failed-check entry.
An unknown requested name then returns an error without restoring those obligations.

A concrete trigger is a failed reproduction named `bug`, followed by a request containing both
its replacement and `names: ["missing"]`. Although the request is rejected, the original failure
has already disappeared. New registration with an invalid selector also leaves unwanted state.

The guard belongs at tool argument/selection validation, before registration, not in another
completion prompt. The regression must check registration, full evidence snapshot, retained failure
bookkeeping and final-gate selection—not just whether the tool returns an error.

### 2. `repro` plus `failed_only` silently registers without an initial run

**Class:** clumsy / misleading outcome. **Disposition:** repaired with a replayed regression.

Registration originally happened before filtering for previous `code_failed` observations. A new
reproduction has no such observation, so the command is excluded and the tool returns a successful
“nothing run” message while retaining its registration. Replacing a failed reproduction can also
clear its evidence and then select nothing.

The coherent contract is to refuse this combination before changing registration. Register and
run first; use `failed_only` or the registered name for subsequent attempts. Cover new registration
and replacement, including absence of command side effects and preservation of prior evidence.

### 3. Text recovered as executable tool calls

**Class:** reckless. **Disposition:** historical finding already mitigated; no duplicate repair.

The old audit reported hosted endpoints lifting quoted text into calls without checking names.
Current `crates/dial-llm/src/llm/mod.rs::lift_calls` restricts recovery to local/private endpoints,
checks offered names, and parses into a candidate so rejected text is preserved. Inspecting
`text_tool_calls::apply` alone misses this caller-owned boundary. Existing recovered-call approval
policy must remain intact. No live model test was performed in this review.

### 4. Search evicts witnesses needed for an edit

**Class:** amnesiac / clumsy. **Disposition:** historical mitigation exists.

The earlier logic audit identified broad search pages evicting targeted source witnesses. Current
`crates/dial-tools/src/tools/edit/hashline.rs` contains a separate broad-search recording policy.
This does not establish that every present edit refusal is a harness defect. Read-before-edit,
source freshness and memory-pressure refusal must not be weakened to conceal failures.

### 5. Passing one check erases an unrelated failed check

**Class:** overconfident. **Disposition:** existing guard and coverage; no new defect established.

The historical failure is now explicitly represented by owning verification tests, including
`strict_required_subset_cannot_discharge_an_unrelated_failed_check` in
`agent/verify/checks/tests.rs`. Preserve command-based reconciliation: passing B is not evidence
that A passed. Test presence is not a fresh passing execution.

### 6. Machine/profile benchmark report comparison

**Class:** blind / clumsy. **Disposition:** earlier saved finding is stale.

`examples/benchmarks/compare.py::direct_report` now supports machine configuration selection and
requires an explicit choice for ambiguous reports. It also validates run and verification
contracts. Do not reimplement the old missing feature or treat legacy report analysis as new
model-quality proof.

### 7. Existing safety ratchets

**Class:** reckless. **Disposition:** preserve existing work; not reverified wholesale here.

The audit follow-up names implemented guards for safe-shell expansions, Git metadata mutations,
session-writer identity races, recovered-call approval, SSH quoting and secret inheritance.
Their existence is a reason to check the current owner before patching an old report, not a claim
that all security paths or platform tests passed in this review.

### 8. Slow sessions, subagents and UI

**Class:** flooded / efficiency. **Disposition:** coordinated with other live sessions.

The history repeatedly reports lag, long builds, excessive work between requests, and subagent
cost. Concurrent work already targets those owners. Avoid overlapping edits and duplicated
pipelines. No wall-clock or model-success improvement is claimed by this review's verification fix.

### 9. Generated applications pass checks but fail actual use

**Class:** overconfident. **Disposition:** real historical concern; no new causal harness bug proven.

Snake and Tetris session prompts repeatedly distinguish code/test inspection from playing or
visually inspecting the requested application. Existing completion/evidence gates do not turn
unit tests into end-to-end proof. Those games were not executed in this review; application bugs
are not automatically attributable to a missing harness guard. Require target-specific runtime
or visual evidence when the task needs it, and disclose unavailable acceptance tiers.

### 10. Implemented fixes still have acceptance gaps

**Class:** overconfident / evidence quality. **Disposition:** explicitly open.

The audit follow-up leaves native Linux/Windows behavior, live collaboration, remote SSH and hosted
release administration partly unverified. Cross-compilation, authored CI and mocked protocols are
not substitutes for those runtime outcomes. Retain their existing acceptance boundaries.

### 11. Open owner decisions

**Class:** reckless / maintainability. **Disposition:** no unilateral policy change.

Storage retention, portable process-group signaling races, release protections and provider
subscription-policy tradeoffs remain explicit limitations or decisions in the follow-up register.
Do not invent destructive retention defaults, change permissions, or claim GitHub protections
were configured based only on local code.

### 12. Repeat-failure metrics are not established

**Class:** evidence quality. **Disposition:** no invented metric or parallel ledger.

Historical complaints, attempted fixes and tests are not a consistently classified event dataset.
This review cannot honestly compute repeat-failure rate or time-to-guard. Existing verification
and reliability owners should be extended if such telemetry is later requested; adding a second
failure pipeline or a prose ledger that claims unexecuted proofs would defeat the article's goal.

## Repair acceptance

The two selected defects share the existing `check_tool` owner. No new tool, setting, persisted
schema, permission exemption or model prompt is required. Tests belong in its existing out-of-line
test module. Acceptance requires observing the new regressions fail before the production change,
then pass afterwards, plus running the owning check-tool suite. Actual command results belong in
the delivery report; the findings and test names in this document are not verification receipts.

### Observed verification

- Before the production fix, `cargo test --locked -p dial-agent --lib reproduction_ -- --nocapture`
  reported 8 passed and 2 failed. The two new regressions failed on unwanted registration and
  the non-error “nothing run” response. After the production fix the same command passed all 10.
- After the final test additions and formatting,
  `cargo test --locked -p dial-agent --lib agent::verify::check_tool::tests -- --nocapture`
  passed all 26 tests, including both regressions and valid mixed-selection replacement coverage.
- `cargo clippy --locked -p dial-agent --lib --tests -- -D warnings` passed.
- `rustfmt --edition 2024 --config skip_children=true --check` on `check_tool.rs` and its
  `tests.rs` passed. An initial command mistakenly also supplied a directory and was rejected;
  the corrected file-only command passed. No source repair was needed for that command error.
- The two changed Rust files had identical SHA-256 identities before and after the final suite,
  Clippy and format checks. The controller inspected the retained execution output, including
  individual test names and command exit markers, rather than relying only on a worker summary.

These are focused macOS checks, not a fresh full-workspace run or live-model acceptance. No UI,
external service, native Linux/Windows runtime, release or model-quality improvement was verified.
