# Contributing to Dial

Thanks for helping. This page covers the workflow; [`ARCHITECTURE.md`](ARCHITECTURE.md) covers the
design and [`AGENTS.md`](AGENTS.md) lists the rules the build enforces.

## Setup

- Rust: the toolchain is pinned in `rust-toolchain.toml` (rustup picks it up automatically).
- Python 3.12 for the Python contracts and benchmark graders (`make test-python`,
  `make test-bench-offline`). MLX checks are optional and Apple-Silicon only.
- Optional: [`cargo-nextest`](https://nexte.st) for `make test-nextest`, and `cargo-deny` for
  `cargo deny check` (CI runs it).

`make build` builds the binary; `make update` installs your checkout's build as the active `dial`.

To add a built-in tool, implement `tools::ext::Tool` in its own module, return it from a `tools()`
constructor, and list it in `Registry::standard`. Crate layout and module ownership are in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Before opening a pull request

```sh
cargo fmt --check
make lint
make test-rust          # or make test-nextest
```

Run the narrower tiers that match your change as well: `make test-drivers` for browser/eval/kernel
drivers, `make test-python` for Python code, `make test-bench-offline` for benchmark tooling.

`make help` is the command index. `make test-packaging test-release-policy` checks distribution
metadata and release policy; `make test-build` checks local installation without replacing your installed binary;
`make test-install` exercises the Unix release installer. See [examples/README.md](examples/README.md)
for runnable samples versus benchmark infrastructure.

Start with `make release-check` for no-build source, packaging and tooling readiness. It does
not replace compilation or runtime tests. Before pushing a committed tree, use `make preflight`.
For staged or uncommitted release work, use `make preflight-worktree`: it checks the current files
without committing or resetting them. Both preflight commands compile; neither replaces
native-platform CI or hosted signing verification.

## Pull requests

- One coherent change per PR. Describe the behaviour change and how you verified it, listing the
  commands you ran and anything you could not exercise (for example real providers, Windows or MLX).
- Add regression tests for behaviour and error paths, in the sibling `tests.rs` of the module.
- Update the relevant `docs/*.md` page and add a line under `## [Unreleased]` in `CHANGELOG.md`.
- Adding a dependency: declare it in the root `[workspace.dependencies]` and make sure
  `cargo deny check` still passes (licenses and advisories are configured in `deny.toml`).

### Prompt wording, thresholds and recovery limits

Changes to model-facing wording (system prompt sections, harness notes, recovery and correction
text) or to tuning constants (recovery limits, stall and loop thresholds, research windows,
escalation and budget defaults) change model outcomes that unit tests cannot observe. Such a PR
must state its evidence in the description:

- **Outcome evidence:** a before/after `dial benchmark` comparison on the same suite, fixture
  revision and model (see [docs/benchmarks.md](docs/benchmarks.md)), with the repetitions and
  the suite named; or
- **No outcome change claimed:** the change is a correctness or safety fix, or a refactor that
  keeps the model-visible text and decisions, and the PR says so.

Without one of these, keep the current value. A new tuning knob ships disabled or at the current
behaviour until a measurement justifies a different default. Passing assembly or routing tests
does not establish a live-model improvement. The `[Unreleased]` CHANGELOG entry names the
measurement or says the effect is unmeasured.

## Releases

Maintainers prepare release metadata with `scripts/release.sh <version>`; tagging and publication
are separate explicit steps. See [docs/release-and-install.md](docs/release-and-install.md).

## Security issues

Do not file public issues for vulnerabilities; follow [`SECURITY.md`](SECURITY.md).
