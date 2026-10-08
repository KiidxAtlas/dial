# dial

A local-first coding agent harness for macOS, native Windows, and Linux. Run it in a project directory like any agent CLI. Self-improvement is automatic: observations feed verified practice, harness proposals and local LoRA training; only an external evaluation gate can activate a candidate. Local MLX inference and training require Apple Silicon; Intel Macs, Windows, and Linux use hosted or OpenAI-compatible providers.

Standalone: no dependency on omp, Hermes, or any hosted service. Rust control plane and tools; Python for local MLX inference/training (`mlx-lm`, plus `mlx-vlm` for vision).

## Install

Release targets are **macOS 13+ on Apple Silicon or Intel**, **native Windows x64**,
and **Linux x86-64 (Ubuntu 22.04+)**. Download a prebuilt executable; no Rust
compiler or source checkout is required. Windows runs directly in PowerShell,
Command Prompt or Windows Terminal. Command tools use Git Bash from Git for Windows,
like OMP; you do not need WSL for native use.

This repository remains private. Download access requires a GitHub account that
has been granted repository access. You can download the matching archive and
checksum from the release page in your signed-in browser, then follow the manual
instructions below.

For the terminal installer, install [GitHub CLI](https://cli.github.com/), then sign
in with an account that has access and download the release's installer:

```sh
gh auth login --hostname github.com
gh release download --repo KiidxAtlas/dial --pattern install.sh
bash install.sh
export PATH="$HOME/.local/bin:$PATH"
dial
```

The installer uses authenticated GitHub CLI downloads for private releases;
credentials stay managed by GitHub CLI. The curl path remains available for public
releases. Complete the platform prerequisites below before installing.

The installer selects the correct archive and verifies its SHA-256 before installing.
Run `bash install.sh v0.1.2` to select a specific release. The commands above are
for macOS/Linux; use the PowerShell installer below on native Windows.

### macOS (Apple Silicon or Intel)

In the download directory (replace `v0.1.2` with the release you downloaded):

```sh
version=v0.1.2
# Apple Silicon: aarch64-apple-darwin; Intel: x86_64-apple-darwin
target=aarch64-apple-darwin
shasum -a 256 -c "dial-$version-$target.tar.gz.sha256"
tar -xzf "dial-$version-$target.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 755 "dial-$version-$target/dial" "$HOME/.local/bin/dial"
export PATH="$HOME/.local/bin:$PATH"
dial --version
```

Stop if checksum verification fails. Add `export PATH="$HOME/.local/bin:$PATH"`
to your shell configuration (usually `~/.zshrc`) to keep Dial on your PATH.
To update, repeat with a newer release and restart any running Dial sessions.
The archives are not signed or notarized; if macOS blocks execution, review your
system's security prompt rather than disabling Gatekeeper globally.

Hosted providers need an account/API key configured through first-run setup, but
do not need Python. Local MLX inference is **macOS/Apple Silicon only** and
additionally requires `uv`, network access for runtime/model downloads, and sufficient memory for the selected model. The
Python worker is embedded in the binary; no repository checkout is needed.

### Native Windows

Install Git for Windows and GitHub CLI, then open a new PowerShell terminal:

```powershell
winget install --id Git.Git -e
winget install --id GitHub.cli -e
gh auth login --hostname github.com
gh release download --repo KiidxAtlas/dial --pattern install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
# Open a new terminal after installation:
dial --version
cd C:\path\to\your\project
dial --sandbox off
```

The installer downloads the native `.exe`, verifies its SHA-256 and version, and
installs it under `%LOCALAPPDATA%\Dial\bin`, adding that directory to your user
PATH. An update that fails verification preserves the existing executable.
Use `-Version v0.1.2` for a specific release. Access to this private repository is
required. You can also download the Windows ZIP and its `.zip.sha256` in your
signed-in browser, verify it with `Get-FileHash`, extract it, and run `dial.exe`.

Complete first-run provider setup. Hosted providers need no Python; local Windows
inference can use an OpenAI-compatible server such as LM Studio or Ollama.
Apple MLX inference/training remains Apple Silicon only. Python 3 is optional for
the Python kernel; configure `kernel.python` if `python` is not on PATH.

Native Windows command tools require an **explicit** `--sandbox off` (or
`[sandbox] mode = "off"` in your own config). They run with your normal user permissions
and retain tool approval prompts. Windows Job Objects supervise command descendants
for cancellation, timeout and normal shutdown; they do not restrict filesystem or
network access. Dev/strict OS sandbox modes remain unavailable on native Windows.
Use the WSL2 instructions below when Linux filesystem/network confinement is required.
Set `DIAL_BASH` to an operator-installed `bash.exe` if Git Bash cannot be discovered.
Dial keeps state in `%USERPROFILE%\.dial` unless `DIAL_HOME` is set, and protects
private state files with the current user's Windows ACL.

### Windows through WSL2 / Linux x86-64

On Windows, install **WSL2 with Ubuntu 22.04 or newer**, using PowerShell as
administrator if it is not already installed:

```powershell
wsl --install -d Ubuntu-22.04
```

Restart if prompted and create your normal Ubuntu user. For confinement, disable
Windows executable interop in this Ubuntu distribution: edit `/etc/wsl.conf` with
`sudoedit`, preserving existing settings, and add or update:

```ini
[interop]
enabled=false
appendWindowsPath=false
```

Then run `wsl --shutdown` from PowerShell and reopen Ubuntu. This affects that
distribution's ability to launch Windows executables, not Windows itself. Dial
refuses sandboxed commands while Windows interop remains enabled, rather than
letting a Windows process escape Linux confinement.

In Ubuntu (or native Linux), install the sandbox and certificates:

```sh
sudo apt-get update
sudo apt-get install -y bubblewrap ca-certificates
```

Download the Linux archive and checksum into a Linux directory, then:

```sh
version=v0.1.2
sha256sum -c "dial-$version-x86_64-unknown-linux-gnu.tar.gz.sha256"
tar -xzf "dial-$version-x86_64-unknown-linux-gnu.tar.gz"
mkdir -p "$HOME/.local/bin"
install -m 755 "dial-$version-x86_64-unknown-linux-gnu/dial" "$HOME/.local/bin/dial"
export PATH="$HOME/.local/bin:$PATH"
dial --version
```

Stop if checksum verification fails. Add the PATH export to `~/.bashrc`. Keep your
checkout and Dial's home/state on the Linux filesystem (for example `~/code`),
not the Windows-mounted drive, and run Dial as your normal user, never with sudo.
Choose a hosted provider in first-run setup or `/providers`; local MLX is unavailable on Intel Macs and Linux/WSL2.
An OpenAI-compatible server running elsewhere can also be configured as an endpoint.

Linux/WSL2 commands require working unprivileged user namespaces. Dial runs a real
filesystem and network enforcement probe; missing or blocked bubblewrap disables
command execution with an error, never an unsandboxed fallback. Linux Dev mode is
deliberately stricter than macOS Dev mode: only the system runtime, workspace and
explicit `sandbox.extra_write` directories are visible. Host home directories,
IPC sockets and Windows drives are not exposed wholesale. Secret/denied paths
overlapping any exposed tree are refused; keep secrets outside allowed trees.
Toolchains must be installed in system paths or inside the project/explicit allowed
directories. Do not allow your entire home directory just to expose a toolchain.

### Build from source

With Rust and a C toolchain installed (Xcode Command Line Tools on macOS;
`build-essential` on Ubuntu), clone this repository and run:

```sh
cargo install --path . --locked
cd ~/code/my-project
dial                                  # native terminal UI; first run opens model/provider setup
dial "fix the failing test in src/lib.rs"   # interactive, opening with a message
dial -p -y "summarize this repo"      # one-shot; prints the answer (use -y or --approval auto for tools that change things)
dial -c                               # continue the latest session   (-r <id-prefix> to pick one)
```

To rebuild after changing this checkout, run `make update` here (or `make -C /path/to/dial update`).
This runs `cargo build --release --locked` and copies the binary to `bin/dial` in this checkout
(git-ignored); run `./bin/dial` or add that directory to your `PATH`. `cargo install --path .`
installs into Cargo's bin directory instead. `command -v dial` shows which copy your terminal
will run, and `make help` lists the other build and check targets. Restart any running dial
session afterward. See `ARCHITECTURE.md` for the module layout.

For local MLX you can also prepare the runtime and model explicitly:
`dial setup -m mlx-community/Qwen2.5-7B-Instruct-4bit` (requires `uv`).
Discovery itself does not download weights. Selecting an uncached local model and submitting a
prompt resolves its weights; a missing default MLX runtime gets an installation confirmation.

### Publishing a release (maintainers)

The release workflow runs on pushed `v*` tags and requires the tag to match
`Cargo.toml`'s version. After updating the version and lockfile as needed, commit
the release changes and push that commit and its tag, for example:

```sh
git tag v0.1.2
git push origin main
git push origin v0.1.2
```

`.github/workflows/release.yml` checks formatting, runs the Rust tests (including
required Linux sandbox probes), builds Apple Silicon/Intel Mac and Linux x86-64 binaries,
and smoke-tests each extracted archive outside the checkout. Only after **all three**
platform builds pass does it create a GitHub release with the archives, bundled MIT
license and README, per-archive SHA-256 checksums, a combined `SHA256SUMS`, and
generated notes. CI runs on both macOS and Ubuntu without weakening host security.
The workflow preserves repository visibility. Private releases are available only
to accounts with repository access; publishing a release does not grant that access. Dial is MIT licensed; see `LICENSE` for
permission to use, modify, and redistribute it.

### Anthropic subscription sign-in

Use Dial's browser OAuth flow for a Claude subscription: open `/providers`, select **Anthropic**,
then choose **Sign in with Claude subscription** and complete authorization in your browser.
Dial stores access and refresh tokens in its private user directory, refreshes access before
requests, fingerprints harness requests with Claude Code's billing-header and body attestation,
and sends subscription requests only to Anthropic's official API endpoint. An Anthropic API key
remains supported; sign out of the subscription in `/providers` to use the API-key route.
Providers with zero discovered models are hidden in `/models`, but remain in `/providers`
so they can be configured.

## Project architecture and reuse

Dial's project memory indexes source outlines and source-backed findings for reuse across
sessions and subagents. It also prioritizes Markdown design documents named `ARCHITECTURE.md`,
`DESIGN.md`, `CONTRIBUTING.md`, or `PRINCIPLES.md` (case-insensitive names), and Markdown files
under `architecture`, `adr`, or `decisions` directories. This uses the existing bounded,
ignore-aware index; ignored or unindexed documents are not guaranteed to appear.

Keep project goals, module responsibilities, dependency boundaries, canonical helpers and
architectural check commands in these documents. Record important decisions and their rationale
in ADRs. Outlines include section headings so the agent can read the relevant current sections;
they are not a substitute for reading the source.

Up to three design records receive retrieval priority, using at most a third of the context
budget for that priority lane. Source hashes are checked before reuse; changed sources and
their dependent findings must be refreshed. Agents are guided to identify the owner, search
for reusable implementations, justify new abstractions and review the diff for architectural
fit. These are evidence and workflow aids, not a guarantee of compliance. Enforce important
boundaries with executable checks configured through `[verification].commands`.

## Discover, import and toggle skills

Browse compatible skills from Claude, Codex, Agents (`.agents`/`.agent`), omp and pi
in this project and your home directory with `dial skills sources` or `/skills sources`.
Browsing does not activate them. Import portable `<name>/SKILL.md` folders explicitly
from those locations or any locally checked-out repository; no downloading or script execution.

```sh
dial skills sources
dial skills import ~/.claude/skills --dry-run
dial skills import ~/.claude/skills --user
dial skills import ~/.agents/skills --user
dial skills import ./vendor/skills/code-review
dial skills list
dial skills disable code-review          # this project
dial skills enable code-review --user    # user default; project overrides still win
dial skills import ~/.claude/skills --user --update --dry-run
dial skills import ~/.claude/skills --user --update
```

Sources can be one skill or a directory of immediate skill folders. Default
destination is `.dial/skills`; `--user` uses `$DIAL_HOME/skills` (normally
`~/.dial/skills`). Project skills shadow user skills. `/skills` lists installed skills,
their on/off state and location. Chat supports `/skills import <path> [--user] [--dry-run]
[--update]`, `/skills enable <name> [--user]` and `/skills disable <name> [--user]`.
Quote paths containing spaces. Invoke enabled skills with `/skill:<name>`.

Toggles persist by skill name in `skills.json` outside the workspace: project state by
default, or Dial's user directory with `--user`. A project setting overrides the user
default, including for same-name shadowed skills. Disabling never deletes a skill; it
removes it from completion, the model catalog and skill-tool loading. Imports and toggles
refresh chat completion immediately and the model catalog on the next turn. Commands
submitted while a turn runs are queued until it finishes. Previously loaded skill text
remains in conversation history; disabling is not a sandbox or a way to erase that text.

Folders need lowercase/digit/hyphen names and frontmatter with a nonempty
`description`; any declared `name` must match. Invalid entries and conflicts
are reported as skipped. `.dial-import.json` records source paths and hashes.
`--update` only replaces imports from the same source with no local content
changes. Updates are explicit; back up local edits before resolving conflicts.

Symlinks and special files are refused. Limits: 64 KiB per file, 200 files,
8 MiB per skill, five nested directories. Executable permissions are not copied;
invoke reviewed scripts with their interpreter. Harness-specific tool names and
dependencies may need adaptation. Importing does not grant extra permissions.

## Terminal interface

- Inline, scrollback-native transcript with a live composer; finished turns remain in normal terminal
  scrollback. Model, provider and settings panels use a full-screen overlay and restore the terminal
  when closed. Approval prompts and settings adapt to narrow terminal widths.
- A configurable status line shows model, role, path, Git state, context, token usage/rate, approval,
  sandbox and learning state. Settings let you choose presets or define left/right segments.
- Local inference shows **loading model**, **processing prompt**, then a live **generating tokens**
  counter and tokens/second in the composer. Dial's MLX runners report engine counts, including
  reasoning and tool tokens; the text runner also reports prompt processing progress. Other local
  servers use estimates marked `~` until reported usage arrives. Counters reset for each request.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.
- **Action palette:** `Ctrl-P`, `Alt-K` or `/palette` searches built-ins, custom commands and enabled
  skills without replacing your chat draft. `Tab` / `Shift-Tab` switch categories; `Enter` runs the
  selected action, asking for required arguments first. `Alt-Enter` edits optional arguments.
  During a turn, the panel explains whether the selected action runs immediately, waits or stops
  the turn. Pasting never runs an action. Provider discovery waits until Models or Providers opens.
- Dark, light, monochrome, Gruvbox, Nord and Paper themes; custom JSON themes, symbol presets,
  rounded or line composer, styled headings/code/diffs, expandable tool results, and `NO_COLOR`.
- Unicode-aware multiline editing, bracketed paste, persistent per-project prompt history and history
  search. Secret provider inputs are masked and never enter prompt history.
- Pasted text stays in the draft. Buffered unbracketed paste preserves line breaks, and an immediate
  trailing Enter after bracketed paste is ignored. Press Enter after the paste settles to send it.
- **Inline suggestions:** command arguments offer valid choices as dim ghost text even before you have
  used them: `/understanding ` suggests `architecture`, `/skills s` suggests `sources`, and
  `/skills enable ` suggests an installed skill name (including disabled skills). Built-in choices
  take priority over matching history; ordinary prompts still suggest the newest matching history entry.
  `Tab` or `Right` accepts the ghost; `Esc` hides it; `Enter` sends only what you typed. Suggestions
  appear only at the end of an idle draft, never in secret prompts or mid-line.
- **Slash commands:** typing `/` opens a list of every command (built-in and your `.dial/commands/*.md`)
  that filters as you type. `Up`/`Down` move (wrapping, with a position counter for long lists), `Tab`/`Right`
  complete, `Esc` dismisses without losing the draft. `Enter` on a partial name completes it so you can add
  arguments; `Enter` on a complete name runs it. Built-ins win over a custom command with the same name.
- While a turn runs, `Enter` steers the current turn: Dial cancels the pending model request or
  discards its stale response before executing tools. A tool already running may finish; unstarted
  actions in its batch are skipped, and completed results are retained. The latest instruction takes
  precedence over conflicting older plans and task summaries. `Alt-Enter` queues a follow-up for the
  next turn. The queue is visible above the composer. `Up` brings back the last queued
  message for editing. Interrupting preserves queued messages and the draft. Slash commands never become
  model steering: `/help`, `/hotkeys`, `/todo`, `/skills`, `/commands`, `/context` and `/clear` run at once
  without stopping the turn. `/model`, `/models`, and Alt+M open the picker immediately; a selection
  applies after the current turn, without canceling it. Closing the picker without selecting changes
  nothing. Other commands wait for the turn to finish, with a notice, except commands
  that replace or alter the active session/work: `/exit` (`/quit`, `/q`), `/new`, `/resume`, `/sessions`,
  `/tree`, `/fork`, `/undo`, `/rollback`, `/compact` and `/plan` stop it first.
- `/sessions` and `/resume` open searchable session pickers; `/tree` shows forks. `/rename <title>`
  names a session; `/export [path] [--html]` exports Markdown or self-contained HTML without
  overwriting existing files. `/copy` copies the last answer; `Ctrl-G` opens the draft in an external editor.
- `/plan [on|off]` enforces read-only planning. The model's `exit_plan` request asks for approval
  before enabling edits. `/permissions` lists scoped rules; for example,
  `/permissions allow --persist bash cargo test` persists a project rule outside the workspace.
  Deny rules take precedence, including in automatic approval mode.
- Idle terminal frames are not rewritten. Fenced code gets theme-aware lexical highlighting,
  and completed Markdown prefixes are cached while streaming. Edit and write cards show the change as a
  numbered, coloured diff (separate hunks with two lines of context, per file); the anchored preview rows
  and removed-lines report the model gets are left out. Each todo call renders the list as it now stands,
  with the steps that call closed struck through and emphasized. Passing automatic checks are one line.

| Action | Shortcut / command |
|---|---|
| Search application actions | `Ctrl-P` / `Alt-K`, `/palette` |
| Model selector / roles | `Alt-M` / `Alt-R`, `/model` |
| Provider setup | `Alt-P`, `/providers` |
| Settings | `Ctrl-,` (`Ctrl-4` fallback), `/settings` |
| Edit keyboard shortcuts | `/hotkeys`, or `/settings` → Setup → Keyboard shortcuts |
| Previous/next prompt | `Up` / `Down`; preserves the unsent draft |
| Search prompt history | `Ctrl-R` |
| Command list / completion | `/`, then `Up` / `Down`, `Tab` or `Right` to complete, `Esc` to dismiss |
| Accept inline suggestion | `Tab` or `Right` at the end of the draft |
| Multiline input | `Alt-Enter`; `Shift-Enter` when supported by the terminal |
| Expand/collapse tool results | `Ctrl-O` |
| Scroll transcript | `PageUp` / `PageDown` |
| Cancel a panel / stop at next callback | `Esc` / `Ctrl-C` |
| Quit from an empty composer | `Ctrl-D`, `/exit` |

`/hotkeys` opens a searchable shortcut editor when idle (during a turn it prints the active reference).
Type to filter by action, group or key; `Enter` edits, `Delete` restores defaults, and `Esc` goes back.
Enter chord names separated by spaces, such as `Alt+M Ctrl+K`; an empty field disables the action.
Changes save globally and apply immediately, only after validation and a successful write. Conflicting
bindings are rejected, and the editor keeps a way to confirm and close panels. You can also edit
`~/.dial/keybindings.toml` directly, for example:

```toml
"app.model.select" = "Ctrl+M"
"app.history.search" = []
```

`/settings` is the configuration hub. Type to search across every category; `Left`/`Right` change
categories and `Tab`/`Shift-Tab` jump sections. `Enter` toggles a switch or opens an editor/choice list;
choices are saved only when confirmed, and `Esc` cancels. `Ctrl-S` switches **All projects** / **This
project** storage; the title shows the destination and successful saves are acknowledged. `Delete`
removes the selected override so the inherited value applies. The detail area identifies the effective
source and restart requirements. The **Setup** category opens models, providers and keyboard controls.

To create a theme, put a JSON file such as
`~/.dial/themes/custom.json` there; it may extend a built-in (`dark`, `light`, `mono`, `gruvbox`,
`nord`, or `paper`) and overrides reload live. `NO_COLOR` forces monochrome output.

`Tab` changes global/project storage scope in the model and provider panels (`Ctrl-S` in settings).
The model picker switches between the main model and helper-role assignments. Supported roles are **main**, **feedback**,
**subagent**, and **compaction**; each routes real requests. “Use the main model” restores role inheritance.
Model rows include monochrome icons at the right for vision, tool use, reasoning and embeddings, with a legend and capability
labels in the highlighted model's details. ASCII themes use `V`, `T`, `R` and `E`. `?` means metadata is
unknown. Typing `vision`, `tools` or `reasoning` searches supported capabilities as well as names.
Local capability declarations come from downloaded configuration and chat templates; discovery does
not start inference or load weights to produce these badges.

Viewed images and user image attachments also appear as compact inline terminal thumbnails, up to
80 columns by 16 rows, with their filename and original dimensions. Previews preserve proportions,
shrink for narrow terminals and remain in scrollback. iTerm2, Kitty and Ghostty receive a sharper
native image preview; other terminals and multiplexers use portable colored cells. Image decoding
runs on a worker while input and interruption remain responsive. The model receives the original
image bytes independently of preview size; supported cloud image requests use high detail.

### Providers and discovery

The catalog finds cached Hugging Face MLX snapshots, MLX models downloaded by LM Studio (its
`downloadsFolder`, default `~/.lmstudio/models`; listed under Local MLX and served by dial's own MLX
server, so LoRA training applies), and running LM Studio, Ollama and llama.cpp
servers on their usual loopback ports. It also discovers OpenAI-compatible endpoints and Anthropic's
Messages API. OpenAI, Anthropic, OpenRouter, Groq, Mistral and DeepSeek have setup presets.

`/settings` → Model → Context offers **Current model context**, **Cloud context default** and
**Local context default**. Each accepts `auto` (the default) or a token cap from 2,048 to 2,097,152.
Auto follows the selected model's catalog/downloaded metadata; unknown capacities fall back to 32,768.
ChatGPT discovery uses `max_context_window` when valid and larger than the standard `context_window`
default, so the picker and Auto budget expose the account's extended capacity instead of stopping at 272k.
Subscription capacities come from the account catalog and may differ from API model-card capacities.
A numeric current-model override takes precedence over the cloud/local default, and known model or
loaded-server capacities cap the result downward. Local defaults also cover loopback/LAN endpoints.
Picking a different model resets its current override to Auto while preserving both family defaults.
The window controls retained history and compaction; it does not send that many tokens on every
request or reserve that much model memory. Actual usage appears before the slash in the context meter.
Configure the same values in TOML, for example `context_window = "auto"`,
`cloud_context_window = 64000`, and `local_context_window = 16384`.

Anthropic discovery reads `max_input_tokens`; older catalogs with missing windows use
documented limits for known models, including 1M tokens for Claude Opus 5.5. The status meter and
compaction share this resolved window. Custom endpoints use their own reported limits.

**LoRA indicators.** The model picker has a LoRA column: `unverified` for a local MLX model whose architecture
the installed mlx-lm implements (a static check of `config.json`, not proof of trainable adapter targets), `unsupported` when Dial’s adapter probe rejects it, and `-` for models another server runs. The main model distinguishes awaiting training,
training, paused for coding, evaluating, accepted vN, loading, loaded, used on last response, and request failed.

Local training yields to a new coding request at a safe step boundary. Dial saves adapter weights,
optimizer state, RNG state and the batch cursor, exits the training process to free GPU memory,
and hands compute to the coding turn. Idle chats unload their local models when the paused trainer
requests compute back; training resumes the same candidate and remaining iterations automatically.
The currently admitted adapter stays in use until the candidate passes evaluation. Dial allows
two seconds for cooperative handoff before terminating the training subprocess. `/improvement`
and `dial improve jobs` show the current phase and iteration progress. Operator stop also cancels
the idle wait. Resume occurs within the running improvement cycle; restarting a stopped or crashed
cycle does not automatically recover its unfinished candidate. Older already-running workers can
be stopped from the compute menu and restarted with the updated binary.

Open `/compute`, or **Settings → Setup → Compute & background work**, for a live menu of compute
ownership and processes across all projects. **All**, **Models**, **Background**, **Services** and
**Tools** tabs separate model servers, capability checks, improvement jobs, Laya and tool subprocesses.
Live processes and the compute owner appear green, including models still loading. Process rows show
PID, CPU use and memory in decimal GB. On macOS this uses physical footprint, including compressed
and GPU-backed allocations, with RSS as a fallback. The header totals Dial processes and sessions,
shared services, and their combined memory; shared services are counted once. Process details show
RSS separately. These observational totals are not a measurement of the whole machine's memory use.
Use Left/Right or Tab to switch sections, Enter for details, and **x** to stop/unload a process or
remove and disable a background job. Saved model weights, adapters and session history are preserved;
models reload on their next use. Orphaned model servers from older Dial versions are included too.
The shared OMP Laya service appears in All and Services as `Laya (OMP/shared)`; stopping it also
affects OMP.
Pause/Resume background work and Start/Stop controls remain available. While waiting for compute,
Ctrl+, opens these controls directly. Status refresh and job controls run off the terminal thread.
Idle chats unload resident main and role models as soon as the turn and its child agents finish,
including when no other job is waiting. Queued follow-ups keep their models; the next turn reloads
an unloaded model. Verification and evaluation also yield for coding requests.
If training does not acknowledge a pause within two seconds, Dial terminates its subprocess and
resumes from its last committed step later. A checkpoint is committed at every step, so only an
unfinished step may need repeating. **Stop job now** targets a verified detached background worker
and its child processes, preserves the admitted adapter, and abandons that unfinished cycle; it
never terminates a foreground chat. This also provides recovery for older workers without preemption.

A failed capability probe overrides the static architecture check; only a successful probe can show
awaiting training. Architecture support alone never claims LoRA compatibility.
Opening Dial or browsing the model catalog reads saved LoRA results without loading weights, including
models assigned to roles. Unverified models stay unverified until an explicit F6 check or improvement
work needs the capability probe. Results and reasons are saved in each model's private project store
and reused across restarts. F6 runs a check with diagnostics after runtime/model changes under the shared
compute lock. Verification loads the model/tokenizer, selects architecture-compatible linear
projections, performs a rank-one gradient
update on a short prompt, and checks that a saved/reloaded adapter preserves output. The temporary
adapter is discarded. Ordinary attention keeps query-only targets; hybrid, recurrent, and fused blocks
use compatible output or other linear projections. Training and adapter validation share this selection.
Old probe results are rechecked on the next explicit verification or improvement run.
The managed Python worker updates with the binary; explicit custom worker paths are preserved.
Verification does not establish that a full training run will fit memory or improve quality.
Accepted means the gate admitted the version; loaded requires the local server's startup generation
check; used requires a successful, uninterrupted model response with that adapter. Runtime evidence
is session-local and clears when the server is released. Training/evaluation indicators require a live
worker lock, so a crashed worker cannot leave a permanent busy label. Disabling future training does
not hide an already accepted adapter. `/improve` shows the runtime adapter path, latest LoRA gate
decision and baseline/candidate scores, plus the last training skip reason. Scores measure the gate's
tasks; successful adapter use alone does not prove improvement. Once the learning loop has probed
the model, its recorded capability replaces the static architecture check.

`/providers` can add a custom endpoint, set/remove a URL-scoped API key, and choose a model. OpenRouter
also offers **Sign in with OpenRouter** (PKCE), which creates a user-controlled API key without asking
you to paste one. Dial checks that key against OpenRouter's protected `/api/v1/auth/key` endpoint
before saving. The option appears only for OpenRouter's official endpoint, so a generated key is never
forwarded to a custom gateway.

When entering OpenAI or Anthropic API keys, Dial checks them against each provider's model-list
endpoint before saving; this is metadata, not an inference call. OpenAI uses bearer authentication;
Anthropic API keys use `x-api-key` and `anthropic-version`. Other custom endpoint keys are saved
without an online check. Environment credentials take precedence over saved API keys. Saved keys are
**owner-private plaintext** in `credentials.json`, not an OS keychain; they are never written into
TOML or rendered back into the UI. Failed discovery keeps cached models marked unverified. Anthropic
uses its Messages API; other remote endpoints use OpenAI-compatible chat completions. Both
**ChatGPT subscription** and **Claude subscription** have dedicated OAuth sign-in flows and use their
respective official inference APIs.

**Use your ChatGPT/Codex subscription.** Restart Dial, open `/providers`, select **ChatGPT
subscription**, and choose **Continue with ChatGPT (add account)**. In the browser, authorize Dial
to use your ChatGPT plan. Return to Dial, open `/models`, and assign an account model to the main
role. Eligible plan access and shared limits are controlled by your account; the model catalog is
not an entitlement check, and your first completed inference request verifies access. Manage Dial's
permission and limits in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

The provider menu lets you choose a saved workspace, sign in again, or sign out. Dial refreshes
rotating tokens under a cross-process lock. Sign-in continues if you close the provider panel; use
**Cancel pending sign-in** in its menu to stop it. OAuth credentials are stored in owner-private,
atomic `chatgpt-accounts.json` storage under Dial's user directory (plaintext, not an OS keychain),
separately from API keys and editable project files. Requests use the ChatGPT Codex Responses
endpoint at `chatgpt.com/backend-api/codex/responses`, include the account ID, and never fall back
to an API key. A partial or failed response never releases its tool calls. Dial follows OpenAI's
[Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source). Existing
accounts created by Dial's older login flow must sign in again. Legacy
`https://api.openai.com/v1` ChatGPT endpoint settings are migrated to the Codex backend on load.

Dial combines the account model list with visible models in a recent local Codex model metadata
cache, when present, so newly released models can appear before the account list catches up.
Those additions are labeled **Codex catalog**; their first completed request verifies account access.
Dial reads model metadata only from Codex and uses its own OAuth credentials for every request.
For ChatGPT models, `/reasoning` shows the current and supported levels. Use `/reasoning high`,
`/reasoning max`, or another advertised level to change it; `/thinking high` is an alias. Choices
persist per model in this project's settings and are sent through the Responses reasoning field.

**LM Studio.** Dial reads LM Studio's native model list (`/api/v0/models`, loopback or LAN hosts only) to show
whether each model is `loaded` (with its real context length) or `loads on first use`, and greys out
embedding models, which cannot chat. LM Studio loads a model on demand with a context length of its own
choosing, often far below Dial's 32768 default (an 8192 load was seen in practice); Dial asks the server
what is actually loaded and plans compaction and the context meter against the smaller number. A model
LM Studio refuses to load (memory guardrail, unsupported architecture) is reported with the server's own
message; load it in LM Studio first, with a smaller context, or choose another model. Larger models need
LM Studio's own resource limits to allow them; Dial cannot override those.

**llama.cpp and Ollama.** The same runtime check covers them: llama.cpp's `/props` reports the per-slot
context it serves (`-c` divided by `--parallel`; its `/v1/models` lists only the training context), and
Ollama's `/api/ps` reports the context a running model was loaded with. Ollama's default is small and it
truncates oversized prompts silently, so raise it with `OLLAMA_CONTEXT_LENGTH` (or `num_ctx` in a
Modelfile) for agent work; Dial learns the size after the first reply that loads the model.

Use any OpenAI-compatible server instead of the local MLX model (LM Studio, Ollama, llama.cpp, a hosted API): `dial --endpoint http://localhost:1234/v1 --api-key-env KEY -m <model-name>`. Weight updates need the local MLX model.


### Automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, todo, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Exit status: `0` completed, `1` error, `2` interrupted/no answer, `3` required approval denied.

`dial sessions --all` lists sessions across projects. `dial sessions rename <id> <title>` and
`dial sessions export <id> [path] [--html]` expose session management without the TUI.

The **Session rail** above the composer shows the current session and per-file additions/deletions;
new files are labelled `new` and binary diffs `binary`. `/changes` lists the cached workspace snapshot.
**Live worker cards** show synchronous and background child phases, step counts and streamed previews.
They stay bounded on small terminals and running workers remain visible between turns. Toggle either
panel in `/settings` → Appearance → Display. Ctrl-P opens the searchable command palette; viewed
images use compact inline previews when supported by the terminal.

Execution uses shared, revisioned result bodies backed by the session's branch-aware JSONL journal.
RPC `state` exposes a `tree` containing session, conversation and execution nodes. Each execution
node has an `id`, `parent`, `kind`, `label` and `result` (`text`, optional structured `data`, `phase`,
`revision`). `state_reset` clears the current observer view; `state_node` replaces one node and
`state_delta` appends text to an existing node at the supplied revision. Worker updates use bounded
replacement snapshots. Legacy events remain available; clients should choose one projection to
avoid displaying a token twice. Tool-result events include original structured `data` when supplied.
Snapshots are written at boundaries, not for every token. Resuming never restarts unfinished workers.
Unassigned child roles inherit the parent model, profile and request controls; the model picker can
still assign an explicit child model.
Ctrl-C interrupts silent HTTP response reads without replacing the configured inference deadline with
a short timeout. Connection establishment and partial writes still use their configured deadlines.

For repeatable repository checks, create a TOML suite and run `dial bench suite.toml --report result.json`.
Each case is copied to a fresh temporary workspace and must declare deterministic checks; for example:

```toml
version = 1
name = "parser regression"

[[cases]]
id = "parses-emoji-path"
prompt = "Fix the parser so a path containing emoji is preserved."
checks = [
  { kind = "file_contains", path = "src/parser.rs", needle = "emoji" },
  { kind = "command", command = "cargo test --offline --lib parser", expect_exit = 0 },
]
```

`dial bench suite.toml --baseline old.json --report current.json` reports pass-rate, token, latency and
tool-call regressions and exits nonzero on regression. Command checks run in the disposable copy with
network disabled; state, `.dial`, build caches and common credential files are excluded from the copy.
The report omits prompts and model output. The Rust `BenchmarkSuite` API accepts custom agent and
verifier callbacks for normalized reports from other harnesses.

After edits, dial derives focused Rust and Python syntax checks from checkpointed changed paths.
`[verification].commands` can add operator-owned sandboxed commands (name, command, optional file
extensions and timeout); a failure is sent to the agent for the configured bounded repair attempts,
then the edited tree and final diagnostics remain visible for `/undo` or manual repair. Approval-denied
writes are not treated as successful verification. `todo done` is a checkpoint too: an investigation
requires a successful tool call since it started. An active non-investigation step may instead close
after a nonempty text-only answer was delivered for that step, provided no edits await verification.
This records delivery, not an independent judgment of the report's quality. Delivery does not carry
over to the next step. Reworded `todo done` failures for the same target share a four-failure retry
limit and end the turn as incomplete rather than looping. Steps that edited files still run focused
checks, so a broken step is caught while its context is fresh. A failed check keeps the step open
with diagnostics; repeating `done` alone does not convert a failure into verification. Turn-end
verification is skipped when the last step check already covered every edit.

Extended investigation must use real todos, not a prose plan. In live runs with the todo tool available,
after a task-dependent budget of four, six, or ten successful inspection calls without an open plan,
the next request offers only `todo` until
the model creates or appends open tasks; a direct final answer remains allowed. Bookkeeping cannot
reset this checkpoint, and non-planning tool calls are refused while it is active. Ordinary short
questions and sealed evaluations are unaffected. Guidance directs the model to append necessary
follow-on work uncovered by results before pursuing it, preserving existing tasks and findings; use
`start` when a newly discovered prerequisite must come first.

For nontrivial changes, the default guidance starts with 1–3 investigation questions in `todo`
(`investigation: true`), then implementation and verification. Investigations start from saved project
understanding and stop once the affected path, proposed change and relevant check are known. Closing
an active investigation requires `finding` and 1–4 `evidence` references as well as the normal successful
tool-activity gate. Findings survive session resume and compaction; the latest three are included with
the current step so the agent can reuse answers. Reopening a question clears its old conclusion.
These references are agent observations, not independently validated proof. Simple changes skip the
checklist. A bounded, turn-local observer redirects identical successful read/search calls that return
unchanged results; it executes the read before comparing, so external changes remain discoverable.
Different ranges, changed results, artifact recovery and command polling are allowed. Mutations and
shell commands clear the observation cache. The redirect is advisory, not a hard ban on rereading.

Mid-turn requests end with a never-stored reminder of the current todo step, next step and recent findings.
Because it sits after the newest tool result, every earlier byte stays identical between requests and a
prefix (KV) cache only needs to process the appended reminder. Tool results are clipped to about an eighth of the
context window (4–24 KiB); successful retrievable read/search/shell/web previews are additionally capped
at 8 KiB. Failed diagnostics and mutation results retain the larger context-dependent budget.
Clipping keeps both ends plus error, panic, failed-test and traceback lines from the omitted middle;
full output stays in artifacts. `bash` also bounds the output it holds in memory.

`[dap.<id>]` defines an operator-owned adapter executable. The `dap` integration exposes
`dap_<id>` actions and requires approval on every call; adapter startup is lazy, and launch program
and cwd paths must resolve inside the workspace. `[mcp.<id>].lifecycle_hooks` opts into
`turn_started`, `turn_finished`, `tool_started`, and `tool_finished` notifications through
`notifications/dial/event`; payloads contain identifiers/status only, never prompts, arguments,
outputs, or source content. Hook delivery is observational and cannot bypass tool approvals.

Transient provider errors use bounded backoff with server retry hints and visible notices (at most five
attempts and 60 seconds of backoff). This includes macOS socket `Invalid argument (os error 22)` and
TLS `BadRecordMac` alerts before a response arrives. Each retry opens a fresh connection; TLS certificate
verification remains enabled. Exhausted retries pause the turn with a reason and instructions to continue; authentication
errors and failed response streams are not replayed. `@image.png` and image `read` calls attach actual
PNG/JPEG/GIF/WebP pixels up to 5 MiB. To supply an image outside the project, explicitly reference its
absolute path or home path, such as `@"/Users/me/Desktop/Screenshot 1.png"` or `@~/Pictures/example.jpg`.
Pasted standalone or quoted image paths also attach automatically without `@`. The model can reread
exactly those supplied images; other external tool reads stay confined. Sessions store file references and content hashes, not base64;
changed or missing attachments are skipped with a notice rather than silently replaced on replay.
Browser screenshots attach rendered pixels automatically, and Dial's visual-work guidance calls for
proactive inspection when looking would help. Visual tasks receive an initial inspection reminder;
edits to styles/templates/components trigger a bounded completion checkpoint when no fresh rendered
pixels have reached the model. Earlier screenshots become stale after subsequent edits. If visual
inspection remains unavailable, the final result explicitly reports that it was not completed.
Known non-vision models skip images with a notice to both the user and model. Downloaded local models
with a vision encoder in `config.json` use MLX-VLM and receive the image pixels; text-only models use
MLX-LM. `dial setup` installs both runners in Dial's private runtime. The vision runner retains at most
one image-feature cache and disables persistent prefix caching. External endpoints with unknown
capabilities are attempted and may be rejected by the provider; use a vision-capable model for visual work.
`/cost` shows accumulated cost only when catalog pricing and token usage are available; no local-model
price is invented. It also shows observed main-agent input, output, cached-read, cache-write and reasoning
counts for the current runtime, with response-count coverage. These counters do not yet include worker
usage or transport retries and are not restored across process restarts. Reasoning may overlap output;
do not add them twice. Missing provider usage is unknown, not free.


### Measuring efficiency

From this checkout run `dial benchmark examples/benchmarks/efficiency.toml --profiles machine.toml --report efficiency-report.json`
using the machine/profile configuration described below. The five disposable cases cover a simple
question, a one-file edit, a failing-test repair, a multi-file rename, and continued-session compaction.
Compare correctness first, then observed prompt/output/cache/reasoning usage, calls, errors and elapsed
time. The simple-question case checks tool avoidance only; manually review its answer for correctness.
The continued-session case tests multi-turn compaction, not process restart/resume. No live-model savings
are claimed by these fixtures, and prompt cache hits need not be free under a subscription limit.
## Model controls, embedding SDK and persistent RPC

Profiles declare capabilities for an **exact** provider/model; no model-name heuristics select reasoning,
sampling, vision or context behavior. Select with `--profile name` or `/profile name`; `/reasoning <level>`
and `/thinking on|off` update the selected profile and reject unsupported combinations.

```toml
provider = "openai"
model = "operator-model"
model_profile = "main"
[endpoint]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
[model_profiles.main]
provider = "openai"
model = "operator-model"            # replace with the exact supported model identifier
max_tokens = 8192
edit_mode = "replace"
reasoning_effort = "low"
[model_profiles.main.capabilities]  # declare only capabilities documented for that exact model
tools = true
reasoning = "openai-effort"
reasoning_efforts = ["low"]
output_limit = "max-completion-tokens"
```

Native Anthropic profiles support declared budget/adaptive thinking and `output_config.effort`.
`prompt_cache=true` requires declared caching support and adds stable system/tool cache breakpoints.
OpenAI caching is automatic. Cached read/write usage is reported separately; unknown counts/prices
remain unknown rather than being fabricated. Output-limit recovery honors its increased request budget.

For browser setup, install outside the editable workspace:
```sh
npm install --prefix "$HOME/.dial/browser" playwright
"$HOME/.dial/browser/node_modules/.bin/playwright" install chromium
```
Set `[browser] playwright_module` to that absolute module path. Approved browser `open`/`navigate`
requests to `localhost` or `127.0.0.1` automatically permit only that exact host and port for the browser
session, so local development sites work without configuring each port. Redirects and subresources
cannot grant access to another local endpoint. `/settings` → Tools → Browser controls this behavior;
`auto_localhost=false` restores explicit allowlisting with `allowed_hosts=["127.0.0.1:3000"]`.
Other private hosts still require operator allowlisting. Metadata/link-local addresses remain forbidden.
Use managed `bg` processes for previews: list/reuse a running server, start one if needed, wait for
readiness, then open its HTTP URL. Foreground `bash` kills servers at its deadline. The browser accepts
`about:blank`, and tab IDs may be omitted when exactly one tab is open; multiple tabs require an ID.
Browser failures show the underlying network refusal before Chromium's generic error. Networking is
paused between approved browser operations. Python and
Poppler must also be installed when using kernel/PDF capabilities.

`dial rpc` is a persistent version-1 JSON-lines control endpoint. Requests have `id`, `method` and `params`;
responses correlate by `id`, while unsolicited events have `event`. Methods include `send`, `state`,
`messages`, `compact`, `seed_context`, `new_session`, `resume`, `rename`, `plan`, `steer`, `abort`,
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

The Rust SDK exposes `extensions::{Extension, ExtensionHost, Command}`, versioned `tools::ext::Tool`,
atomic tool registration and observational lifecycle events. Operator-trusted extensions can register
tools, prompt commands and render panels through `Ui`; executions still pass the agent's normal
validation/approval boundaries. It is a Rust embedding API, not a loader for arbitrary workspace plugins.

Benchmark suites now support seeded `context_messages`, sequential `turns` with `compact_before`,
categories and bounded metric assertions. `dial bench suite.toml --profiles profiles.toml` selects
configured model-profile names and validated ablations (`no_integrations`, `replace_edit`, `hashline_edit`).
The first supplied profile is the baseline; comparisons rank verified success before latency.
Run `python3 examples/benchmarks/smoke.py --dial ./target/debug/dial` for deterministic capability
regressions, or add `--profiles examples/benchmarks/profiles.toml`. Scripted-provider results are harness
regression evidence, not claims about real-model quality.

## What it does

**Tools** (file edits confined to the project; shell access follows the chosen sandbox)
- The lean default is `read`, `write`, `edit`, `glob`, `grep`, `bash`, `todo`, `tool_search`, `tool_load`, `artifact_read` and `working_state`. Specialists are discoverable by name/capability rather than sending every schema on every request. `tool_load` takes `names`; newly enabled schemas are available in the **next** model round, and enabled selections survive session resume. Explicit profile allowlists remain authoritative.
- **Subagents** (`task`): `scout`, `reviewer`, `planner` and `worker` retain independent contexts, role routing, approval boundaries and structured-output validation. `action="run"` is blocking; `start` returns live IDs, then `status`, `wait`, `message` and `cancel` operate on those jobs. Peers use `task_message` for coordination and artifacts for shared evidence. `/jobs`, `/message <id> <text>` and `/cancel <id>` expose live controls to the user. Inbox/history/concurrency are bounded; children cannot spawn children, and workspace mutations remain serial.
  Blocking task calls still keep the UI responsive: both serial children (including workers/local models)
  and parallel batches run off the UI thread. Timed event pumping updates the timer and handles input
  even while a child produces no output; cancellation prevents pending approvals from authorizing work.
- `read` takes line selectors in either edit mode: `path:50-200`, `path:50+150`, `path:-60` (last 60 lines), `path:5-16,960-973`, or `offset`/`limit`; the footer states the range returned and the file's total line count, and in hashline mode only the lines shown can be edited. `grep` matches a Rust regex per line (`fixed_string` for literal text) with `ignore_case`, `glob`, `context` lines and `max_results`.
- **Edit modes.** The default `replace` mode edits by exact text (`old` → `new`). `--edit-mode hashline` (as in omp) has `read` return `[path#TAG]` and `N:text` rows and `edit` take `PUT`/`CUT`/`MV`/`REM` operations anchored to those line numbers, with registers for moving text. Stale tags are recovered only when the file's changes provably do not touch your anchors; unseen lines cannot be edited; multi-file patches are validated before any write; every result lists the existing lines a replacement removed, because `PUT N.=M` replaces N through M inclusive and an off-by-one silently deletes a neighbour. Hashline suits strong models; a local 35B-A3B model was observed repeatedly deleting adjacent lines with it, which is why `replace` is the default. Block anchors (`N*`) use a bracket/indent heuristic, not a parser.
- **Loop safeguards.** The system prompt is identical on every request of a session (until a tool is loaded or plan mode changes); recalled notes and verified repair patterns are decided once per user turn and stored with that message, so a provider or local server can reuse its prompt cache. A reply that stops at the output-token limit is never taken as the answer. Reasoning-only replies use the reasoning gate below; truncated answer text or tool arguments can retry with a doubled budget that fits the context window, then receive bounded nudges. `harness.max_tokens` defaults to 8192. Tool results over 24 KiB keep their beginning and end with an explicit omission marker; `read`, `grep` and `bash` cap themselves at 20 KiB and say how to fetch the rest, so paging footers are never lost. A replace-mode `edit` that misses explains why (line numbers copied from `read`, differing indentation with the file's real text shown, or a drifted block with the line where it starts), edits CRLF files with LF text, and lists the line numbers of ambiguous matches.
- **Reasoning gate.** Before answer text or tool arguments begin, main and child agent requests have an operator-owned deadline of 60 seconds and a cap of 16 KiB of streamed reasoning. Reasoning events and keep-alives do not reset the deadline; hidden reasoning is covered by time, not guessed token counts. Connection setup, model startup, authentication and provider backoff count toward that wait (startup itself may take longer to cancel). A hit cancels only that request and discards all its partial calls. Dial allows one action-focused recovery per turn, with no token-budget increase or step refund; another hit ends the main turn as `incomplete` or fails the child job. Empty reasoning-only output-limit replies share that same recovery allowance. Successful requests do not replenish it. Notices explain interceptions and main-agent metrics expose `reasoning_interceptions`. Adjust the gate in `/settings` under Models / Roles & Sampling or `[reasoning_guard]`; `enabled = false` restores the previous recovery behavior. These settings are outside the improvable harness. Compaction and semantic reranking retain their own request limits.
  The streamed-reasoning byte cap remains active after answer text or tool arguments start; a short preamble cannot disable it. Main and child agents also compare successful observations, including `bash` results: changed timeout arguments and artifact handles do not disguise unchanged evidence. Repetition gets explicit investigation feedback; the main agent temporarily withholds the repeating tool until a different action succeeds. Four repeats of the same unchanged observation stop the run; unrelated checks and errors do not erase its history. Successful edits allow fresh verification, and new user steering resets the turn-local guards. Repeating a substantial explanation three times also sends a loop self-check to the model, even with different calls. Wording alone does not stop genuine progress; result and failure guards enforce those stops.
- **Model I/O robustness.** After answer text or tool arguments begin, streamed replies are bounded by silence, not by total time. Remote models use `remote_model_idle_timeout_secs` (180 seconds by default, capped by `worker_timeout_secs`); raise it in `/settings` for models that reason longer without stream events. Local servers retain `worker_timeout_secs` after their first event; the reasoning gate separately bounds their pre-action wait. Keep-alive comments and read boundaries never reset the event deadline. While waiting, the UI stays editable and shows elapsed time without output; Ctrl-C or Esc cancels the request. Tool-call arguments that are almost JSON are repaired rather than regenerated: raw newlines/tabs inside strings, invalid escapes such as `\d`, single quotes, unquoted keys, trailing commas and a double-encoded object are accepted as written; cut-off or trailing-garbage arguments are refused and never executed.
- **Whitespace-tolerant `edit`.** If `old` is not found byte-for-byte but matches exactly one place once leading/trailing whitespace is ignored, and every line differs by the same indentation prefix, the edit is applied with `new` shifted by that prefix and the result says what was matched. Two candidates, uneven indentation or any text difference is refused with the file's real text shown.
- **Forgiving hashline bodies.** A `PUT` body row written without its `+` prefix, or a raw blank line between body rows, is taken as content and the result warns how many rows lacked the prefix, instead of failing the whole patch. Rows that look like unified-diff deletions (`-…`) are still refused. Lines the model was shown are rebuilt from the session's earlier `read`/`edit`/`write` results when the agent restarts or is rebuilt, for files whose content is unchanged since, so a resumed session does not reject edits with "no read of this file is recorded".
- **Compaction is checked, not assumed.** The trigger (80% of the window) uses the provider's own count for the last request plus a local estimate of everything added since, so a large tool result is counted before the request it would overflow, not after. After summarising, dial estimates the next prompt the same way (the fixed system prompt and tool schemas included). Above 70% of the window it compacts once more, harder; if it still would not fit it says so and names the fix (a larger context window or fewer `integrations`) instead of summarising again every step.
- **Stuck-turn detection.** Besides the identical-call guard (warns at the third identical failure, stops at the fourth), dial counts failures whatever the arguments: the same file tool (`read`, `edit`, `write`, `glob`, `grep`, `ast_edit`, `ast_search`, `artifact_read`) failing five calls in a row gets a warning and ten stops the turn, even with successful reads in between (the read-guess-read-guess loop); `bash` and `kernel` failures are task feedback and are never counted. Four rounds in which every call failed add a warning, eight stop the turn. A stopped turn is **reported**, not raised: the reason is recorded in the conversation, shown as a notice, the outcome is `incomplete`, and print mode exits 2 (with `--output-format json` the result has `outcome: "incomplete"` and no `error`). The step limit ends the same way.
- **Step budget.** `harness.max_steps` counts model calls that did work; `0` means unlimited interactive turns, including when an admitted harness has a finite budget. Positive values keep their step limit. Automatic repair and sealed evaluation still have finite caps. A reply that was unusable (truncated answer text or tool arguments, an unreadable call, a tool call written as plain text) is retried without spending a step, up to eight times per turn, and compacting after the server rejected the prompt size is never charged. Sealed evaluation runs get no refunds: their budget is what they measure.
- **Completion and todos.** `todo` keeps a numbered plan: `set` (optionally grouped by `phase`), `append`, `start`, `done`, `drop` and `block` (with a reason). Whenever nothing is in progress the earliest pending task starts, so "finished this, on to the next" is one `done` call. The open list is pinned above the composer in the TUI (progress, current step, blockers; long lists window around the active task), streamed as `todo` events over RPC and `stream-json`, carried verbatim through compaction, and survives session resume. If the agent stops with pending or in-progress tasks, dial asks it to continue through tools, up to three reminders; another reminder requires a changed todo list or new successful tool evidence, so repeated reads and text-only promises cannot loop forever. Stopping when every open task is blocked ends the turn as `blocked` without reminders. The `finish` control tool reports `completed`, `blocked`, or `needs_input`. Completion refuses open (including blocked) todos and running live tasks; blockers and questions require a concrete `reason` and leave open work intact. Ordinary informational replies still work without todos. JSON print results and RPC `send` responses expose `outcome`; blocked, needs-input, incomplete, and interrupted print runs exit with code 2 instead of reporting success. These controls do not independently prove that a model's task-status claims are true; relevant checks still run after completed edits.
- **Specialist integrations** (available per `integrations` in the config, loaded on demand; never in sealed evaluation/self-play). MCP tools follow `[mcp.*]`; enabling a schema never bypasses approval:
  - `bg`: long-running processes (`start`/`list`/`output`/`wait`/`kill`) under the same sandbox as `bash`, in their own process group, at most 8 at once, 256 KiB of output kept each, all killed when dial exits. Browser tool loading also exposes `bg` when enabled. Start or reuse local preview servers here, wait for readiness, then open the actual HTTP URL; foreground `bash` kills its process group on return/timeout and refuses common dev-server commands before launching them. Stop temporary preview processes you created after inspection, preserving existing user services.
  - `web_fetch` / `web_search`: HTTP(S) fetch with HTML→text conversion and an SSRF guard (loopback, private, link-local and metadata addresses are refused on every connection and redirect), and search through DuckDuckGo's HTML endpoint (a scraping path that can break) or your own `[web] search_url` template containing `{query}`. Both need approval and are disabled by `sandbox.mode = "strict"` or `allow_network = false`.
  - `lsp`: definition, references, hover, implementation, type_definition, document_symbols, workspace_symbols, diagnostics and `rename_preview` (never applies) through language servers found on `PATH` (rust-analyzer, typescript-language-server, pyright/pylsp, gopls, clangd) or `[lsp.<language>]`. Servers run unsandboxed, so the call that would start one asks for approval.
  - `ast_search` / `ast_edit`: structural search and rewrite with ast-grep patterns (`$VAR`, `$$$ARGS`) over 28 languages. `ast_edit` previews by default; applying is all-or-nothing, atomic, checkpointed (`/undo` works) and refuses results that no longer parse.
  - MCP: every tool of each `[mcp.<name>]` stdio server appears as `mcp__<server>__<tool>` and requires approval. A server's `readOnlyHint` is not an authorization boundary. Servers are trusted operator programs (unsandboxed); a server that fails to start only produces a notice.
  - `find`: repository retrieval by behavior or exact text. Default behavior mode collects bounded, source-grounded lexical and syntax-declaration candidates (including declarations with no query-word overlap), then asks the configured model to rank candidate indices; paths, spans and snippets come only from captured workspace evidence. `ranking="lexical"` is deterministic and local; `mode="exact"` never calls a model. The semantic request is bounded and non-exhaustive, uses no embeddings or probability scores, and errors honestly if the model or response is unusable. Optional `lsp_path`/`lsp_query` adds live LSP metadata.
  - `browser`: real Chromium tabs, navigation, DOM actions/evaluation, screenshots and console/network evidence. Requires approved interactions and externally installed Playwright/Chromium; exact operator-owned local host/port exceptions enable dev-server verification.
  - `kernel`: persistent Python state, expression results, stdout/stderr and tracebacks. Approval required; reset/deadline/cancel discards the namespace. Required OS fork containment prevents subprocess creation: use `bash` for child processes.
  - Rich `read`: PDF pages (`file.pdf:page=2-4`), notebook cells (`file.ipynb:cell=2`), ZIP/TAR/gzip members (`file.zip:docs/readme.txt:2-20`), and read-only SQLite (`data.sqlite:table?offset=0&limit=20` or `:table:key=2`). Archives never extract to disk; notebook cells never execute. PDF requires Poppler `pdftotext`.
  - Shell/background output is streamed to the UI and fully spooled before clipping. `artifact://<sha256>` references survive compaction; `artifact_read` retrieves line ranges or exact byte pages. Abandoned temporary spools are removed.
- Native tool calling over streaming; Hermes-format `<tool_call>` text is understood when the server does not parse it.

**Safety**
- Approvals: `ask` (default) presents Allow once / Always allow / Deny before mutations, shell execution and operator tools; `auto` / `-y`; `readonly`. A small set of inspection shell commands is exempt, but builds, tests, package scripts, Git and ripgrep still require approval. Always-allow for bash records a command prefix, not blanket shell access. Unsupported compound shell syntax cannot inherit a prefix grant. Without a terminal, `ask` denies unless a named tool or permission rule authorizes it.
- Shell sandbox (macOS `sandbox-exec`, fails closed): `dev` (default) can read most of the machine but never `~/.ssh`, `~/.aws`, cloud/keychain dirs or dial's own state, writes only the project (plus configured `extra_write`), secret-looking environment variables are dropped; `strict` is workspace-only with no network; `off` is an explicit opt-out. `sandbox-exec` is deprecated and this is not a security boundary against a determined local attacker.
- Checkpoints: every file change is journaled per turn. `/undo` drops the last turn and restores its files; `/rollback n` restores files only.
- Configuration, state, memory, sessions and evaluation data live under `~/.dial` (outside the project), so the agent cannot edit its own approvals, sandbox or gate.

**Context (omp / Hermes style)**
- Project files: `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, `.dial/AGENTS.md` up to the git root; `~/.dial/SOUL.md` for identity; `.dial/SYSTEM.md` / `APPEND_SYSTEM.md` overrides. Treated as data, not commands.
- `@file`, `@dir/`, `@diff`, `@staged` references in messages.
- Skills (`.dial/skills/<name>/SKILL.md`, `~/.dial/skills`, agentskills.io format) loaded on demand; the agent can create and refine them (`skill_manage`).
- Provenance memory lives in private `$DIAL_HOME/memory.sqlite3`. `recall`, `retain`, `memory_update`, `forget`, `memory_history` and approved `memory_import` support project/global scope, source/session provenance, revision-checked corrections and expiration. Relevant bounded entries are retrieved on demand with lexical BM25 ranking. Legacy `MEMORY.md`/`USER.md` import is explicit (`confirm=true`) and preserves originals; old configuration tool selectors are migrated, not kept as executable aliases. Forget is logical redaction, not forensic disk erasure.
- **Saved codebase understanding.** Dial automatically builds a project-scoped source map in private `$DIAL_HOME/projects/<identity>/codebase.sqlite3`, outside the repository. It records file outlines, actual syntax declarations, imports, module documentation and bounded manifest/README excerpts; unchanged file hashes reuse saved outlines across restarts and new sessions. Each turn refreshes changed sources in a background worker and attaches a relevant slice to that user message, keeping the system prompt stable. Independent child agents reuse the same map. Agents save deeper responsibilities, relationships, entry points and check commands with `codebase_memory` `action="remember"`, a `topic`, factual `summary` and 1–8 supporting `sources` paths. Findings carry source hashes, the originating parent session and a revision; changed, removed or inaccessible sources withhold the old summary and expose the topic/revision/paths for targeted refresh. These are agent observations, not independent verification proof. `action="recall"` retrieves more; `refresh` updates the map; `forget` removes a topic. Updates and forgetting require the observed `expected_revision`; a new topic defaults to 0. Cache operations do not edit project files or require a project mutation approval. The automatic map respects ignore rules and excludes generated directories, links, binary and oversized files; copied text uses the existing secret/instruction screening. Scans are bounded to 2,048 files, 16 MiB and three seconds, with partial coverage reported; files over 256 KiB are omitted and at most 128 findings are saved. Source outlines are structural excerpts, not a complete call graph or generated architectural explanation. Default turn context is 8,000 bytes. Configure `[codebase]` or `/settings` → Models → Project understanding; disable it with `enabled=false`. Sealed evaluations receive no persistent project knowledge.
- Sessions: JSONL per project, resume/fork, rewind, automatic summarization when the context fills (`/compact`), full-text `session_search`.
- Historical evidence stays append-only. The request context is a projection of it, built to keep the conversation prefix byte-identical from step to step so a provider or local server can reuse its prompt cache: recalled notes are stored with the user turn they were recalled for, a superseded read becomes a fixed placeholder once (and only while little text follows it, or when the context is filling up), and under context pressure old, large `read`/`grep`/`glob`/`find`/`bash`/`lsp`/web results outside the newest fifth of the window are replaced once by a pointer to their full text (`artifact_read` retrieves it). Mutation results, todo and memory updates, and small results are never replaced. `working_state` preserves explicit goals, constraints, decisions and unfinished work; compaction also carries the original goal and evidence references. Agent notes are not verification proof.
- Custom slash commands: `.dial/commands/*.md` (`$ARGUMENTS`, `$1`…).
- Session commands include `/help /new /sessions /resume /fork /tree /rename /export /copy /plan /permissions /cost /compact /undo /rollback /todo /skills /memory /commands /search /approval /outcome /context /improve /exit`. Independent supported read tools run concurrently with mutation barriers and ordered results. Interrupted tool-call groups are closed with explicit results for safe resume; automatic compaction is announced. Process startup and non-cancellable external tools may still delay interruption.

## Automatic self-improvement

No `improve init`, hand-written evaluation suite, or manual training command is required. Normal sessions record trajectories and tool failures. After **5 new live turns**, a detached worker is queued; cycles run at most once every **6 hours**. On local MLX, a session that queues improvement unloads its resident models and releases the shared compute lock between turns, after its running child agents finish. The chat stays open. Its next turn reloads the model and may wait if training/evaluation owns compute. Other local sessions can still hold up the queued worker. This is automatic idle-time learning, not uninterrupted concurrent training and serving.

### What a cycle does

`/improve jobs` shows each model's worker status and log alongside the shared compute lock.
`/improve stop <state_id>` requests a safe stop for the model ID shown there. Updated training
subprocesses cancel promptly; inference/evaluation finishes its current case before stopping.
A stop also pauses future cycles until `dial improve enable` is run for that model. Workers
started with older binaries cannot acknowledge stop requests; the result explicitly reports this.
A recorded `running` state with `control_busy: false` is stale, not evidence of active training.
Compute can be held by another model, project, interactive session, or capability verification.

`/improve` shows the remaining cycle and LoRA cooldowns separately, new-turn prerequisites,
the active stage and elapsed cycle time. The learning status line updates stage progress automatically
(including self-play task and evaluation-check counts). LoRA training reports its planned iteration
count while waiting for the worker; it does not yet stream per-iteration progress or predict a finish
time. Eligibility also depends on verified training data, so an elapsed cooldown is not a training ETA.

1. **Observe and monitor.** Keep tool events, repaired calls and completed conversations. The event-feature logistic probe ranks risky steps when enough externally scored trajectories exist and its held-out AUC is informative; predicted risk remains separate from ground-truth scores. `/outcome` supplies a human verdict and `/undo` records rejection. Shell exit codes alone are telemetry, not task-success labels.
2. **Practice with external checks.** Run 24 generated file-editing, scripting, search and structured-data tasks in disposable, strictly sandboxed workspaces. Files, JSON values, answers and command results are checked deterministically. Later cycles prioritize train families with low verified pass rates, high risk scores or recent tool-error categories; gate/anchor families never enter self-play.
3. **Build training data.** Harvest successful verifier-scored trajectories plus schema-invalid tool attempts that were later corrected and executed without error. A bounded preference pair keeps the malformed assistant response as rejected and the verified correction as chosen; other raw failures are not preference labels. Conversations remain in one train/validation split, and held-out prompts are excluded. `learn_from_sessions = false` excludes future harvesting of live conversations and repairs.
4. **Train locally.** An isolated feedback-model invocation proposes harness changes. Bounded mutations explore prompts, notes, tool selection, temperature, step budgets and edit mode. For supported local MLX models, LoRA resumes from the active adapter on up to 32 new verified records plus 16 accepted replay records; validation is capped at 32. A small reference-free preference pass adds chosen SFT loss plus a 0.1-weighted pairwise logistic ranking loss on repair pairs. The base model stays frozen; rejected candidates do not advance the training watermark.
5. **Validate before activation.** Evaluate against fresh primary and general tasks, plus a frozen anchor suite. Admission requires primary improvement, enough paired wins, an exact one-sided sign test (adjusted for the cycle's candidate count), and no suite dropping over one percentage point. The anchor also limits cumulative drift from the original version. Private case contents and failed evaluator details never go to the feedback model.
6. **Serve or reject.** At most one candidate is admitted per cycle, with an atomic SQLite baseline check. Rejected candidates never become active. New turns pick up admitted versions; adapter changes restart the local server. A live monitor rolls back when enough external verdicts show a configured drop from the predecessor.

LoRA is enabled by default but requires a supported checkpoint, **24 examples** in the bounded delta/replay batch, a nonempty validation split and a **6-hour training interval**. Training defaults to **32 iterations**. A candidate must still pass the full sealed gate before activation. Data-poor cycles can still evaluate harness changes; no gain is guaranteed.

This is not online learning on every prompt: normal requests do not change model weights. The cycle
may train a separate LoRA adapter for a supported local MLX model from accumulated verified examples.
The adapter only becomes active after the external gate admits it; rejected candidates do not alter
the serving model. Hosted endpoints can improve the harness, not their remote weights.

During live work, Dial may attach up to three matching repair hints from the same project to the user message they match. They are stored with that message, never placed in the system prompt, so the cached prompt prefix does not change from turn to turn. Hints show only tool names, argument field names and failure categories—not old argument values—and never bypass current validation or approval.

The MLX-LM server adapter preserves malformed model tool calls as explicitly invalid, non-executable calls rather than allowing the pinned upstream formatter to crash on missing arguments. These errors remain visible to the agent and the learning pipeline. Its prompt (KV prefix) cache is bounded to a quarter of physical memory (`--prompt-cache-bytes`); mlx-lm's default holds up to ten caches with no byte limit, which can push weights into swap on unified memory.

```sh
dial improve jobs                   # all project models, active locks, stage and recent log
dial improve stop <state_id>        # cancel training / stop after current case; pause future cycles
dial improve status                 # scheduler state, last cycle, scores, probe and layer sensitivity
dial improve versions
dial improve disable                # pause pending/future automatic cycles; a running cycle finishes
dial improve enable
dial improve cycle                  # optional: run a complete cycle now
dial improve train                  # optional: request a gated LoRA candidate now
dial improve diagnose
dial improve harvest
dial improve propose proposal.json
dial improve rollback
dial improve outcome 42 --score 100 --source human-review
```

### Verified local-model workflows

These features improve the harness around a model; they do not imply a measured increase in model
quality merely because training ran.

- **Executable repair evidence.** In recorded live sessions, the same declared check must fail, a
  checkpointed file must change, and the check must subsequently pass. Dial retains the command,
  objective, exit results, immutable output, and before/after source bytes with hashes. Sources and
  outputs are retrievable through `artifact_read`, including after undo. Related records are retrieved
  within the same project; exact failed-state applicability is checked with a bounded hash scan.
  An unchanged flaky pass, timeout, missing snapshot, or changed command cannot establish a repair.
  Changed tests or environment can still explain a pass: this evidence never sets a trajectory success
  score or bypasses the training admission gate.
- **Measured workflow candidates.** Recurring tool-form and investigation failures propose protocol
  discipline, alternate edit-format, or bounded-investigation harness profiles. Profiles enter the
  existing held-out correctness/drift gate. Paired measurements additionally reject newly lost cases
  and hidden regressions in tool calls, protocol errors, chatter, or generation rounds. Candidate
  rationale, source counts, measurements and model identity persist with the version; existing
  activation/rollback remains authoritative. An efficiency-only gain cannot bypass correctness admission.
- **Targeted practice.** Recurring failure categories prioritize generated configuration-precedence,
  empty-search-versus-error, and behavioral-boundary exercises. Separate held-out contracts test
  transfer to null/clearing rules, search recovery and semantic boundaries. Synthetic variants use
  aggregate categories, not copied project source, user prompts or sealed gate answers.
- **Per-todo evidence packets.** An active task gets a bounded context packet containing its question,
  recorded findings, verification objective and evidence references. At task transitions, eligible old
  successful observation bodies are replaced by artifact references. All user messages, call/result
  pairing, mutation/error results and the most recent twelve messages remain intact. The session is
  append-only; packets stay stable while the task state is unchanged and never modify the system prefix.

Behavioral evidence is opt-in per configured check. Syntax/build checks remain explicitly classified
as syntax-only unless the operator identifies the command as testing behavior:

```toml
[verification]
behavioral_checks = ["regression"]

[[verification.commands]]
name = "regression"
command = "python3 -m unittest discover -s tests"
file_extensions = ["py"]
timeout_secs = 120
```

**Isolated patch search and escalation** are optional specialist tools, disabled by default. Enable
them in user/project configuration, then load `solve` or `escalate` with `tool_load`:

```toml
[solve]
enabled = true
max_candidates = 3
max_steps = 16
max_snapshot_bytes = 67108864

[escalation]
enabled = true
allow_remote = false
role = "stronger"
max_evidence_bytes = 12000

[model_roles.stronger]
provider = "mlx"
model = "/absolute/path/to/stronger-mlx-model"
```

`solve` takes `goal`, exact relative `allowed_paths`, independent `candidates` (approach strings),
and explicit `checks` (`command`, optional `timeout_secs`). It runs bounded local workers in separate
copies, then each check in another isolated copy. Workers and checks are network-disabled and follow
approval policy. A candidate that changes other files or fails a check cannot win. Among passing
candidates, the smallest changed-byte span wins; application refuses stale source or symlinks and
preserves unrelated dirty/untracked work. Checks establish only what they actually assert, not broad
semantic correctness. Source files are never reset or checked out.

`escalate` takes only `goal`, `question`, `evidence` and `rejected_hypotheses`. It sends no automatic
repository files or conversation history. The stronger role must be configured explicitly. Local MLX
and exact loopback endpoints are allowed by default; private LAN and public endpoints both require
`escalation.allow_remote = true`. Approval remains separate. Advice is marked unverified, carries
usage/cost accounting, and must be checked locally before becoming a correctness claim. Neither a
solver win nor a stronger-model answer automatically becomes a verified training outcome.

### Machine-configuration comparisons

Use the existing benchmark CLI to compare explicit configurations on identical checked tasks:

```sh
dial benchmark suite.toml --profiles machine.toml --report machine-report.json
```

Example `machine.toml`:

```toml
[machine]
repetitions = 2
case_timeout_secs = 300
minimum_correctness = 1.0
allow_remote = false

[[configurations]]
name = "q4-8k"
provider = "mlx"
model = "/absolute/path/to/mlx-model"
context_window = 8192
quantization_bits = 4

[[configurations]]
name = "q4-16k-adapter"
provider = "mlx"
model = "/absolute/path/to/mlx-model"
context_window = 16384
quantization_bits = 4
adapter = "/absolute/path/to/adapter"
```

The first configuration is the baseline. `quantization_bits` validates existing MLX model metadata;
it does not quantize weights. Adapter metadata must identify the selected base model. Endpoint
variants use explicit provider/model IDs; non-loopback providers require `machine.allow_remote=true`.
Each case has fresh state against one source snapshot, with automatic improvement, secondary model
roles, search and escalation disabled. Reports include machine identity, settings, suite hash, checked
correctness, wall time including failures/checks, observed tokens and repair rounds, and correctly
completed cases per second. Unavailable measurements stay null. Recommendations must meet the
correctness floor and per-case regression limits; reports never apply settings, activate adapters or
start training. Scripted-provider smoke results verify this plumbing, not real-model quality.


`improvement.log`, datasets, adapter artifacts and the SQLite audit history live under the project's private state directory, outside the workspace. For custom use cases, an optional `[gate]` can point to your own independent JSONL suites; cases accept `expected`/`matcher` or a declarative `check`, and optional seeded `files`. Automatic generated tasks measure a bounded coding/tool-use curriculum, not arbitrary real-world correctness. Custom gates use their explicit point thresholds rather than the automatic curriculum's significance rule.

On hosted endpoints, the loop can improve the harness but cannot train weights; automatic evaluation and feedback use that provider and may incur API charges. Set `[improve] enabled = false` to disable recording and scheduling entirely. Set `[training] enabled = false` to keep the harness loop but prohibit weight training.

Still capability-unavailable: expert growth, routing recalibration, attention retrofit and a hidden-state neural probe. The implemented event-feature risk probe and gradient-norm layer scoring are not implementations or benchmark reproductions of the named research papers.

## Configuration

Layered TOML, user then project-specific: `~/.dial/config.toml`, `~/.dial/projects/<name>-<hash>/config.toml`, then flags. Never read from the project directory.

Learning stores and adapters are additionally isolated under
`~/.dial/projects/<name>-<hash>/models/<identity>/`, so switching providers/models does not reuse an
incompatible adapter. Sessions, skills, memory and prompt history remain project-scoped.

```toml
remote_model_idle_timeout_secs = 180

[reasoning_guard]          # operator-owned; not mutated by harness improvement
enabled = true
max_pre_action_secs = 60
max_reasoning_bytes = 16384

[codebase]
enabled = true
context_max_bytes = 8000

[ui]
theme = "dark"                    # dark | light | mono | gruvbox | nord | paper | auto
status = "default"                 # default | minimal | compact | full | nerd | ascii | custom
status_left = ["spinner", "model", "path", "git"]
status_right = ["context"]
status_separator = "powerline-thin"
composer_shape = "box"             # box | line
symbols = "auto"                   # auto | unicode | nerd | ascii
show_tools = true
history_limit = 500

[model_roles.compaction]
provider = "mlx"
model = "mlx-community/Qwen2.5-0.5B-Instruct-4bit"

[model_roles.subagent]
provider = "ollama"
model = "your-model-id"
base_url = "http://127.0.0.1:11434/v1"
```

The model picker persists `provider = "mlx"` when selecting local inference, explicitly clearing an
inherited remote endpoint. Helper roles use the same rule. A project role with `provider = "inherit"`
follows the main model even if a global role assignment exists. An admitted harness may pin an edit mode;
the settings panel labels that mode as pinned instead of claiming an ineffective override.

```toml
model = "mlx-community/Qwen2.5-7B-Instruct-4bit"
approval = "ask"          # ask | auto | readonly
integrations = ["bg", "web_fetch", "web_search", "lsp", "ast_search", "ast_edit", "dap"]
context_window = "auto" # defaults to 32768 tokens; serving endpoints may cap to loaded context
[sandbox]
mode = "dev"              # dev | strict | off
allow_network = true
extra_write = ["~/.cargo", "~/.npm"]
[endpoint]                # optional: OpenAI-compatible server instead of local MLX
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LMSTUDIO_KEY"
[harness]                 # the part the improvement loop is allowed to change
system_prompt = "Work in small verified steps."
tools = ["read","edit","write","glob","grep","bash","todo","tool_search","tool_load","artifact_read","working_state"]
[tools.lint]              # operator tools: trusted programs, not sandboxed
program = "/usr/local/bin/mylint"
timeout_secs = 60
[subagents]               # custom scouts: extra instructions, selected with task agent = "security"
security = "Focus on injection, authentication and secret-handling bugs."
[web]
search_url = "https://search.example/api?q={query}"   # optional; default is DuckDuckGo HTML
search_api_key_env = "SEARCH_KEY"                    # optional bearer token variable
[lsp.rust]                                           # override or add a language server
program = "/opt/homebrew/bin/rust-analyzer"
extensions = ["rs"]
[mcp.docs]                                           # stdio MCP server (trusted program)
program = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
timeout_secs = 60

[dap.python]                 # approved local DAP adapter process
program = "/workspace/bin/python-debug-adapter"
args = []
timeout_secs = 60

[verification]
enabled = true
repair_attempts = 1

[[verification.commands]]
name = "focused-tests"
command = "cargo test --offline --lib"
file_extensions = ["rs"]
timeout_secs = 120


[improve]
enabled = true
background = true
interval_secs = 21600
min_new_turns = 5
selfplay_tasks = 24
gate_primary_cases = 30
gate_guard_cases = 20
max_candidates = 3
alpha = 0.1
min_net_wins = 2
lora_min_examples = 24
learn_from_sessions = true
monitor_min_turns = 8
monitor_drop = 15.0
drift_tolerance = 3.0

[training]
enabled = true
interval_secs = 21600
iters = 32
batch_size = 1
rank = 8
num_layers = 4
max_seq_length = 8192
```

### Lifecycle hooks, path permissions, isolated workers and schedules

**Hooks.** `[[hooks]]` in your user or project-state config (never the repository) run programs at
`user_prompt_submit`, `pre_tool_use`, `post_tool_use` and `stop`. The event arrives as JSON on stdin.
Exit 0 continues (stdout is shown; for `post_tool_use` it is appended to the tool result), exit 2 blocks
(`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message; stderr is the reason).
Any other exit, a timeout or a missing program only produces a notice. `matcher` is an anchored regex on
the tool name. Hooks are trusted and unsandboxed like `[mcp.*]`, apply to subagents too, are off in
sealed evaluations, and disable parallel read batching so every call passes `pre_tool_use`.

```toml
[[hooks]]
event = "pre_tool_use"
matcher = "write|edit"
command = ["sh", "-c", "grep -q '\\.env' && { echo 'no .env edits' >&2; exit 2; } || exit 0"]
timeout_secs = 10
```

**Path permission rules.** `/permissions deny write --path .env --path 'secrets/**'` (add `--persist` to
keep it). Globs are project-relative, a pattern without `/` matches at any depth, and matching is
case-insensitive on macOS. They apply to `read`, `write`, `edit` and `ast_edit`; hashline patch headers
are inspected, an undeterminable target is denied by deny rules and never granted by allow rules, and an
allow rule must cover every file a call touches. They do not cover `bash`, `grep` or `glob`.

**Isolated workers.** `task` accepts `isolation: "copy"` for `worker` agents. The worker edits a private
copy of the project (dirty and untracked files included; `.git`, `.dial`, secrets and build caches
excluded). Afterwards only changed files are merged back, each only if the project's file is still what the
copy started from; others are reported as conflicts, never overwritten, and the full copy is kept. Merges
go through checkpoints (`/undo` works). On remote endpoints isolated workers run concurrently. This is a
file copy, not `git worktree`, so it works with dirty trees and non-git projects.

**Schedules.** `dial schedule add "prompt" --every 30m --watch 'src/**/*.rs' [--allow read,edit | -y]`,
`list`, `remove <id>`, `log <id>` and `run [--once]` (a foreground scheduler, one per project; keep it in
tmux or launchd). Runs are ordinary `dial -p` children, read-only unless `--allow`/`-y` was given at add
time, one at a time. Watch triggers wait for edits to settle and ignore the run's own changes.

### Git tools, budgets, custom agents and HTTP MCP

**`git` tool** (an integration, on by default). Structured `status`, `diff`, `log`, `show`, `branches`
(no approval) and `add`, `unstage`, `commit`, `branch_create`, `switch`, `push`, `pr_create` (approval),
plus `pr_view`, `pr_checks`, `issue_view` through the `gh` CLI. Arguments are validated and never pass
through a shell; repository hooks, external diff drivers and textconv are disabled; push never forces.
Path permission rules apply to `git` `paths`. Default slash commands `/review`, `/commit` and `/pr` use
it; a file in `.dial/commands/` with the same name replaces them.

**Budgets.** `[budget] max_session_cost_usd` and `max_turn_cost_usd` stop a turn between model
requests. They rely on catalog pricing and reported usage, so a model without a known price is never
stopped by them (`/cost` shows whether pricing is available).

**Custom agents.** `[agents.<name>]` with `base` (`scout`, `reviewer` or `planner`), `prompt`,
optional `description`, `tools` (can only narrow the base's tools) and `max_steps`. Writers are not
configurable because approving a worker launch is decided without the configuration.

**HTTP MCP.** `[mcp.<name>]` takes `url` instead of `program` (https, or http on localhost) with
`bearer_env` naming an environment variable that holds a token, and optional `headers`. Streamable
HTTP with JSON or SSE replies and session ids is supported; there is no OAuth flow.

### Keeping improvement off your machine's critical path

Background cycles start only when the machine is free (`[improve]`):
`idle_minutes` (default 10) since your last coding turn, `require_ac_power` (default true; macOS
`pmset`), not in Low Power Mode, not thermally throttled, and under `daily_compute_minutes`
(default 120, `0` = unlimited), which is also enforced while a cycle runs: it pauses with state
`paused` and resumes when next due. `dial improve cycle` and `/improve start` bypass the start-time
checks. `dial improve status` shows `waiting_for`, `compute_used_today_secs` and
`last_cycle_seconds_by_phase` so you can see whether generation, training or evaluation holds the machine.
Evaluation stops early when a rejection is already certain (a suite over its regression bound, or a
primary suite that cannot reach the required gain even if every remaining case went the candidate's
way). This never changes whether a candidate is admitted, only how many cases run. Cloud main
models never wait for local compute; only local models and explicit local roles take the compute lock.

## Known limits

- Tool-calling quality depends on the model: 7B-class instruct models handle the tool set; sub-1B models often emit unreadable calls (which become training signal rather than being hidden).
- Local compute is serialized across projects. A session that queues improvement releases compute when idle and its child agents have finished; a subsequent turn waits if the cycle holds compute. There is no claim of zero-downtime training.
  While waiting for compute, the elapsed timer advances and commands follow the same live/deferred/interrupting policy as during inference. Turn duration includes that wait; acquiring compute returns the status to Working rather than leaving it marked as waiting.
- The TUI is keyboard-driven; not every terminal supports modified Enter keys. `dial schedule` provides scheduling but no daemon installer; there is no voice integration. Browser and kernel require available OS containment; they fail closed rather than falling back to unrestricted execution.
- Live memory/skill edits are the user's working state and are not gated; only the admitted harness is.
- Operator tools are trusted programs and are not sandboxed during live work; they are excluded from automatic evaluation. Native Windows command tools require `sandbox.mode = "off"` for live use. Linux uses bubblewrap; generated evaluations require an available strict OS sandbox.

## Source layout

```
src/agent/    agent loop, system prompt, project context, todo, typed subagents, approvals
src/tools/    Workspace + built-ins (files, search, shell, hashline, sandbox) and pluggable tools
              (ext.rs registry: bg, web, lsp, ast, mcp)
src/llm/      backends, providers, catalog, auth, local MLX server, tool-call parsing
src/learn/    self-improvement: curriculum, harvest, mutate, probe, gate, worker, verify
src/storage/  sessions, memory, skills, checkpoints, SQLite store, paths, settings
src/config/   configuration types and validation
src/ui/       terminal renderer, composer, themes, overlays
```

Adding a tool: implement `tools::ext::Tool` in its own module, return it from a `tools()` constructor, and list it in `Registry::standard`.

### Measured harness improvement and repository exercises

Dial bounds initial reasoning according to task complexity (20–60 seconds, capped by the configured
reasoning limit), redirects repeated unchanged investigations, and saves completed investigation
findings with source hashes. Investigation todo findings and drop/block reasons allow up to 4,000
characters, including relevant detail and remaining uncertainty; saved codebase notes allow up to
20,000 UTF-8 bytes.
Saved findings are untrusted context: changed sources make them stale,
and replaying an old completed todo cannot make them fresh again. Error recovery hints distinguish
missing paths, malformed calls and failed edits so retries can change approach.

Improvement records tool errors, repeated investigations, interventions, tool calls, tokens and elapsed
time alongside correctness. Task profiles may select an accepted version's known-good parent for a
particular task category only after paired evaluation shows a consistent advantage without lost
correctness. Unmeasured categories retain the active version. `/improve` and `dial improve status`
show the measurements; timing is diagnostic and does not by itself establish improved correctness.

Register real repository exercises for the selected model with an explicit JSON manifest:

```json
{
  "id": "parser-boundary",
  "prompt": "Fix the parser's boundary handling while preserving valid inputs.",
  "split": "train",
  "files": ["src/parser.py"],
  "verification_files": ["checks/parser_boundary.py"],
  "command": "python3 -B checks/parser_boundary.py"
}
```

```sh
dial improve register-task exercise.json
dial improve tasks
dial improve register-task repair.json --trajectory 123
```

Registration captures immutable, explicitly listed UTF-8 source and verification files. Include every
required dependency; checks run offline in an isolated workspace. Verification files are withheld
from the initial solver workspace and restored before verification. Use separate `evaluation`
exercises for held-out checks: duplicate content and prompts cannot cross splits. Repository
exercises supplement the automatic evaluation suites; they do not replace the primary admission
gate. Each cycle replays at most four registered training tasks. Training may make one correction
after an independent failure; evaluation receives no such repair attempt.

`--trajectory` imports a retained behavioral repair, including its original failed source snapshots.
The supplied independent checks must fail before and pass after the recorded change. The manifest
must list all changed files, including deletions; changed verification files are rejected. A repair
record or a successful tool call alone never becomes a successful training label. Syntax-only checks
remain distinct from behavioral evidence.

The optional `solve` and `escalate` tools use the configuration documented under **Isolated patch
search and escalation** above. Candidate search verifies independent patches in isolated workspaces;
escalation supplies compact evidence to an explicitly configured stronger model. Both remain disabled
until configured, and stronger-model advice still requires local verification.

Use `/improve jobs` to distinguish the selected model's idle state from another model's active
compute job, and `/improve stop <state_id>` to request cancellation. LoRA support verification,
training, evaluation, activation and confirmed adapter use are separate states. An available or
verified adapter capability does not mean training has run or that inference used an adapter.

### Inspect Dial's saved understanding

Use `/understanding` to inspect saved project findings and source outlines, or
`/understanding routing` to filter by a topic, path or symbol. Entries show their source references
and freshness: **fresh**, **stale**, or **unchecked** when the freshness read budget is exhausted.
Findings include their originating session and revision. Stale summaries remain visible for auditing,
clearly marked as historical; they are not restored to the agent's current context.

The interactive view shows up to 40 matching entries and reports truncation. To see or export every
saved entry without loading a model:

```sh
dial understanding
dial understanding routing
dial understanding --json > understanding.json
```

This inspects existing saved knowledge without refreshing the index or generating new claims.
Coverage is limited to supported, permitted indexed files; it does not imply the model knows every
file or that agent findings have been independently verified. `/todo` shows current session tasks,
`/memory` shows other project memory, and `/context` shows context usage.

`/understanding architecture` shows saved architectural findings, design documents and manifests,
indexed directories, and observed import/module declarations with source locations and freshness.
The view identifies missing architectural knowledge explicitly. Directory groups do not establish
responsibilities, and imports are not a resolved dependency graph. No extra model inference is used.
The same view is available as `dial understanding architecture`, with `--json` for all records.

### Architecture-guided coding

Laya decision advice is available with `/laya on`, or `[laya] enabled = true`. It uses the
Dial-owned Laya service's native typed prediction API for retrieval scope and post-edit evidence
review. `/laya status` shows the latest session decision; `/laya check` probes the live checkpoint
and device. Advice and raw probabilities never certify behavior or replace deterministic checks.
The Architecture model role writes architecture summaries. Settings → Model → Project understanding → **Architecture model** defaults to local `mlx-community/Qwen2.5-3B-Instruct-4bit`, a compact summarization model. It requires the MLX runtime and model weights; unavailable local inference defers maintenance rather than sending it to the coding provider. Clear this setting to follow the main coding model. An explicit Architecture assignment in the model picker (Alt+M) takes precedence; clearing that role also clears the local fallback. Laya supplies typed decisions rather than summaries.

Install the independent runtime with `dial laya install --checkpoint /path/to/checkpoint`.
Dial copies the checkpoint assets into `$DIAL_HOME/laya/model` (default `~/.dial/laya/model`),
creates its own Python environment, and bundles its own server. No other coding-agent installation
is required. Reinstallation without `--checkpoint` updates the runtime and preserves the saved model.

With `laya.auto_start = true` (default), a refused connection starts Dial's server using its private
runtime. Default port 8002 is separate from other applications. Health responses must identify the
service as `dial-laya` with the supported protocol; foreign services are rejected rather than reused.
Cold startup waits only the configured request interval before coding continues with a **loading**
notice. `/laya start` explicitly starts/waits for up to 30 seconds, with responsive cancellation.
`dial laya start` and `dial laya status` also work from a terminal. The managed server logs to
`~/.dial/laya/server.log` and exits after 15 minutes without prediction requests. Disable advice
with `/laya off`; existing managed requests finish and the service can shut down when idle.

With `[codebase].synthesize_architecture = true` (the default), a successful turn can maintain a
persistent architecture synthesis using the Architecture model role. Maintenance response text, reasoning, and tool argument streams stay out of the transcript; progress and completion notices remain visible. The task's first action does
not wait for this maintenance call. One call per indexed source state is attempted, with a 60-second
deadline, bounded source excerpts and output, no tools, and visible cancellation. An invalid response
leaves the previous record intact; `/understanding build` explicitly retries. Sealed evaluations do
not load or synthesize project memory.
Queued foreground work takes priority: automatic maintenance skips or cancels without consuming the
queued message, and an interrupted maintenance attempt may be retried on a later turn.

The record explains project purpose, component responsibilities and state ownership, supported
execution paths, constraints, and validation evidence. Every section must cite sources actually
supplied to the model and still unchanged when saved. It records partial coverage and unknowns;
source citations establish provenance, not semantic correctness. Fresh sections survive incremental
updates, while changed or excluded supporting sources make affected sections unavailable for reuse.
Unchanged projects reuse the saved record across sessions without another synthesis call.

Freshness also covers dependency scopes. Component observations watch their cited module, lexical
consumer candidates, and project documents/configuration; broader architectural observations and
saved investigation findings watch the supported project sources. File additions/removals/renames,
branch identity, manifests, lockfiles and toolchain configuration invalidate saved assumptions.
Checks are bounded (2048 supported files, 16 MiB, three seconds per snapshot); incomplete scopes are
**unchecked**, never silently current. Old records without dependency provenance require rebuilding.
Before the first edit to a path, outdated saved assumptions cause an investigation checkpoint: inspect
current sources and dependencies before retrying. This is a review prompt, not a semantic proof.

`codebase_memory` action `invalidate` records a contradiction using `section_ids` for architecture
or `topic` for a finding, plus `reason`. Contradicted prose is withheld until explicitly replaced;
concurrent synthesis cannot erase a newly reported contradiction. The architecture view distinguishes
source status from behavioral status. The latest configured behavioral check records command digest,
source/environment fingerprints and time; it becomes stale when its provenance changes. A failure
requires review, and a pass does not certify an architectural claim. Runtime services, environment
variables and dynamically resolved dependencies still need task-specific checks; lexical consumers
do not establish a complete call graph.

`/understanding architecture` leads with this synthesis. Raw imports are labeled unresolved references.
Each coding turn receives a bounded purpose statement and task-relevant architecture sections.
After edits, the harness supplies affected-area review guidance and lexical consumer/test candidates;
the agent uses configured `lsp` definitions/references to resolve symbols where available. Existing
language-server startup approvals still apply. No architecture-generated shell command runs automatically.

Before editing/debugging tasks, up to eight operator-configured `behavioral_checks` run to record the
pre-edit baseline, subject to normal tool approval. Relevant configured checks run again after edits.
This distinguishes pre-existing failures from regressions and supplies failure/change/pass repair
evidence without labeling the whole task successful. Syntax checks remain separate. Changed-file
review prompts cover callers, interfaces, configuration/serialization, state ownership, and reuse.

An unchanged investigation observation now consumes the remaining unplanned inspection allowance,
triggering a concrete investigation todo when needed. The existing repeated-evidence stop still applies.
Workflow records separately count architecture output tokens, failed verification checks, repair
rounds, and change-review prompts. Compare actual task correctness and complete-run cost with the
existing repository benchmarks before attributing performance gains to these features.

### Coding assistance without local training

Dial reuses completed work through project-scoped experience, separate from model weights and
self-improvement admission. `/experience` shows saved recipes and source-scoped failures;
`/experience ledger` shows this session's observations and hypotheses. `dial experience --json`
exports saved project records without starting a model. `/understanding architecture` remains the
view of the overall architecture.

The investigation ledger stores questions, source hashes and output digests, and survives session
resume. Unchanged evidence supplies a reminder before further investigation; changed source stops
that evidence from being reused. Hypotheses remain agent interpretations linked to an observation,
not independently verified facts. Actual file contents remain available through the ordinary read tool.

Successful file edits can become executable recipes only after configured behavioral checks pass
following the last edit. Arbitrary shell changes, missing source preconditions, failed checks and
oversized or sensitive records prevent capture. Replaying `experience(action="run", id=...)`
rechecks each step's source hashes, requires the checks still be configured as behavioral, and runs
through normal tool approvals, hooks, sandbox and checkpoints. A stale recipe remains advisory;
it cannot overwrite changed source. Failed replay retains edits for diagnosis, rather than undoing
unrelated work. These observations do not certify a model, provide training rewards or admit adapters.

Local models receive a smaller initial tool set, with core tools and explicitly loaded tools retained.
Observed tool outcomes rank optional tools per model; `tool_search` and `tool_load` retain access to
allowed specialist tools. Compact descriptions preserve parameter schemas and the complete edit
contract. Recalled recipes accumulate display, source-overlap reuse and subsequent configured-check
counts. These are observed-use proxies, not causal accuracy estimates.

The syntax guard parses proposed complete source before write, replace/hashline edit and AST commits.
It rejects newly broken syntax, allows repairs that reduce existing errors, and leaves unsupported
languages to existing checks. `edit_preview` parses proposed source without changing files; its
optional `check="type"` uses an offline Cargo check in a disposable project copy. That copy is limited
to 2,048 files, 32 MiB, a three-second preparation budget and a 30-second compiler timeout. Compiler
previews may execute build scripts and require normal approval. Rust/Cargo is currently the supported
type-preview backend; syntax parsing supports the installed tree-sitter languages. Preview success
is not verification of the live workspace. Directly matched Python tests and Rust module tests
supplement, rather than replace, configured verification. Empty suites do not earn recipe verification
credit. This does not implement token-level compiler steering during generation.

An optional `experience` model role can annotate a verified workflow with a short lesson. It requires
both an assigned hosted role and explicit enablement, makes at most one bounded call per eligible
completed turn, and yields when queued coding arrives. It receives bounded workflow metadata, never
creates executable steps or checks, and never loads another local model. Local teacher roles are
skipped to preserve coding compute. No teacher call occurs by default.

All controls are available in `/settings` under Learning / Coding assistance, Tools / Verification,
and Models / Context. Assign the teacher in `Alt+M → Roles → experience`.

```toml
[assistance]
enabled = true
syntax_guard = true
selective_verification = true
compact_tools = true
initial_tool_limit = 16
context_max_bytes = 4096
teacher_enabled = false
teacher_timeout_secs = 10
```

### Experimental MLX activation steering

Dial can extract a unit-length behavior vector from matched positive/negative assistant responses,
then add it to one language-decoder transformer layer during inference. This runs in Dial's own
MLX-LM text server and MLX-VLM vision server, including image requests and cached decoding.
The vision encoder stays unchanged. Vision-model extraction and evaluation use the same decoder
weights and model tokenizer for text/tool contracts; these checks do not certify image accuracy.
Hosted providers and external inference servers have no Dial activation hook. Incompatible decoder
architectures bypass the vector and report the reason through `/steering`. Automatic creation is enabled
by default as part of automatic improvement; no vector is used before it passes evaluation.
No random vector is activated automatically, and this is independent of OMP and Laya.

Automatic creation reads only train-split preference pairs matching recorded successful tool repairs.
It waits for at least eight distinct, bounded contrast prompts, extracts at the model's middle layer,
and tests eight separate exact tool-call contracts covering read, write, grep and glob. Candidates
must improve over the unsteered model, pass the existing repetition and latency gates, and avoid
reducing the incumbent vector's held-out score. Failed candidates preserve the active vector.
These are response-contract checks, not proof of overall coding improvement. Manual vectors are
preserved. Unchanged completed attempts have a 24-hour cooldown; new evidence can trigger a new attempt.

`/settings` → Model → Activation steering → **Automatic vector creation** controls both automatic
creation and use. **Enable manual vector** controls explicit vectors independently. `/steering`
shows collecting `N/8` prompts, candidate extraction/evaluation progress, rejection/admission and
actual runtime use. Jobs share the improvement cycle's compute lease, budget, stop controls and
foreground handoff. Auxiliary roles and sealed harness evaluation remain unsteered.

```sh
dial steering status
dial steering extract --pairs pairs.json --layer 14
dial steering evaluate --cases held-out.json
```

Use `-m MODEL` before `steering` to select the model. Choose a zero-based layer that actually exists;
Dial probes the model's layer count and hidden dimension. Extraction requires 8–64 distinct matched
prompts. The pairs file is a JSON array with this shape for each entry:

```json
{
  "positive": [{"role":"user","content":"Implement a typed addition function."},
               {"role":"assistant","content":"def add(a: int, b: int) -> int:\n    return a + b"}],
  "negative": [{"role":"user","content":"Implement a typed addition function."},
               {"role":"assistant","content":"def add(a, b):\n    return a + b"}]
}
```

Both sides must have identical preceding messages and different final assistant responses. Pick
examples that isolate the behavior you want. The held-out file is a separate JSON array of 8–32
cases, with distinct prompts that did not appear in extraction:

```json
{
  "messages": [{"role":"user","content":"Write a typed integer multiply function."}],
  "required": ["def multiply(", "-> int"],
  "forbidden": ["TODO"],
  "python_ast": true
}
```

Each case needs required text or an `expected_tool`. Tool cases can additionally supply OpenAI
`tools` declarations and `required_arguments`; checks require complete JSON, the expected tool
name and presence of those arguments. These are response contracts, not comprehensive tool-schema
or coding correctness checks. Generation is deterministic and capped at 128 tokens per case.

Evaluation compares baseline and steered responses in alternating order. Admission requires a
strictly better contract pass count, at least 80% passing, no tool-validity/repetition regression,
and bounded first-token latency regression. A passing result is **not full coding accuracy
certification**; inspect real coding behavior before relying on a vector. Admission is tied to
this artifact and exact strength. Model/adapter metadata changes invalidate the binding; the
fingerprint uses paths, file sizes/timestamps and selected config content, not a full hash of every
weight byte. If using LoRA, pass the same `--adapter /absolute/path` to both maintenance commands;
a base-model vector is bypassed when an adapter is loaded.

For a manually extracted vector, after evaluation open `/settings` → Model → Activation steering
to enable the manual vector and select
the vector name and strength. `/steering` reports the evaluation, completed/total job progress,
and live `loaded`, `used` (with forward-pass count), `bypassed` or `unconfirmed` state. CLI status
reads saved state without starting inference and cannot confirm another session's runtime use.
Changing the profile restarts the inference server and clears its prompt cache. A missing,
unadmitted or incompatible vector is bypassed so coding stays available. Auxiliary model roles
and sealed harness evaluations do not use steering.

Extraction/evaluation are explicit background jobs. They wait for idle compute, obey the existing
`/compute` global pause/resume controls, model stop flags and daily compute budget, and yield to foreground coding.
A checkpoint is saved after each extraction pair or evaluation case; interrupted work repeats only
the unfinished unit. `Ctrl+C` stops the command and its worker; rerun the same command to resume.
After an operator stop, select Start in `/compute` before resuming. Changing the dataset or model
binding starts a fresh checkpoint. Progress and private JSON vectors live under the model's
learning directory in `steering/`; no weights are changed. No checkpoint compatibility is promised
across steering format versions.

### Live task and phase policy

Dial now selects request controls between model steps, without a classifier model call, training,
or a compute lease of its own. `/policy` shows the last effective request settings, the reason for
selection and bounded recent request metrics; it also works while the agent is busy. Open
`/settings` → Model → Adaptive policy for controls. CLI inspection and previews load no model:

```sh
dial policy
dial policy "Investigate why the build fails" --json
```

A CLI preview uses the configured baseline; an active session may use a different evaluated harness
version. The live session report is the record of what that session requested. Capability-unverified
sampling is labelled unverified, rather than claiming the provider applied a temperature.

Initial phase comes from the task. Observed source reads select investigation, successful edits
select editing, recognized checks select verification, and errors or reasoning interceptions select
recovery. Design/alternatives requests start in exploration. Control calls do not clear recovery.
A recognized check is an observed execution event, not proof that the whole task is correct.
Later reads after an edit remain focused on implementation. New turns reset phase detection.
Pin a phase to override this heuristic. Settings saved during a running turn apply after that turn;
phase selection itself changes at each request boundary.

By default, phase-specific pre-action time and output limits are enabled; **phase sampling is off**.
Explicit model-profile output/temperature/top-p/thinking/effort overrides win. Reasoning-model output
budgets are preserved, and supported unpinned reasoning effort can adapt only within the exact
model's declared capability list. Existing reasoning and retry guards remain authoritative.
Truncated output retries escape the phase token cap; partial tool calls still cannot execute.
Editing keeps the baseline output budget. Task evidence packet size can shrink for verification or
recovery, while cached source messages and tool permissions remain unchanged.

Enable Experimental phase sampling to try configurable temperature presets and top-p choices:

| Phase | Default temperature | Default top-p | Output limit |
|---|---:|---:|---|
| Investigation | 0.20 | 0.95 | min(baseline, 2048) |
| Editing | 0.15 | 0.90 | baseline |
| Verification | 0.05 | 0.80 | min(baseline, 1536) |
| Recovery | 0.10 | 0.90 | min(baseline, 2048); expanded during length recovery |
| Exploration | 0.50 | 1.00 | min(baseline, 4096) |

These are experimental operator-enabled presets, not measured optimal values. Unknown endpoint
capabilities keep their previous request behavior; MLX uses Dial's supported request controls.
Pin baseline sampling to preserve the baseline temperature/top-p regardless of phase. Explicit
model-profile sampler values remain pinned individually even when experimental sampling is enabled.
A one-request profile is consumed at the request boundary, including failed requests; retries keep
the same profile, and auxiliary calls cannot inherit it. No server restart or cache flush is needed
for these request controls. Activation steering remains a separate fixed server profile.

```toml
[adaptive]
enabled = true
sampling = false
reasoning = true
output_limits = true
pin_sampling = false
phase = "auto"
editing_temperature = 0.15
investigation_tokens = 2048
verification_tokens = 1536
```

Policy history stores settings and observed execution/token/latency metrics, not new training labels
or permission grants. It never promotes sampler settings from self-reported success. Existing task
routes between evaluated harness versions remain scoped to their admission records. Auxiliary roles
and sealed harness evaluations retain their existing request controls. To measure sampler changes,
compare pinned model-profile configurations on held-out coding tasks and check correctness, tool
validity, retries and latency; this release does not automatically tune and admit phase presets.

### Tool reliability and measured interfaces

Dial builds execution validation from the same tool-schema snapshot sent with each model request.
The system catalog is historical configuration; current schemas are authoritative. Current-call
names, valid argument examples and single-call restrictions appear in the offered definitions,
without inserting transient messages into the conversation or rewriting its cached prefix.
Investigation checkpoints still require real todos; tools withheld by a checkpoint are explained
as temporarily gated, and invalid todo calls receive a valid argument shape.

```sh
dial reliability status
dial reliability check
dial -m mlx/mlx-community/Qwen2.5-0.5B-Instruct-4bit reliability check --json
```

The explicit check makes up to eight model requests, each with a 60-second wall-clock limit, through the selected backend: read, grep,
glob, write, todo, edit, a fixture assertion through bash, and a two-call batch. Model output is
executed only after matching the exact expected tool and arguments, inside disposable fixtures.
It never edits project sources. The edit fixture uses the configured hashline or replace contract.
A profile that explicitly disables parallel calls skips the batch case rather than scoring it as
a model failure. Transport or fixture infrastructure failures preserve the previous profile rather than counting
as model failures. Local checks wait for idle compute, yield to foreground coding and obey stop
controls; Ctrl+C interrupts calls and compute waits. No training is required. Hosted checks use
the selected service and can incur its normal inference charges.

Results are private, model-scoped `reliability.json` records. Binding includes the selected backend,
model, model profile, edit format, server helper, available local path metadata and Hugging Face
main revision pointer. Results expire after seven days. This is metadata invalidation, not a full
hash of all weights; recheck after changing an external server's model behind an unchanged ID.
These fixtures measure tool contracts and operations, not overall coding accuracy. Automatic
interface selection excludes sessions with LoRA or steering because calibration currently tests
the base model. Failed fixture cases select one-call operation; two consecutive live contract
failures also select it for recovery. Invalid multi-call batches in this mode execute nothing.
Three consecutive contract-failure rounds stop by default. Normal runtime errors retain the
existing path/patch recovery, identical-call guards and stall limits.

Use `/reliability` during a turn to inspect successful calls, schema and runtime failures,
corrected rounds, check outcomes, time to the first useful action and the selected interface.
The panel reads a cached session snapshot; it performs no inference, Git or database work on the
render thread. **Settings → Model → Tool reliability** controls the gate, examples, automatic
interface selection and failed-round limit. CLI status shows saved base-model results, not another
session's live observations.

For providers with documented strict tool support, explicitly declare the exact model profile:

```toml
[model_profiles.my_model.capabilities]
strict_tools = true
parallel_tool_calls = false
```

This extends an existing exact provider/model profile; do not infer support from a model's name.
Compatible object schemas receive native strict flags in Chat Completions, Responses and Anthropic
requests. Optional fields become nullable on the wire and their null placeholders are removed
before validating against the original execution schema. Required nulls, unknown properties and
invalid values remain errors. Unsupported schema compositions retain ordinary validation.
Dial's pinned native MLX server has no constrained tool decoder; strict declarations for that
backend are rejected rather than presented as working constraints. Runtime validation is always
active. Strict schemas constrain structure, not whether a path, edit or solution is correct.
See [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling)
and [Anthropic strict tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/strict-tool-use).

`finish completed` checks pending edits with existing focused verification before accepting the
claim, and refuses unresolved failed checks. A pending diff or visual review continues the agent
normally; it is not a tool failure or an accepted completion. Visual edits require fresh rendered
pixels in a subsequent model round. If inspection is unavailable, the agent reports that limit;
the visual reminder is bounded for each revision. Operator-declared behavioral checks and existing
bounded repair remain in force. `/reliability` distinguishes missing automatic checks from passing
checks; build or syntax success is not a certificate of task correctness. Inspect the diff and
use behavioral checks that cover the requested outcome.
