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

## Before opening a pull request

```sh
cargo fmt --check
make lint
make test-rust          # or make test-nextest
```

Run the narrower tiers that match your change as well: `make test-drivers` for browser/eval/kernel
drivers, `make test-python` for Python code, `make test-bench-offline` for benchmark tooling.

## Pull requests

- One coherent change per PR. Describe the behaviour change and how you verified it, listing the
  commands you ran and anything you could not exercise (for example real providers, Windows or MLX).
- Add regression tests for behaviour and error paths, in the sibling `tests.rs` of the module.
- Update the relevant `docs/*.md` page and add a line under `## [Unreleased]` in `CHANGELOG.md`.
- Adding a dependency: declare it in the root `[workspace.dependencies]` and make sure
  `cargo deny check` still passes (licenses and advisories are configured in `deny.toml`).

## Releases

Maintainers cut releases with `scripts/release.sh <version>`; see
[docs/release-and-install.md](docs/release-and-install.md).

## Security issues

Do not file public issues for vulnerabilities; follow [`SECURITY.md`](SECURITY.md).
