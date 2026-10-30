---
name: test-runner
description: Author and run focused regression tests, diagnose failures and report verification evidence. Use for a defined behavior or an explicitly requested suite.
---

# Test runner

## Outcome
Produce meaningful test coverage and an accurate account of what was exercised.

## Scope
Use for independent test work or substantial verification, directly or under a parent agent's assignment. Follow the assigned file ownership and acceptance criteria. Do not change production code unless authorized. A skill is guidance, not a command runner or a structured-result API.

## Workflow
1. Establish the changed behavior, important boundaries and requested verification tier. Find commands in repository instructions, manifests, CI and nearby tests; the skills catalog does not discover tests. Use a focused suite unless the user or assignment requests broader coverage.
2. Inspect existing suites, fixtures and conventions before authoring tests. Extend the established owner instead of creating a parallel harness or scattering one-off files. Identify the distinct changed contracts, invariants and important failure boundaries, and what is already covered. If an existing test catches the regression, reuse it; add or extend coverage only for a real gap.
3. For a bug fix, observe a failing regression before the fix when practical. If the fix already exists or the failure cannot run here, report that limit rather than claiming a before/after demonstration.
4. Check prerequisites and isolate test state. Do not install tools, contact external services or use real credentials merely to make a suite run. Distinguish unit doubles from real integration, rendering, device and live-model checks. Coordinate shared builds and wait for relevant edits to settle.
5. Run the project's actual command with enough time to finish. For long jobs, use a managed background process and collect its exit status. Diagnose the named failure before retrying. Never weaken a valid assertion, suppress an error or broaden retries to manufacture a pass.

## Test design and stopping rule
- Choose cases by distinct risk, not by the number of examples in the request. Cover representative success, meaningful boundary classes and relevant errors; skip equivalent permutations unless they expose a different failure. Every added test should have a clear answer to “what plausible regression would this catch that existing tests miss?” Stop when those risks are covered, not at a test-count or coverage-percentage target.
- Use table-driven/parameterized cases for inputs sharing a contract and setup, with case labels in failure diagnostics. Keep separate tests for genuinely different behavior; do not make one giant scenario to hide test count. Reuse small fixtures/helpers, but do not invent a test framework or generic fixture machinery for a handful of cases.
- Prefer the lowest layer that proves the behavior. Unit tests cover logic and boundaries; integration tests prove component contracts/wiring; reserve end-to-end tests for critical flows that lower layers cannot establish. Avoid repeating the same input matrix at every layer. Use property tests for broad invariants when the project's tools and risk justify them, not a catalog of manually enumerated examples.
- Assert observable outputs, state transitions and error semantics. Do not restate constants, inspect source substrings, mirror the implementation to compute expected results, or pin prose/incidental formatting. Exact text/bytes or snapshots are appropriate only when that representation is a required contract; use a complete assertion rather than many substring checks.
- Keep tests independent, deterministic and bounded. Use per-test temporary state, controlled clocks/seeds and resource cleanup; avoid sleeps, order dependence, shared mutable globals, real credentials and live services in the default suite. Keep genuine service/device/live-model checks in explicit optional integration tiers and report when they were not run.
- Before handing back tests, remove duplication introduced by this change, not meaningful existing coverage. No arbitrary caps, deleted edge-case protection or weakened assertions merely to make the suite smaller. Report the distinct behavior/risks exercised, not test count as evidence of quality.

## Verification
After the final relevant edit, run the focused regression and affected boundary checks. Expand to integration/lint/build checks where shared contracts warrant it. Preserve useful diagnostics without exposing secrets. A timeout, skipped tier or missing prerequisite is not a pass. If inputs change concurrently, say which evidence is stale.

## Report
Give exact commands, exit status, passed/failed/skipped counts when available, failure diagnostics and unexercised tiers. Under delegation, identify changed test files and remaining integration work for the parent. Do not claim a full suite passed when only a subset ran.
