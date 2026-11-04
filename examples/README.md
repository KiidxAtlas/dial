# Examples and evaluation

Run commands from the repository root. These are source inputs, not generated release reports.

| Directory or example | Purpose |
| --- | --- |
| [workflows/](workflows/README.md) | Reusable workflow packs and their invocation examples. |
| [skills/](skills/) | Example agent skills, with instructions in each skill's `SKILL.md`. |
| [clients/rpc-v1.jsonl](clients/rpc-v1.jsonl) | Sample RPC client messages; see [RPC/ACP](../docs/rpc-and-acp.md). |
| [benchmarks/](benchmarks/README.md) | Evaluation suites, graders, scripted regression probes and committed fixtures—not just user demos. |
| [runtime_baseline.rs](runtime_baseline.rs) | Runtime overhead measurement: `cargo run --locked --example runtime_baseline -- target/runtime-baseline.json`. |
| [snapcompact_demo.rs](snapcompact_demo.rs) | Synthetic visual frames: `cargo run --locked --example snapcompact_demo -- scratch/snapcompact-preview`. No model or private sessions. |

Crate-owned examples stay beside their owners: the [UI gallery](../crates/dial-ui/examples/ux_gallery.rs)
uses `cargo run -p dial-ui --example ux_gallery`; the
[workspace-copy benchmark](../crates/dial-core/examples/workspace_copy_bench.rs) belongs to `dial-core`.

Keep generated reports/captures under `target/` or `scratch/`. Do not delete fixture files as
output: CI, integration tests and benchmark suites consume them at their current paths.
