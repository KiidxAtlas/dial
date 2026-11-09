# Shell tool

The `bash` tool runs a command with the shell chosen by `shell.backend` (default `auto`): `/bin/bash -c` on Unix when it exists, Git for Windows' `bash.exe` on Windows when it is installed, otherwise Dial's built-in shell (on Windows without the built-in shell, PowerShell). The tool runs from the workspace root. Supply `cwd` to select an existing directory inside the workspace. A simple leading `cd PATH &&` is extracted into the working directory only when PATH resolves to a directory inside the workspace; shell expansions and compound commands are left to the shell. Any other `cd` is not extracted and runs inside the shell, where the sandbox (if enabled) still applies.

By default, commands run with stdin at EOF and stdout/stderr captured, without allocating an interactive terminal. Native foreground, named-session and managed shell routes report controller-owned launch context in `execution.shell_launch`, mirrored in `shell_launch` result data and a short factual note. This identifies stdin (`eof`, `pipe` or `terminal`), captured output, terminal allocation and the managed job name when known. Job observations retain the launch context; pre-start refusals and non-shell eval cells do not invent it. These facts describe Dial's launch, not redirections or terminals a descendant may create, and do not establish that the host lacks interactive capabilities. A zero exit code is process status, not proof that the requested application behavior worked; an application can print an error or skip its work and still exit zero.

Commands are placed in a dedicated process group. The tool combines stdout and stderr, streams output while running, and bounds retained output while preserving diagnostic lines. The configured timeout defaults to 300 seconds; a positive `timeout` argument requests a shorter duration, capped by the configured limit. A timeout of zero in configuration disables the deadline. Long-lived services should be started with the managed background tool rather than detached from foreground Bash.

The sandbox may deny network access or writes outside the project. Use the command's `$TMPDIR` for temporary files. Do not retry a denied command unchanged; adapt it to the sandbox or explain the limitation.

System Bash enriches missing-command diagnostics for known Python, pip and Node alternatives using
builtin lookup in the failing command's current PATH/cwd; it never runs a candidate to probe it.
Availability is not compatibility. Existing operator missing-command handlers and ERR traps are
preserved. Bash 3 uses a conservative simple-command fallback, so conditional failures and complex
syntax can omit the hint. Embedded, in-process and PowerShell backends do not invent host-PATH
evidence for their results. Recovery guidance uses supplied availability instead of probing again;
when no evidence is present, alternatives should be checked together once after correcting the cause.

Dev-policy and unsandboxed child environments use an explicit runtime/toolchain allowlist,
rather than guessing secrets from variable names. Unknown inherited variables and credential-
bearing URL values are excluded. MCP operator-configured `env` entries are applied separately
as explicit grants; do not place credentials there unless the server is trusted to receive them.
This environment filtering does not turn an approved unsandboxed command into a sandbox.

## Interactive programs

For a terminal application (curses, a TUI, a REPL or an interactive CLI), call `bash` with `{"command":"<actual command>","pty":true}`. This allocates a terminal at launch through the existing managed `bg` runner. Without explicit readiness, startup observes the existing job for up to one second after spawn (ending sooner on observed exit), then returns its rendered screen and structured `job` data (`name`, `pid`, `terminal`) with process/capture facts. Exit-reader settlement can add up to 300 ms, plus spawn and scheduling overhead. An observed early nonzero or signal exit is an error, not a successful launch receipt. Use the returned name with `bg` `output`, `input`, `wait`, `resize` and `kill`; discover/load `bg` first if it is deferred. For example, `{"action":"input","name":"<returned name>","text":"help\n"}` sends input, and `{"action":"wait","name":"<returned name>","timeout":5}` waits up to five seconds without killing the job.

PTY mode is opt-in (`pty` omitted or false retains the pipe behavior). It always returns a managed job, regardless of `background`, `async` or the automatic-background threshold. Explicit `timeout` and named `session` are rejected before launch in this mode: use `bg wait` for bounded observation and `bg kill` to stop the job. There is no implicit process deadline for a PTY job. A running pipe process is never converted or rerun automatically in a terminal.

