---
name: implementation-quality
description: Implement features and fixes or review patches for correctness, maintainability, contract compatibility and appropriate verification.
---

# Implementation quality

## Outcome
Deliver the requested behavior with a small, coherent change that fits the project.

## Scope
Use for substantial implementation or patch review. Small, obvious edits need no additional ceremony. Review requests produce findings, not source edits. Use debugging for uncertain failure causes and specialized cleanup skills for broader removal or path migrations.

## Workflow
1. Establish the outcome, acceptance criteria, affected interfaces and behavior to preserve. Read relevant project conventions and the nearest working example, callers and tests. Use symbol navigation when available and source search for configuration and dynamic references.
2. Reuse existing owners, types, helpers and error conventions. Respect crate/module boundaries. Add abstractions only for demonstrated maintenance benefit; do not impose arbitrary file lengths, class structures or helper extraction.
3. Check consumers when changing signatures, schemas, defaults, serialization or persisted state. Preserve backward compatibility or make the requested migration explicit. Follow existing parameter validation and configuration conventions.
4. Address applicable boundaries: missing/empty values, partial failure, ownership, cleanup, cancellation, retries and concurrent updates. Validate untrusted inputs, retain useful error context and avoid secret logging or misleading success. Do not add machinery for irrelevant cases.
5. Preserve unrelated work and formatting. Optimize only a demonstrated bottleneck or clearly costly operation in the changed path. In review mode, trace each finding to a concrete trigger and consequence; separate defects from style preferences and state uncertainty.

## Verification
Exercise the requested behavior and an important failure or boundary case. Prefer observable outcomes over implementation-detail mocks; isolate unavailable dependencies without claiming live integration. Run the narrowest established test/type/lint/build checks and expand when shared interfaces warrant it. For visual changes, inspect the rendered result or state why rendering was unavailable. Inspect the final diff for churn, debug remnants and stale consumers. Never claim an unrun check passed.

## Report
For implementation, give outcome, actual checks and material limits. For review, give prioritized, cited findings; a clean review is valid. Stop once the requested acceptance conditions and relevant checks are satisfied.
