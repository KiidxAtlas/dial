<div align="center">

# dial

**A coding agent that runs every command in a sandbox, can undo every edit, and shows evidence
before it says "done". It works with your own models or anyone else's.**

[![CI](https://github.com/KiidxAtlas/dial/actions/workflows/ci.yml/badge.svg)](https://github.com/KiidxAtlas/dial/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/KiidxAtlas/dial?sort=semver)](https://github.com/KiidxAtlas/dial/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<img src="docs/images/home.png" alt="Dial's home screen: the dial wordmark, a centered composer asking what are we building, and the active model, project and branch" width="640">

</div>

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
cd your-project && dial
```

On first launch Dial asks which model to use. Then you tell it what you want.

## What makes it different

- **Sandboxed by default.** On macOS and Linux/WSL2, shell commands run inside the OS sandbox
  (`sandbox-exec` or bubblewrap), with optional per-domain network rules. If the sandbox is
  missing, commands are refused rather than run unprotected. (Windows has no supported sandbox, so
  commands there need an explicit opt-out.) A git guard stops subagents from running
  `reset --hard`, `clean -f` and similar commands that would throw away your uncommitted work.
- **Every edit can be undone.** Dial saves a file's original contents before changing it.
  `/undo`, `/rollback` and `/review-file` keep or revert changes, even after a restart.
- **Evidence, not "done".** After making changes, Dial runs the relevant tests and shows you the
  commands and results before it reports the work as finished.
- **Any model, including fully local.** MLX on Apple Silicon, GGUF through llama.cpp, LM Studio,
  Ollama, any OpenAI-compatible endpoint, a hosted API key, or your Claude or ChatGPT/Codex
  subscription. If a local server is already running, first launch finds it and needs no API key.
- **Teach your local model.** On Apple Silicon, Dial can optionally train LoRA adapters from your
  own sessions. A candidate is kept only if it passes held-out checks
  ([improvement](docs/improvement.md)).
- **Interruptible.** You can send a message while it works and choose whether to steer the current
  task or queue the message for later.

<table>
<tr>
<td width="50%"><img src="docs/images/work-evidence.png" alt="A turn showing the user's request, a cargo test run with 18 passed, and the agent's verified answer"><br><sub>Test results in the transcript</sub></td>
<td width="50%"><img src="docs/images/steering.png" alt="While Dial is working, a picker asks whether a new message should steer the task or be queued for after the turn"><br><sub>Steer or queue while it works</sub></td>
</tr>
</table>

## `Install`

| Platform                              | Command                                                                                     |
| ------------------------------------- | ------------------------------------------------------------------------------------------- |
| macOS 13+, Linux (x86-64/arm64), WSL2 | `curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh \| bash` |
| Windows (PowerShell)                  | `irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 \| iex`        |

You don't need Rust. The installer checks release signatures. Homebrew, Nix, Docker and source
builds are also supported. To update, run `dial update`. Linux needs `bubblewrap` for the sandbox.
For all options and requirements, see [docs/release-and-install.md](docs/release-and-install.md).

## Use it

```sh
dial                                  # open the terminal UI
dial "fix the failing test"           # start with a task
dial -p -y "summarize this repo"      # one-shot, prints the answer (for scripts)
dial -c                               # continue your last session
```

Inside Dial:

| Key or command          | What it does                 |
| ----------------------- | ---------------------------- |
| `/providers`, `/models` | Sign in and pick a model     |
| `/settings`             | Configure everything         |
| `Ctrl-P`                | Search every command         |
| `/review-file`          | Review, keep or undo changes |
| `/help`                 | Commands and shortcuts       |

The approval mode sets how much Dial does without asking: `ask`, `write` (the default), `auto` or
`readonly`. See [configuration](docs/configuration.md).

## Learn more

- [Quickstart](docs/quickstart.md): install to a reviewed, undoable fix in five minutes
- [Everyday Dial](docs/everyday.md): start, steer, review, undo and resume
- [Configuration](docs/configuration.md) and [choosing a model](docs/provider-catalog.md)
- [Tools](docs/tools.md), [subagents](docs/subagents.md), and [MCP, hooks and skills](docs/extensibility.md)
- [Editor integration (RPC/ACP)](docs/rpc-and-acp.md)
- [How Dial compares](docs/comparison.md)
- [All documentation](docs/README.md)

Small local models (under about 7B parameters) often produce broken tool calls. If results are
poor, try a larger model.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and [ARCHITECTURE.md](ARCHITECTURE.md).

MIT licensed; see [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).