For an explicit job name or initial terminal dimensions, use `bg` directly: `{"action":"start","name":"app","command":"<actual command>","pty":true,"rows":30,"cols":100}`. `bash -i` changes shell mode but does not allocate a terminal. A failed pipe launch is not a reason to change application code to bypass its interactive path or hand the task back untested.

If a completed command reports a terminal requirement after a nonterminal launch, shared parent/worker recovery distinguishes that diagnostic from the observed launch facts and offers an eligible terminal-backed route. Closed-input diagnostics instead point to `bg` with `stdin: true` when available; needing input does not necessarily mean needing a terminal. Deferred tools must be loaded before use. Disabled, excluded or unsupported routes remain boundaries, not reasons to install tools, change application code or try a wrapper. Advice is not permission and does not replay the command: inspect possible effects and retain its target, command and working directory. Named-session state cannot be migrated into a fresh PTY job.

A running job and its initial screen do not establish readiness or prove the application worked. Observe that existing job with `bg output` or `bg wait` rather than starting a duplicate. A successful observation call does not mean its child exited successfully; exit status, running state, readiness and behavior verification remain separate. Startup observation sends no input, replays no command and is interruptible. Explicit `ready` requests retain their own wait semantics; ordinary pipe starts remain asynchronous. Unknown effects, interrupted execution and incomplete capture retain their existing recovery restrictions. Printed diagnostic text never changes execution status or grants a retry.

Tool allowlists, `shell.no_pty`, sandbox policy, platform support and approvals still apply. PTY launches do not receive the read-only inspection exemption: a terminal can enable pagers or interactive actions, so even an inspection command follows ordinary approval policy and is refused in read-only mode. Discovering a tool does not grant permission to execute it; a refusal is a boundary, not a reason to use a shell wrapper to evade it. Session-owned jobs stop when the Dial session ends.

## Shell backends

Set `shell.backend` in Settings (Tools tab) or in `config.toml`:

