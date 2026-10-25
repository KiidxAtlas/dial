# Agent and contributor working notes

Guidance for coding agents (and humans) changing this repository. [`ARCHITECTURE.md`](ARCHITECTURE.md)
is the authority for crate layers, module ownership and invariants; read it before structural changes.

## Layout in one paragraph

One Cargo workspace. The root package `dial` holds the binary, a facade library re-exporting every
crate, and the integration tests (`tests/integration`). Crates under `crates/` are layered
`dial-build-info` → `dial-core` → `dial-llm` → `dial-storage` → `dial-tools` → `dial-computer` →
`dial-agent` → `dial-ui` → `dial-app`; a crate may depend only on lower layers.
`tests/integration/architecture.rs` enforces the layering and module rules.

## Rules that the build or tests enforce

- Unit tests never live inline in `src/`: write `#[cfg(test)] mod tests;` and put the body in the
  sibling `tests.rs` (`foo.rs` → `foo/tests.rs`).
- A module uses either `name.rs` + `name/` or `name/mod.rs`, never both.
- Configuration structs use `#[serde(default, deny_unknown_fields)]` with a `Default` impl.
  User-visible settings are declared once in `crates/dial-llm/src/config/schema/` and consumed by
  the UI, CLI and validation.
- Built-in tool parameter schemas are hashed into contract identities
  (`crates/dial-tools/src/tools/surface/contracts.rs`). An optional property whose absence keeps the
  old meaning must be listed in `ADDITIVE_PROPERTIES`, or sessions recorded under the old schema
  are refused on resume.
- Dependency versions are declared once in the root `[workspace.dependencies]`.

## Checks

```sh
cargo fmt --check
cargo build --locked --workspace --all-targets
make lint                      # clippy with the CI gate
make test-module MOD=llm::retry  # fast loop for one module (searches every crate)
make test-integration          # integration test binary
make test-rust                 # whole workspace
make test-nextest              # same suite under cargo-nextest, if installed (.config/nextest.toml)
```

Report the commands you actually ran. Tests that need a model, a display, a device or network are
optional tiers (`make test-mlx`, `make test-smoke`); never present a skipped tier as passing.

## Working conventions

- Reuse existing owners and helpers; do not add a parallel pipeline for an existing concern.
- Keep changes focused. Preserve unrelated uncommitted work in the checkout.
- Update the matching `docs/*.md` page for user-visible behaviour, and add an entry under
  `## [Unreleased]` in `CHANGELOG.md`.
- Never weaken a valid check, suppress a warning, or leave stubs to get a green run.
- Secrets and per-project private state live outside the workspace (`crates/dial-llm/src/config/paths.rs`);
  never commit them or print them in logs or test output.
