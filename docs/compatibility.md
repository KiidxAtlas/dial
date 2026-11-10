# Public API compatibility

Repository searches establish internal usage, not the absence of external Rust callers. Dial's
root facade also exposes APIs from the workspace crates. Cleanup must preserve those paths until
there is an explicit migration decision; a `pub` item with no internal callers is not automatically
dead code.

## Reviewed surfaces

| Surface | Decision | Migration or removal condition |
| --- | --- | --- |
| `dial::runtime::template::register_partial` | Keep supported. It is the public registration path for process-wide template partials; there is no equivalent replacement. | Do not deprecate without a replacement for custom partials and a migration plan. Registration, replacement and failed-replacement behavior are covered by a public-path integration test. |
| `dial::config::settings_store::{write_private, validate_private_path}` | Keep compatibility exports. Live internal callers still use them; both forward to the canonical `dial::platform::atomic` owner. | Prefer `platform::atomic` in new code. Migrate existing callers before proposing deprecation; keep the private-path and publication policies unchanged. |
| `dial::learn::download` | Deprecated since 0.3.4, but still exported. Repository callers already use the canonical inference acquisition owner. | Replace imports with `dial::llm::download` (or `dial_llm::llm::download` when using the owning crate). Types and functions are re-exported unchanged. No removal release is scheduled. |

These decisions do not remove template syntax, model acquisition capabilities, persisted settings
or configuration aliases. They do not certify that every exported workspace API has been audited.
The focused contract tests live in `tests/integration/compatibility.rs`:

```sh
cargo test --locked -p dial --test integration compatibility::
```

## Deprecation procedure

1. Identify the canonical owner and a behaviorally equivalent replacement. Check internal callers,
   public re-exports, documentation, serialized contracts and dynamic registration separately.
2. Migrate repository callers and test both public paths where compatibility is retained.
3. Announce the replacement in the changelog and migration documentation. A Rust deprecation
   warning must name the replacement; lack of a replacement is a reason to retain support.
4. Remove an exported path only through a separately reviewed breaking-change decision. No
   repository-wide text search or dependency analyzer proves external consumers have migrated.
