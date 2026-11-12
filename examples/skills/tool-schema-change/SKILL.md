---
name: tool-schema-change
description: Change a built-in tool parameter schema without breaking recorded sessions: contract identity, additive properties, history compatibility tests. Use for schema edits.
---

# Tool schema change

## Outcome
A built-in tool schema change that keeps history recorded under the old schema resumable, with the contract identity and compatibility tests updated in their owners.

## Scope
Use when a built-in tool parameter, enum, default or property changes in `crates/dial-tools`. Not for: tool behaviour that keeps its schema, MCP or extension tools, or documentation-only wording. For a removed or renamed property, stop and report that it needs a legacy projection decision before editing.

## Workflow
1. Locate the schema and its registration: `crates/dial-tools/src/tools/surface/` and the tool's own module. Read the current pinned tests in `crates/dial-tools/src/tools/surface/contracts/tests.rs` for the same tool before editing.
2. Classify the change. A new optional property whose absence keeps the old meaning is additive: add its name to `ADDITIVE_PROPERTIES` in `crates/dial-tools/src/tools/surface/contracts.rs` (line 25 area), with a doc line that says why. A new enum value uses `ADDITIVE_ENUM_VALUES` in the same file. A required property, changed type, changed default meaning or removed property is non-additive: do not list it; it needs a projection in `legacy_arguments` (line 589 area) and a refusal test.
3. Keep the JSON schema the single source: the identity is computed from the schema shape by `identities` and `shape_sha256` in `contracts.rs`. Do not hand-edit a stored hash.
4. Add or extend one history test next to the closest existing one, for example `history_recorded_before_an_additive_builtin_property_stays_valid` in `contracts/tests.rs`. Build the old call shape as data; do not replay a tool.
5. Run the focused module tests (see the focused-rust-verification skill), then `cargo fmt --check`.

## Verification
- `make test-module MOD=surface::contracts CRATE=dial-tools` must pass. Its filter matches the contract tests and the surrounding surface tests.
- If the tool's own module changed, also run `make test-module MOD=<tool module path> CRATE=dial-tools`.
- Schema-hash-sensitive resume tests live in the same module; a failing resume test is a compatibility finding, not a reason to update the expected identity.
- If `cargo` or the workspace cannot build here, report that the compatibility suite did not run; do not present a schema read as a test.

## Report
Name the tool, property and classification (additive or non-additive), the files changed, the exact test command and its result, and any old-history case not covered.