- `auto` (default): bash when one is found (`/bin/bash` on Unix; on Windows, Git for Windows' `bash.exe` from `%ProgramFiles%\Git\bin`, `%ProgramFiles(x86)%\Git\bin`, `%LOCALAPPDATA%\Programs\Git\bin`, or PATH). A Git installation is recognized by its `cmd\git.exe` beside `bin\bash.exe`, so WSL's `bash.exe` launcher is never used. Without bash: the built-in shell. On Windows without the built-in shell (a build without `embedded-shell`): PowerShell.
- `system`: bash only. On Windows this means Git Bash; it fails with an actionable message when no bash is found.
- `embedded`: Dial's built-in shell. It needs no system bash, so it works on minimal systems and containers.
- `inprocess`: the same built-in shell, run on a thread inside the Dial process instead of a child process. It is allowed **only when `sandbox.mode = "off"`**: with `dev` or `strict` (or any other mode) the `bash` tool refuses with an error and runs nothing; it never falls back to another shell. A build without `embedded-shell` reports a clear error. See "In-process shell" below.
- `powershell`: Windows only. Runs commands in Windows PowerShell. Elsewhere it fails with a clear message.

The active shell is shown in the start-up banner (the `shell` row), for example `bash (C:\Program Files\Git\bin\bash.exe)`, `built-in shell`, or `PowerShell`.

The built-in shell runs as a child process of `dial` itself (no extra install). It is included in the default build; a build without the `embedded-shell` feature reports a clear error if `embedded` is selected. Its builtins and utilities run inside that host child process, not as separate programs, so on macOS and Linux they inherit the OS sandbox applied to the process: the sandbox grants the running `dial` binary read and execute access, and every builtin or utility runs under the same sandbox rules as the system shell and the commands it starts.

The built-in shell provides the POSIX shell language (pipelines, redirections, `&&`/`||`, variables, `$(( ))`, globs, functions, `case`/`if`/`for`/`while`) with the standard shell builtins (`cd`, `echo`, `printf`, `test`/`[`, `export`, `read`, and so on). Dial also ships these commands so scripts run without GNU or BSD coreutils:

- files: `basename`, `cat`, `cp`, `dirname`, `find`, `head`, `ls`, `mkdir`, `mv`, `rm`, `tail`, `touch`, `wc`
- text: `cut`, `env`, `grep`, `sed`, `sort`, `tr`, `uniq`, `xargs`

Any other command is looked up on `PATH`. Run `<command> --help` for the supported options of a built-in utility.

Limits of the built-in shell:

- `sed` supports only the `s/RE/REPLACEMENT/[g]` substitution, with `-n` and `-e`. Other sed commands need the system shell.
- Patterns in `grep` and `sed` use Rust regular-expression syntax, not the full POSIX basic/extended grammar.
- `xargs` runs external programs only. A name that exists only as a shell builtin is not found and exits 127.
- On Windows, bash, PowerShell, and the built-in shell all run only with the explicit unsandboxed command setting, because native isolation is unavailable there.

## Per-session Cargo build output

Commands Dial runs (`bash`, the built-in shell and background shell jobs) set `CARGO_TARGET_DIR` to a directory unique to the session: `<workspace>/.tmp/cargo-target/<session id>`. Concurrent sessions therefore stop contending for one `target/` build lock. The directory is fixed per session, not per command, so builds reuse their cache. It lives in the workspace scratch directory, which the sandbox already allows Dial to write.

- Each command Dial runs refreshes a `.dial-last-used` marker in an existing session directory. A new session that has no directory yet takes over (renames) the most recently used directory of another session idle for at least 30 minutes, so its first build is incremental rather than cold. When a session starts, other sessions' directories unused for seven days are deleted in the background, so finished sessions do not keep gigabytes of build output indefinitely. Live workspaces and their clones retain a shared session lease; adoption/pruning requires exclusive ownership through the rename/removal. Existing native and cross-target profile `.cargo-lock` files are locked through maintenance too; unreadable or ambiguous layouts are skipped. Lease acquisition errors refuse workspace setup rather than exporting an unprotected target. Resuming a session keeps its directory and its warm cache.

- An explicit `CARGO_TARGET_DIR` in Dial's inherited environment or named-shell state is kept as is.
- A `--target-dir` flag on a Cargo command still wins, because Cargo applies it after the environment.
- Copied workers (`isolation = "copy"`) run in their own root, so they get their own directory under that copy.
- The setting applies only to commands Dial starts. It does not change your global environment or shell profile.

## In-process shell

`shell.backend = "inprocess"` runs foreground `bash` commands without a host child process, so there is no isolation between the shell's builtins and Dial. That is why it needs `sandbox.mode = "off"` and fails closed otherwise. Working directory, the secret-scrubbed environment (`is_secret_env`), `TMPDIR`, output capture limits, artifacts, the `timeout` argument and cancellation behave as in the other backends. The banner shows `in-process shell (sandbox off only)`.

Differences and limits:

- Timeout and interrupt abort the script at its next `await` (every external command, pipe or `sleep`) and attempt to kill the process groups of external commands the run started (Unix; found with `ps` and a per-run `DIAL_INPROCESS_RUN` marker in the command environment). Discovery and cleanup share a 500 ms deadline and 1 MiB output budget, in addition to bounded executor kill/reap overhead and the shell's separate 500 ms shutdown grace. A command that calls `setsid` or clears its environment escapes the kill. Discovery/cleanup failure is reported as uncertain effects rather than proof that all processes stopped; repeated termination and Drop reuse the settled cleanup result. External cleanup is unsupported on Windows and is reported as uncertain there.
- A builtin loop that never yields (for example `while :; do :; done`) cannot be interrupted from outside its thread. Dial reports the timeout, abandons the thread and notes it in the output with unknown effects; the thread keeps using CPU until Dial exits. Shell-thread completion and external-process cleanup are independent facts. Use `system` or `embedded` when a hard kill matters.
- In-process execution has no automatic background handoff. Explicit `async`/`background` requests use the child-process built-in shell instead, where the automatic-background threshold applies regardless of `timeout` (a shorter timeout still kills the command). Named `session`s also use the child-process shell, subject to their restrictions below.

## Named embedded sessions

With `shell.backend = "embedded"`, a `bash` call may supply `session` to retain its
working directory and exported string variables for later calls in this Dial workspace. Named
sessions reject overlapping commands, including while a command is owned by a background job.
Explicit and automatic background handoff retain the session claim until the job settles.
Normal completion publishes state even for a nonzero exit code; cancellation, timeout, a signal
or an incomplete state exchange invalidates it. Functions, unexported variables and process-restart
persistence are not retained. Protected host variables and credentials are excluded, and each
command keeps the ordinary sandbox policy. Saved exports contain only changes from the host's
inherited environment, so unchanged runner variables do not consume the 128-variable, 256 KiB
state budget. Previously retained session changes remain available on later commands.

## Output minimizer

With `shell.minimize_output = true` (default), foreground `bash` output of a recognised noisy command is shrunk before the model reads it. A filter applies when the command is one simple command (optionally with leading `NAME=value` assignments or `2>&1`), or a sequence (`a && b`, `a; b`) whose segments are `cd` or recognised commands that all select the same filter, such as `cd web && npm install` or `cargo check && cargo test`. Mixed sequences, pipelines, redirections and substitutions are never filtered.

| Command | Dropped | Collapsed |
|---|---|---|
| `cargo build/check/clippy/test/run/doc/bench/nextest/install/fetch` | `Compiling`, `Checking`, `Fresh`, `Documenting`, `Downloaded`, index updates | runs of `test ... ok` / nextest `PASS` |
| `git status` | `(use "git ..." ...)` hints, `hint:` lines | |
| `git push/pull/fetch/clone` | object-transfer progress, `hint:` lines | |
| `git commit/checkout/switch/merge/rebase/...` | `hint:` lines | |
| `npm`/`pnpm` install-like, `yarn` install | notices, funding and audit lines, progress | |
| `pytest`, `python -m pytest` | session header, progress dots | `PASSED` lines |
| `go test` | `=== RUN` / `PAUSE` / `CONT` | `--- PASS`, `ok pkg` |
| `pip install`, `python -m pip install`, `uv pip install`, `uv sync`, `uv add` | `Collecting`, `Downloading`, `Using cached`, `Requirement already satisfied`, wheel-build and progress-bar lines | |
| `jest`, `vitest`, `npx`/`bunx jest`/`vitest` | | `PASS` / `✓` lines |

For every command, ANSI escape sequences are stripped and carriage-return progress redraws are reduced to their last frame.

Guarantees: a line mentioning error, warning, panic, fail, traceback, exception or fatal is never dropped or collapsed by the minimizer (a frame overwritten by a carriage return that carries one is kept on its own line), with one exception: a test run (`cargo test`/`nextest`, `pytest`, `jest`/`vitest`, `go test`) whose only such lines are passing runner summaries is replaced by one line, for example `test result: ok. 412 passed; 0 failed; 3 ignored (9 suites; passing run summarized by the output filter)`. Any warning, failure, panic or error keeps the line-by-line filter. The exit status and the `[exit code: N]` line are untouched; the result is used only if it saves at least 256 bytes and a tenth of the output. The text gains a line `[output filtered (cargo): 41230 -> 3810 bytes; ...]` and the `[complete output: artifact://...]` link still holds the raw output, readable with `read`. The foreground shell's inline preview keeps 4 KiB from the beginning and 4 KiB from the end, plus bounded middle diagnostics and explicit omission counts. Complete artifact spooling is independent of this preview bound. Longer output is filtered from the complete copy in its artifact (up to 4 MiB) when nothing was elided, and the filtered text is used only if it fits the inline view; otherwise the head-and-tail view, with its middle diagnostics, is shown. Without an artifact store (so the raw output could not be recovered), or after a capture or artifact error, nothing is filtered. Background (`bg`) output is minimized only when a finished job's complete output is read at once. A failed recognised command starts with a `[failure digest: …]` block: for test runs, each failing test (up to 10) with up to 8 lines of its panic or assertion detail, otherwise the first error lines with their locations. The digest is omitted only when every one of its lines is already visible near the top of the shown output.

A compound command or pipeline (`step 2>&1; ls`, `make | tail`) reports the status of its last step, so an earlier step can fail behind `[exit code: 0]`. When such a command exits zero but its output contains a recognised failure line (the digest's error and test-failure forms, or the shell's own `command not found`/`not found` report), the result gains `[exit 0 is the last step's status; earlier output reports a failure: …]` with at most two of those lines. It is an observation, not a changed status: the exit code stays 0 and nothing is retried. A sole command's zero status is its own and never gets the note.

A command that exits 127 because a program is missing (`bash: NAME: command not found`, zsh's `command not found: NAME`, or dash's `sh: 1: NAME: not found`) gets a `missing_program` notice. For common programs it names a known substitute or the providing package, for example `od -A x -t x1z` for `xxd`, `dpkg-dev` for `dpkg-source`, and JavaScript `eval` or `perl` for a missing Python. These are suggestions only: whether the substitute exists in that environment is not checked, and nothing is installed.

## Git guard

`shell.git_guard` refuses git commands that discard uncommitted work, so agents sharing one checkout cannot destroy each other's edits:

- `workers` (default): the guard applies to delegated workers that edit the parent's live checkout (tasks that are not `isolation: "copy"`). The main agent and isolated copies are not restricted.
- `always`: every `bash` and `bg` command is guarded.
- `off`: never.

Refused forms: `git stash` (push, save, drop, clear, pop, apply; `list`, `show`, `create`, `store` run), `git reset --hard`, `git checkout -f`, `git checkout .`/`:/`/`*`, `git checkout -- <path>` (and `git checkout <path>` when a file of that name exists), `git restore` of the working tree (`--staged` alone is fine), `git clean -f` (not with `-n`), and `git switch -f`/`--discard-changes`. Forms that keep work run normally: `git reset`, `--soft`, `--mixed`, `git reset <path>`, `git checkout -b`, `git checkout --ours|--theirs -- <path>`, `git restore --staged`, `git clean -n`.

The command is read before it runs, following `-C <dir>`, `-c k=v` and other global options, a preceding `cd`, `env`/`command`/`exec` wrappers, `sh -c '...'` scripts, subshells and `if`/`while` bodies. A git command whose directory resolves outside every workspace root is allowed (it is another repository). Only clear matches are refused: a script that cannot be lexed (unterminated quote), `git` reached through a variable, alias or script file, and arguments that need expansion are never refused on a guess. The refusal is a blocked tool error naming the form and the safe alternatives; to proceed, ask the user to run it or to set `shell.git_guard = "off"`, or run the worker isolated.

## Undo for shell changes

Tool edits are checkpointed before they happen; a shell command's effects are not, so by default
`/undo` and `/rollback` leave files that `bash` changed as they are. `shell.undo_snapshot = true`
(off by default; **Settings → Tools → Shell → Undo shell changes**) closes most of that gap:

- Before a turn's first `bash` command that may change files, Dial copies the project's source
  files into the session's private checkpoint storage. Read-only commands (`ls`, `git status`,
  `git diff` and similar) never trigger a copy.
