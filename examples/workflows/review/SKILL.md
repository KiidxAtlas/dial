---
name: review
description: Produce an evidence-backed read-only review and portable session report without changing the project.
---

# Review and handoff

## Outcome
A review with locations, consequences, observed checks and unresolved uncertainties, suitable for
exporting with the existing session CLI.

## Prerequisites and permissions
Use an already configured model and a local checkout. No credentials belong in this skill or its
report. Obtain the requested diff/ref and review scope. Start Dial in read-only mode for enforcement;
this procedure itself does not grant or restrict tools. Commands that run checks still require their
normal execution permission; do not run builds with side effects under an asserted read-only promise.

## Workflow
1. Establish the requested diff and relevant conventions. Read only the changed contracts and their
   consumers needed to resolve a concrete question; do not repeat unchanged reads for ceremony.
2. Trace correctness, compatibility and authority boundaries. Distinguish findings from speculation.
3. If a focused check is necessary, explain its prerequisites/effects and obtain normal permission;
   otherwise report static evidence only. Do not edit source to demonstrate a suspected defect.
4. Summarize severity, location, trigger and consequence. A clean review is a valid outcome.
5. When the user wants a portable report, use `dial sessions --all` to locate the exact session ID,
   then `dial sessions export <id> <path>` to an explicitly chosen destination. Export may contain
   private project text: review/redact before sharing. This export writes a report, not a replayable
   workflow or a new permission grant.

## Verification and report
Cite inspected code and actual check results. Preserve unavailable-check and partial-output labels.
Verify an authorized report destination exists before claiming export success. Stop when the requested
review is answered; hand implementation back to the user rather than silently changing modes.
