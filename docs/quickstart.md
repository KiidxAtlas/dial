# Quickstart

Five minutes from install to a reviewed, undoable fix. For the full loop afterwards, read
[Everyday Dial](everyday.md).

## 1. Install

```sh
curl -fsSL https://dial.rootatlas.dev/install | sh
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

Want to understand the code before changing it? Start with `dial --approval readonly` and ask
“Where does this behavior come from? Cite the files and lines.” This uses Dial's enforced read-only
approval mode, rather than relying on a prompt alone.

## 3. Ask for a fix

Type a concrete request with its limits and press Enter:

```text
the test in tests/test_parser.py fails, fix it without changing the public API
```

Shell commands run unconfined by default. Set `sandbox.mode = "dev"` to confine them to the OS
sandbox on macOS and Linux/WSL2 ([shell](shell.md)). While Dial works you can type a correction and
choose whether it steers the current task or is queued. **Esc** dismisses a chooser. To interrupt
the turn, press **Ctrl+C** at the empty composer; with a draft or panel open, the first press
clears the draft or closes the panel instead.

## 4. Review the evidence

When the turn ends, the report lists the changed files and the checks that ran, with their results.
**Ctrl+O** expands the commands and output. A change reported without a passing check is marked
unverified, not done. Use `/review uncommitted` for a read-only AI review, or `/review files` to
inspect checkpointed changes yourself. Opening file review does not revert anything.

## 5. Keep or undo

Keep the changes by leaving them in place; commit when ready. To restore files from the latest
checkpoint **without changing the conversation**, use `/undo files`. To restore captured edits
**and rewind the conversation**, use `/undo` (`/undo 3` for three turns). These are restore
commands, not previews. Neither reverses external effects such as a push or a running process.

## Next

- `dial -c` continues your last session; `dial -p "…"` runs one prompt for scripts.
- [Everyday Dial](everyday.md): the full ask, steer, review and resume loop.
- [Shell and sandbox](shell.md): what commands may do, network rules and the git guard.
