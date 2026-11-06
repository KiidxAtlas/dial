# Benchmark tooling and fixtures

The usage and measurement contract lives in [docs/benchmarks.md](../../docs/benchmarks.md).
This index explains ownership; existing paths remain stable for suites, CI and integration tests.

## Pick a check tier

| Tier | Entry point | What it establishes |
| --- | --- | --- |
| Offline evaluator contracts | `make test-bench-offline` | Graders, fixture acceptance, report analysis and runner contracts; no model or built Dial required. |
| Scripted real-binary recovery | `make test-smoke DIAL=target/debug/dial` | Runs the selected existing artifact against a scripted local provider and real tools; does not measure model quality. |
| Real terminal lifecycle | `interactive_smoke.py`, `interactive_surfaces.py`, `session_contention_smoke.py`, `side_lifecycle_smoke.py` | CI/preflight probes using an existing binary and terminal prerequisites such as tmux. Captures are text, not screenshot verification. |
| Live-model evaluation | `dial benchmark <suite.toml> --profiles <profiles.toml> --report benchmarks/report.json` | Independently graded cases with supplied model/provider configuration; can consume quota. |

## Directory map

- **Suites** (`efficiency.toml`, `quality.toml`, `reliability.toml`, `representative.toml`,
  `tool-effectiveness.toml`, `interpretation.toml`, `local-tasks.toml`, `recovery.toml`,
  `capabilities.toml`, `compaction.toml`) declare cases and acceptance.
- **Profiles** (`profiles.toml`, `local-measurement.toml`, `*-profiles.toml`) select agents,
  models and limits. They are not suites. Browser/MCP integration suites require explicitly
  provisioned integrations; ordinary offline checks do not install them.
- **Graders and analysis** (`test_*.py`, `compare.py`, `reliability.py`, `tool_effectiveness.py`)
  check evaluation contracts or analyze existing reports. Analysis is not a fresh measurement.
- **Cross-agent evaluation** uses `cross_agent.py` and `cross-agent.toml` with installed agent CLIs.
  [Registry discovery](registry-discovery.md) additionally checks ordered trace behavior;
  answer-only `dial benchmark` runs do not establish that behavior.
- **Specialized probes:** `offline_compare.py` uses scripted loopback responses and synthetic
  token counts; `swe_bench.py` is an optional Docker/SWE-bench workflow with downloads and model
  calls; `retrieval_measurements.py` runs Rust retrieval measurements without a model.
- **Committed fixture inputs** live in `*-fixture/`, `project/`, `representative-rust/`,
  `representative-ts/` and `edit-grammar-corpus/`. The latter feeds `dial edit-bench`.
  Synthetic documents such as the interpretation XLSX are deliberate test inputs, not clutter.
  `generate_interpretation.py` regenerates committed synthetic interpretation inputs.

## Reports and comparisons

Write reports, downloaded datasets, benchmark environments and terminal captures under
`benchmarks/`, not Cargo's disposable `target/` or beside fixture inputs. This local workspace
is ignored by Git and Docker (except its README); traces and derived metadata may be private.
Compare matching suite/fixture revisions, model configuration and repetitions. Check correctness
before latency, tokens or cost; retain failed and invalid runs. Unknown usage is not zero usage.
See the owning measurement guide for exact invocation, dependencies and interpretation limits.
