# Quickstart

Five minutes from install to a reviewed, undoable fix. For the full loop afterwards, read
[Everyday Dial](everyday.md).

## 1. Install

```sh
curl -fsSL https://github.com/KiidxAtlas/dial/releases/latest/download/install.sh | bash
```

On Windows use PowerShell: `irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex`
(see [Windows](windows.md)). On Linux, install `bubblewrap` if you plan to turn the sandbox on (the
installer prints the command if it is missing); with the sandbox on and `bubblewrap` missing, shell
commands are refused rather than run unconfined. Homebrew, Nix and
Docker options are in [Release and installation](release-and-install.md).

## 2. Pick a model

Run `dial` in a project directory. First launch asks for **Hosted provider** or **Local model**:

- **Already running Ollama or LM Studio?** First launch names it and highlights **Local model**.
  Press Enter and pick a model; no API key is needed.
- **Apple Silicon with nothing running?** First launch suggests an MLX model that fits your memory.
  Choose **Local model**, then **Get models**; nothing downloads until you confirm.
- **Hosted:** choose **Hosted provider** to sign in with a Claude or ChatGPT/Codex subscription or
  add an API key.

Small local models (under about 7B) often produce broken tool calls; see
[Model downloads](model-downloads.md) and `dial models recommend` for what fits your machine.

## 3. Ask for a fix

Type a concrete request with its limits and press Enter:

```text
the test in tests/test_parser.py fails, fix it without changing the public API
```

Shell commands run unconfined by default. Set `sandbox.mode = "dev"` to confine them to the OS
sandbox on macOS and Linux/WSL2 ([shell](shell.md)). While Dial works you can type a correction and
choose whether it steers the current task or is queued; **Esc** interrupts.

## 4. Review the evidence

When the turn ends, the report lists the changed files and the checks that ran, with their results.
**Ctrl+O** expands the commands and output. A change reported without a passing check is marked
unverified, not done. `/review` asks for a read-only review of your uncommitted changes.

## 5. Keep or undo

`/undo` reverts the last turn's edits (`/undo 3` for three turns), even after a restart. Shell and
extension effects outside the edit checkpoints are named in the report, because undo cannot reverse
them. When you are happy, commit as usual.

## Next

- `dial -c` continues your last session; `dial -p "…"` runs one prompt for scripts.
- [Everyday Dial](everyday.md): the full ask, steer, review and resume loop.
- [Shell and sandbox](shell.md): what commands may do, network rules and the git guard.
- [How Dial compares](comparison.md): what Dial does differently from other terminal agents.
