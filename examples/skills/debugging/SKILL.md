---
name: debugging
description: Diagnose failures, wrong results, crashes or intermittent behavior; implement and verify the causal fix when requested.
---

# Debugging

## Outcome
Establish an evidence-backed cause and, when authorized, a verified correction.

## Scope
Use for a reported failure or unexplained behavior, not general feature implementation or speculative cleanup. Diagnosis-only requests end with findings; fix requests continue through implementation and verification.

## Workflow
1. Separate expected behavior, observed behavior and environment/version. Accept the user's observation as evidence. Reproduce only to resolve uncertainty, not to dispute the report.
2. Inspect the named failing path and relevant changes first. Check live configuration or processes when saved settings may differ from runtime. Locate the earliest divergence and trace its inputs backward.
3. Keep a small set of plausible causes. Choose the cheapest observation that distinguishes them and state what each outcome would mean. Prefer existing tests, logs and debugger state. Add temporary instrumentation only for a specific uncertainty; exclude secrets and sensitive payloads.
4. Change one causal factor at a time. Update rejected hypotheses rather than repeating equivalent checks. For intermittent failures, inspect timing, shared state, cancellation, retries and ownership; a failed reproduction does not disprove the bug. For performance, distinguish startup, processing, generation, waiting and contention.
5. When authorized, repair the violated contract at its narrowest appropriate owner. Preserve unrelated changes. Inspect analogous sites to establish the defect's scope, but report unrelated defects separately. Do not hide failures with suppressed errors, weakened tests or unconditional retries.

## Verification
When practical, observe a regression test fail before the fix and pass afterward. Otherwise exercise the original trigger and explain the limit. Cover a relevant failure boundary, run focused checks and expand for changed interfaces. Remove temporary instrumentation and inspect the final diff.

## Report
Give the cause and evidence, changed behavior, actual checks and remaining uncertainty. Label an untested explanation as a hypothesis, not a confirmed cause. Stop after the reported failure and relevant checks are addressed.
