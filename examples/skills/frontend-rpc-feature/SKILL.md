---
name: frontend-rpc-feature
description: Add a frontend-neutral feature with RPC parity: engine owner, rpc method, docs schema entry and slash-command classification. Use when a terminal command needs a client method.
---

# Frontend-neutral feature with RPC parity

## Outcome
The behaviour lives in its engine owner, is callable through `dial rpc` with a schema entry, and every new slash command is classified so clients know where it lives.

## Scope
Use for a new user-visible feature or a new slash command that other clients (RPC, ACP, a GUI) need. Not for a purely terminal-presentation change (layout, key bindings, colours) or an internal refactor with no new operation. Read `ARCHITECTURE.md` section "Frontend boundary" and `AGENTS.md` "Working conventions" first.

## Workflow
1. Find the engine owner by layer: `agent`, `tools`, `storage` or `config`. The `ui` and `app` handlers only collect input, call the owner and render its result. Do not put validation in a terminal handler.
2. Add the operation to the owner, with its validation and a coded refusal for bad input.
3. Expose it as an RPC method: add the entry to the method table `METHODS` in `crates/dial-protocol/src/lib.rs`, implement the dispatcher in `crates/dial-app/src/rpc/` (for example a new module next to `settings.rs` or `jobs.rs`), and add the matching entry in `docs/rpc-schema.json`. The test `schema_requests_match_the_method_table_and_the_dispatchers` in `crates/dial-app/src/rpc/tests.rs` fails when these three disagree.
4. Classify each new slash command in `crates/dial-app/src/rpc/parity.rs` (`parity(name)` returns `Method`, `Client` or `Terminal` with a reason). Do not add commands to `TERMINAL_BASELINE` in `crates/dial-app/src/rpc/parity/tests.rs`; that list may only shrink.
5. Reuse an existing event when its structured fields (ids, phases, outcomes, paths) already describe the outcome; add a new event only when none does. Check the event-name scan tests in `crates/dial-app/src/rpc/tests.rs` before adding one, so the same outcome is not announced twice under two names.
6. Documentation is part of the authorized feature, not a review-time edit: update the matching page (`docs/rpc-and-acp.md` for methods, `docs/client-contracts.md` for parity) and add an entry under `## [Unreleased]` in `CHANGELOG.md`. In a review-only task, do not edit them; name them as missing.

## Verification
- `make test-module MOD=rpc::parity CRATE=dial-app` covers classification and the baseline.
- `make test-module MOD=rpc::tests CRATE=dial-app` covers the schema-to-dispatcher agreement and the event-name scan.
- `make test-integration` runs the architecture layering rules (`tests/integration/architecture.rs`); run it when a crate dependency or module moved.
- If `docs/rpc-schema.json` or the method table changed, the RPC tests above are required, not optional.
- A terminal-only classification needs a reason that names the alternative; report it rather than leaving it unclassified.

## Report
List the engine owner, the method name, the event reused or added, the classification of each new command, the files changed and the exact test results. For a feature task, list the documentation and CHANGELOG entries you updated; for a review task, list the ones still missing.
