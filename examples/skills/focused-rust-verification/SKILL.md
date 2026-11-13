---
name: focused-rust-verification
description: Pick and run the narrowest Rust checks for a change: module tests, one crate, the integration binary and the clippy gate, reporting skipped tiers honestly. Use when a Rust change needs checks.
---

# Focused Rust verification

## Outcome
The changed behaviour is checked with the smallest suite that covers it, and the report says exactly which commands ran, which did not and why.

## Scope
Use after Rust edits, or to decide which checks apply. Not for writing new tests (use `test-runner`), reviewing code without running it (use `rust-review`), or running the whole workspace by default.

## Workflow
1. Find the commands from the repository, not from memory: `AGENTS.md` "Checks", the `Makefile` targets and CI. Common ones: `cargo fmt --check`; `make test-module MOD=<path> [CRATE=<crate>]` (`CRATE` such as `dial-tools` builds only that crate; the filter is a substring of the test path); `make test-integration` (the `dial` integration binary); `make lint` (clippy with `-D warnings`, whole workspace).
2. Choose the module path from the file path: for `crates/dial-tools/src/tools/surface/contracts.rs` use `MOD=surface::contracts CRATE=dial-tools`. Use a larger filter only when the change crosses modules.
3. Run the focused command after the last relevant edit. Run `make lint` when public signatures, lints or several crates changed. Run `make test-integration` when architecture, layering or CLI wiring changed.
4. Optional discovery: if a `verify` tool with a `plan` option is offered, use it read-only to list configured checks and requirement mappings instead of guessing names. Treat that plan as discovery only; it grants no verification credit, and its absence is not a failure.
5. Unavailable prerequisite (no toolchain, no display, no model, no network, no `cargo-nextest`): first look for a supported path that already exists in the repository or the installed environment, such as another documented make target for the same tier or an existing runtime; run it and report it under the original requirement. Refuse unapproved installation of tools. Do not substitute a different suite and present it as the requested check. If nothing supported exists, report the command as not run with the missing prerequisite.

## Verification
- Record each command, its exit status and the pass/fail counts from its output.
- A timeout, a missing tool or a skipped tier is not a pass. Say which evidence is stale if an edit came after a run.
- Reuse an earlier passing result only when the code, command and environment are unchanged; say that it was reused.
- Do not weaken an assertion, add `#[ignore]`, or retry until green. Diagnose a failure first.

## Report
State the commands run in order with exit status, the counts, the failures with their first diagnostic line, the tiers not run (for example `make test-smoke`, `make test-mlx`) and why, and any remaining risk.