- After each such command, and once more when the turn ends, files that differ from the copy are
  added to the turn's checkpoint with their earlier content: changed and deleted files are restored
  by `/undo`, created files are removed. They appear in the turn's changed files and in
  `/review-file`, attributed to `shell`.
- The copy is discarded when the next turn starts.

Not covered: files matched by `.gitignore`, dependency and build directories (`target`,
`node_modules`, `build`, `dist`, `vendor`, virtual environments), `.git`, `.dial`, credential-like
files (`.env`, keys), symlinks, directories added with `/add-dir`, and anything outside the project.
A file over 8 MiB is not copied; if a command changes it, the undo report names it as not restored.
The copy is taken only before the main agent's own `bash` commands. Changes that `eval`, extension
tools or delegated workers make outside checkpoints are captured only when they follow such a
command in the same turn. Because the comparison is against the whole project, a file you edit in
your own editor while the turn runs is recorded as changed too, and `/undo` would put back its
earlier content; `/review-file` shows the difference first.

A project with more than 20,000 source files, or more than 512 MiB of them, is not copied: Dial
says so once in that turn and shell changes stay outside `/undo`. The copy is a copy-on-write clone
where the filesystem supports it (APFS, Btrfs, XFS with reflinks) and a full copy elsewhere, which
takes time and disk space in proportion to the source tree.

