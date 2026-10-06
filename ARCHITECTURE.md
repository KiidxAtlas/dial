# Architecture

Dial is a Rust control plane (agent loop, tools, UI, storage, learning loop) with a small Python
worker used only for MLX inference and LoRA training.

## Modules (`src/`)

| Module | Responsibility |
|---|---|
| `agent` | Agent loop, prompts, approvals, guards, subagents, todos, investigation/verification gates, harvesting of training data |
| `llm` | Provider clients (OpenAI-compatible, Anthropic, ChatGPT, local MLX server), streaming, tool-call repair (`hermes`, `json_repair`) |
| `tools` | Model-callable tools: files, hashline edits, search, shell + sandbox, LSP, AST, DAP, MCP, browser, documents, git, kernel |
| `storage` | Sessions, checkpoints, memory, project understanding, skills, settings (outside the workspace) |
| `config` | Configuration types and loading |
| `learn` | Gated self-improvement: curriculum, verification, probes, LoRA training jobs, admission/rollback |
| `ui` | Terminal interface: composer, transcript, overlays, themes |
| `app`, `repl`, `rpc`, `setup`, `main` | Entry points and wiring |
| `benchmark`, `benchmark_cli` | Benchmark definitions, isolation and reporting |
| `steering`, `adaptive`, `schedule`, `reliability` | Runtime steering, adaptive controls, scheduling and reliability helpers |

## Dependency boundaries

Enforced by `tests/architecture.rs` (production code only; `#[cfg(test)]` modules are exempt):

- `ui` does not depend on `learn`, `tools` or `config`.
- `llm` does not depend on `ui`, `learn` or `tools`.
- `tools` and `config` do not depend on `ui` or `learn`.
- `storage`, `agent` and `learn` do not depend on `ui`.

Changing a rule means editing that test and stating why here.

## Invariants

- Tool execution always passes through schemas, approvals, workspace confinement and the
  checkpoint observer (`tools/mod.rs`, `agent/approval.rs`, `tools/sandbox.rs`).
- A learned adapter is used only after it passes the evaluation gate; admission and rollback
  invalidate task routes (`learn/gate.rs`, `learn/profiles.rs`).
- Training yields compute to foreground coding at a step boundary (`learn/compute.rs`).
- Per-project state and secrets live outside the editable workspace (`storage/paths.rs`).

## Checks

```sh
cargo fmt --check
cargo build --locked --all-targets
cargo test --locked
```
