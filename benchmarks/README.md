# Local benchmark workspaces

Benchmark artifacts belong here, outside Cargo's disposable `target/` directory.
Committed suites, tooling and fixtures remain in [`examples/benchmarks/`](../examples/benchmarks/README.md).
See [`docs/benchmarks.md`](../docs/benchmarks.md) for measurement contracts and commands.

## Workspace map

- `harbor-current/`: local Harbor runners, frozen agent binaries and source manifests,
  historical and three-way reports, private trial logs/journals, replay fixes and the
  benchmark-specific Cargo cache.
- `harbor-tb2/`: downloaded Terminal-Bench task checkout, including its Git metadata.
- `harbor-env/`: local Harbor Python environment; entrypoints and activation paths are
  updated for this checkout's location.
- Other local benchmark reports, environments, datasets and captures also belong here.

These three trees were moved from `target/` without rebuilding binaries, replacing
trials or changing task/verifier inputs. `relocation-inventory.json` records the move's
per-entry identity check before subsequent entrypoint/documentation updates.
Historical logs, reports, dependency metadata and source manifests retain their original
bytes and may contain old absolute paths. Resolve recorded `target/harbor-*` locations
under `benchmarks/harbor-*`; do not rewrite historical evidence or recreate `target/` aliases.
The benchmark-specific Cargo cache may rebuild when reused because source paths changed.

## Offline checks

From the repository root:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/harbor-current/test_threeway.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s examples/benchmarks -p 'test_*.py'
benchmarks/harbor-env/bin/harbor --help
```

`harbor-current/run.py` still enforces frozen source/binary identities and refuses
existing trial names. To benchmark the current checkout without a manual freeze, use a
`build_checkout` manifest: the runner builds native Linux ARM64 Dial from the checkout in
Docker (read-only mount, cached `dial-linux-*` volumes, offline), records the binary hash and
every source hash, refuses to continue if any source changed during the build, and writes the
frozen manifest as `harbor-current/<batch>-manifest.json`:

```sh
python3 benchmarks/harbor-current/run.py --manifest benchmarks/harbor-current/checkout-ab-manifest.json --batch checkout-ab-1
python3 benchmarks/harbor-current/run.py --manifest benchmarks/harbor-current/checkout-ba-manifest.json --batch checkout-ba-1
```

These make paid model requests; `--dry-run` plans without building or running trials.

Everything here except this README is ignored by Git, and the whole directory is
excluded from Docker build contexts. Raw logs and derived metadata may contain private
information; moving them here does not make them safe to publish. The local environment
is tied to this host and is not a portable installation. Cargo's ordinary build output
and installed development binaries remain in `target/`.
