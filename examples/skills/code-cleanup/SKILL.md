---
name: code-cleanup
description: Audit or remove dead code, duplicated behavior and demonstrated maintenance cost. Use file-structure-cleanup for path and ownership migrations.
---

# Code cleanup

## Outcome
Remove demonstrated maintenance cost while preserving supported behavior.

## Scope
Use for evidence-backed cleanup across a module or repository. A small local edit needs no cleanup campaign. Audit and plan requests are read-only; implement only when requested. Use debugging for a reported failure and implementation-quality for feature work.

## Workflow
1. Establish the requested scope, behavior to preserve and existing project checks. Exclude generated output, dependencies and unrelated changes. For broad cleanup, keep an ordered plan; persist a plan only when requested or useful for a multi-phase migration.
2. Find candidates with available analyzers, symbol navigation and targeted source search. Zero text references do not establish deadness: check exports, dynamic registration, configuration, reflection and external consumers.
3. Before deletion, inspect initialization, import side effects, generated discovery and compatibility. Before consolidation, compare contracts, side effects, errors and intentional variation, not just similar syntax.
4. Record meaningful findings with location, evidence, confidence, consequence and risk. Distinguish verified internal removals from uncertain public or dynamic uses. File length, nesting and names alone are not defects; do not invent scores or findings.
5. When implementation is authorized, change one coherent reason and verification boundary at a time. Keep behavioral rewrites separate from mechanical removal. Reuse existing owners; avoid cosmetic churn and speculative abstractions.

## Verification
Run targeted checks after each coherent batch; expand when shared contracts change. Inspect the final diff for unintended behavior changes, deletion and unrelated work. Resolve regressions before proceeding. Do not install dependencies just to audit, reset others' work, or commit without authorization.

## Report
For an audit, give prioritized findings and actionable next steps, including uncertainty and checks needed. For implementation, give changes, exercised checks and residual risks. A clean audit is valid; stop when the requested scope is addressed.
