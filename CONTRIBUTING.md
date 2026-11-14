# Contributing to Dial

Thanks for helping. This page covers the workflow; [`ARCHITECTURE.md`](ARCHITECTURE.md) covers the
design and [`AGENTS.md`](AGENTS.md) lists the rules the build enforces.

## Setup

- Rust: the toolchain is pinned in `rust-toolchain.toml` (rustup picks it up automatically).
- Python 3.12 matches CI for Python contracts and benchmark graders (`make test-python`,
  `make test-bench-offline`). Use the pinned portable test environment without changing system Python:
  `uv run --no-project --python 3.12 --with-requirements scripts/requirements-ci.lock make test-python`.
  Worker package versions are declared in `python/pyproject.toml`; MLX checks remain optional
  and require a prepared Apple-Silicon runtime. A portable suite pass does not establish MLX behavior.
- Optional: [`cargo-nextest`](https://nexte.st) for `make test-nextest`, and `cargo-deny` for
  `cargo deny check` (CI runs it).
- [`cargo-machete`](https://github.com/bnjbvr/cargo-machete) for `make lint-deps`
  (`cargo install cargo-machete --locked --version 0.9.2`). CI uses a pinned, checksum-verified
  binary; local preflight reports a skip when the tool is absent and never installs it.

`make build` builds the binary; `make update` installs your checkout's build as the active `dial`.

To add a built-in tool, implement `tools::ext::Tool` in its own module, return it from a `tools()`
constructor, and list it in `Registry::standard`. Crate layout and module ownership are in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Before opening a pull request

```sh
cargo fmt --check
make lint
make test-rust          # or make test-nextest
make lint-deps         # workspace dependency audit; requires cargo-machete
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

- One coherent change per PR and per commit. Describe the behaviour change and how you verified it,
  listing the commands you ran and anything you could not exercise (for example real providers,
  Windows or MLX).
- Keep commits reviewable. Aim for a few hundred changed lines; split anything larger along
  ownership lines (one crate or module, then its callers, then docs). Mechanical changes (moves,
  renames, formatting, generated files, lockfiles) go in their own commit with no behaviour change,
  and the message says so. A commit that mixes a move with an edit hides the edit.
- A commit that changes an agent-loop default (prompt wording, a threshold, a tool's default
  behaviour, an `[experimental]` default) names its evidence in the message: the benchmark report
  it rests on, or "unmeasured". See the next section.
- Add regression tests for behaviour and error paths, in the sibling `tests.rs` of the module.
- Update the relevant `docs/*.md` page and add a line under `## [Unreleased]` in `CHANGELOG.md`.
- Adding a dependency: declare it in the root `[workspace.dependencies]` and make sure
  `cargo deny check` still passes (licenses and advisories are configured in `deny.toml`).
- Public API cleanup follows the [compatibility review](docs/compatibility.md): migrate callers
  and announce a replacement before deprecating; do not delete exported APIs based on text searches.

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

### New integrations and tools are paused

Until the headline task score (`make bench-headline`, [docs/benchmarks.md](docs/benchmarks.md#headline-score))
has a recorded baseline, new opt-in integrations and new built-in tools are not accepted. Fixes,
reliability work and removals for existing ones are welcome. A proposal to lift the pause for one
addition needs a before/after headline comparison on the same model and profile showing it helps,
and must keep the hosted core tool offer inside its schema budget (25 KiB) without removing `task`.

## Releases

Maintainers prepare release metadata with `scripts/release.sh <version>`; tagging and publication
are separate explicit steps. See [docs/release-and-install.md](docs/release-and-install.md).

## Security issues

Do not file public issues for vulnerabilities; follow [`SECURITY.md`](SECURITY.md).
