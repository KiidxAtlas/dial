## What and why

<!-- The behaviour change and the motivation. Link issues. -->

## Verification

<!-- Commands you ran and their results. Name anything you could not exercise
     (real providers, Windows, MLX, a display). -->

- [ ] `cargo fmt --check`
- [ ] `make lint`
- [ ] `make test-rust` (or `make test-nextest`)
- [ ] Narrower tiers that apply (`make test-python`, `make test-drivers`, `make test-bench-offline`)

## Checklist

- [ ] Regression tests in the module's sibling `tests.rs`
- [ ] Docs updated (`docs/*.md`) for user-visible behaviour
- [ ] `CHANGELOG.md` entry under `## [Unreleased]`
- [ ] New built-in tool properties registered in `ADDITIVE_PROPERTIES` when they are optional and additive
