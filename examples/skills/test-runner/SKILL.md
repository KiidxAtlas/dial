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
2. Inspect existing fixtures and conventions before authoring tests. Keep tests with the project's established owner. Assert observable behavior and errors, not prose, incidental formatting or private implementation details.
3. For a bug fix, observe a failing regression before the fix when practical. If the fix already exists or the failure cannot run here, report that limit rather than claiming a before/after demonstration.
4. Check prerequisites and isolate test state. Do not install tools, contact external services or use real credentials merely to make a suite run. Distinguish unit doubles from real integration, rendering, device and live-model checks. Coordinate shared builds and wait for relevant edits to settle.
5. Run the project's actual command with enough time to finish. For long jobs, use a managed background process and collect its exit status. Diagnose the named failure before retrying. Never weaken a valid assertion, suppress an error or broaden retries to manufacture a pass.

## Verification
After the final relevant edit, run the focused regression and affected boundary checks. Expand to integration/lint/build checks where shared contracts warrant it. Preserve useful diagnostics without exposing secrets. A timeout, skipped tier or missing prerequisite is not a pass. If inputs change concurrently, say which evidence is stale.

## Report
Give exact commands, exit status, passed/failed/skipped counts when available, failure diagnostics and unexercised tiers. Under delegation, identify changed test files and remaining integration work for the parent. Do not claim a full suite passed when only a subset ran.
