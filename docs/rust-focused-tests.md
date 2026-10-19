# Rust focused-test selection

With assistance and selective verification enabled, automatic verification maps changed Rust paths to focused `cargo test` commands. The mapping lives in
`crates/dial-agent/src/agent/verify/verification.rs` and reads only `Cargo.toml` (via `toml`) and the
changed sources. It never runs `cargo metadata`.

## Mapping

- The owning package is the nearest `Cargo.toml` at or above the file. It must parse and contain a
  `[package]` table; virtual workspace manifests own nothing.
- Targets come from explicit `[lib]`, `[[bin]]`, `[[test]]`, `[[example]]` tables plus Cargo's
  auto-discovery (`src/lib.rs`, `src/main.rs`, `src/bin/*`, `tests/*.rs`, `tests/*/main.rs`,
  `examples/*`), honouring `autobins`/`autotests`/`autoexamples = false`.
- Test filter per file:
  - Crate roots and file-form test targets run the whole target; a crate root with `mod tests`
    runs `tests::`.
  - Files under `tests/<name>/` run `<module>::` against that integration target. Helpers without
    `#[test]` run the whole target.
  - `src` modules use `<module>::tests::` when they declare `mod tests` (inline or `mod tests;`),
    and `<module>::` when they hold `#[test]` items. A `tests` segment narrows to that subtree.
- Command: `cargo test --offline --manifest-path <Cargo.toml> (--lib | --bin N | --test N | --example N) [filter] --quiet -j1`.
  The package directory is quoted; the name is the Cargo target name.

## Explicit gaps

`RelatedTestPlan::limits` (from `related_test_plan_with_runner`) names root-aware gaps;
`coverage_limits` reports only path-level gaps. The agent publishes the plan's limits alongside
verification results. Gaps are never a pass:

- no owning package, virtual or unreadable manifest, or non-package manifest;
- deleted or renamed-away paths;
- files outside any registered target's source tree (shared `tests/common/mod.rs`, unregistered
  `src/bin` files, `[[test]]` autodiscovery disabled);
- crate roots or modules without their own tests (compile check only);
- non-identifier module names, absolute or parent-relative paths, unreadable or oversized sources;
- path-only: `build.rs`, `benches/`, and `Cargo.toml` changes.

Selection is bounded and syntactic, not a full Rust module resolver. `#[path]`, conditional modules,
generated tests, and ambiguous lib/bin ownership can produce incomplete or zero-test filters.
Zero-test execution is classified as unverified. Run the broader package/workspace suite to close
these coverage limits; focused green checks do not establish full feature correctness.