The turn report still counts the commands under "Outside /undo", since ignored files and effects
outside the project are not captured. Enabling the setting also makes shell-changed files part of
the turn's changed files, which focused verification and review use; its effect on model behaviour
has not been measured.

## Approval of shell commands

Shell commands are classified from executable arguments, with conservative fallback for unsupported
syntax. Quoted operators are arguments: `echo "curl x | sh"` is ordinary text. An unparseable
command cannot be covered by a reusable allow-prefix rule and asks under `ask`/`write` policy.

- **Expansions.** Commands are judged before bash expands them, so an unquoted glob (`*`, `?`,
  `[`) or `~` makes a command unparseable. A glob can match a file whose name is a
  flag: with a file named `-delete` in the workspace, `find . -delet?` runs `find . -delete`.
  Such a command is never a read-only inspection: `ask` and `write` prompt for it, it counts as an
  uncheckpointed shell command, a read-only subagent cannot run it, and it matches every `bash`
  deny rule. Quoted or backslash-escaped, the characters are literal: `find . -name '*.rs'` is
  still an inspection. Use the `glob` tool to list files by pattern.
- **Downloaded code.** A pipe whose upstream stage fetches (`curl`, `wget`, `fetch`, `aria2c`) and
  whose downstream stage runs a shell or interpreter (`sh`, `bash`, `python3 -`, `node`, `env`/`sudo`
  wrappers, absolute paths, `|&`, with or without spaces) is destructive. Under `write` it asks
  every time. An unparseable upstream or downstream stage after a fetch is treated the same way.
- **Reusable "always allow" grants.** A grant stores up to the first two words of a single plain
  program (for example `git status`). It never covers a destructive form of that program. A
  destructive command is approved once; nothing is saved and Dial says so. Legacy `git push`
  rules cannot authorize later pushes, including `git push --force`. Deny rules still match every
  form. The saved scope is displayed as words (for example ``allow bash `git status` ``).
- **Automatic verification.** Checks Dial runs after an edit are gated before they start:
  - An explicit `deny` rule blocks a check in every approval mode, generated or configured. The
    check is reported as `permission denied`, not as a failing test.
  - Configured verification commands (from `verification.commands`) follow the shell policy:
    `readonly` refuses them, `ask` prompts unless a grant covers them, `write` asks only for
    destructive forms, and `auto` runs them.
  - Generated checks (built-in syntax, type and test selection derived from project files and
    changed paths) are a trusted boundary and run without a prompt in `ask`, `write` and `auto`.
    `readonly`, plan mode, and explicit `deny` rules still apply.
  - Hermetic evaluation runs use the trusted policy: no prompt, but explicit deny rules and plan
    mode still hold.
- Sandbox policy is unchanged by these rules. Approval decides whether a command starts; the
  sandbox still confines what it can write.
