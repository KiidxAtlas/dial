# How Dial compares

This page compares Dial with [oh-my-pi](https://github.com/can1357/oh-my-pi) (`omp`), another
open-source terminal coding agent with a similar breadth of tools. It is based on reading omp's
source and documentation at version 18.2.6 (docs from the 18.1.x tree). Newer omp releases may
differ, and corrections are welcome. It compares behaviour and defaults, not the quality of model
output.

## At a glance

| | Dial | omp |
| --- | --- | --- |
| Shell commands | Unconfined by default. With `sandbox.mode = "dev"` or `"strict"` they run inside the OS sandbox on macOS (`sandbox-exec`) and Linux/WSL2 (bubblewrap), and are refused if the sandbox is unavailable | Run with your user's full permissions; no OS sandbox |
| Default approval mode | `auto`: every tool call runs without a prompt; deny rules still apply. `write`, `ask` and `readonly` are available | `yolo`: every tool call, including shell commands, is auto-approved |
| Network access from commands | Optional per-domain allow/deny rules through a filtering proxy that refuses private and cloud-metadata addresses | Not filtered |
| Undo file changes | Original contents saved on disk before each change; `/undo`, `/rollback`, `/review-file`, even after a restart | `checkpoint`/`rewind` trims conversation context; file changes are not restored |
| Finishing a task | Completion is checked against observed work and test evidence | Guided by prompts and goal tools |
| Protecting uncommitted work | Git guard refuses `reset --hard`, `clean -f`, `stash` and similar commands from subagents (or everywhere, if you set it to) | No equivalent guard |
| Local models for the main agent | Dial-managed MLX servers, GGUF through llama.cpp, memory-fit estimates, Apple on-device model, plus running Ollama/LM Studio/llama.cpp servers | Ollama or any OpenAI-compatible server; built-in local inference is limited to small helper models (titles, memory) |
| Training | Optional LoRA adapters from your sessions, kept only if they pass held-out checks (Apple Silicon) | None; "autolearn" writes skill files |
| Resuming old sessions | Tool schemas are hashed; a session recorded under an incompatible schema is refused rather than replayed wrongly | No equivalent check |
| Provider batch APIs | Supported | Not supported |
| Implementation | One Rust binary | TypeScript on Bun with Rust native add-ons |

## Roughly equal

Both offer: LSP and debugger tools, AST search and edits, notebooks, browser automation and desktop
control, MCP, ACP and RPC, SSH, subagents, plan mode, encrypted collaboration, image generation,
voice, GitHub workflows, many hosted and subscription providers, steering while the agent works,
context compaction, and recovery of tool calls that a model wrote as plain text.

## Where omp is stronger

- **Maturity.** omp has a much longer release history and a larger user base.
- **Extensions.** omp has a TypeScript plugin and extension SDK distributed through npm, which is
  easier for most people to extend than Dial's hooks, skills and Rust SDK.
- **Fewer platform requirements.** omp needs no sandbox setup on any OS. Dial's optional sandbox
  requires bubblewrap on Linux and is unavailable on Windows.

## Sources in this repository

- Sandbox: `crates/dial-tools/src/tools/exec/sandbox.rs`, `sandbox/linux.rs`, `sandbox/netproxy.rs`
- Undo: `crates/dial-storage/src/storage/checkpoint.rs`
- Completion checks: `crates/dial-agent/src/agent/verify/`
- Git guard: `crates/dial-tools/src/tools/exec/git_guard.rs` ([shell](shell.md#git-guard))
- Training: `crates/dial-agent/src/learn/improve.rs` ([improvement](improvement.md))
- Tool contracts: `crates/dial-tools/src/tools/surface/contracts.rs`
