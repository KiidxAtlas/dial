# Shell tool

The `bash` tool runs a command with the shell chosen by `shell.backend` (default `auto`): `/bin/bash -c` on Unix when it exists, Git for Windows' `bash.exe` on Windows when it is installed, otherwise Dial's built-in shell (on Windows without the built-in shell, PowerShell). The tool runs from the workspace root. Supply `cwd` to select an existing directory inside the workspace. A simple leading `cd PATH &&` is extracted into the working directory only when PATH resolves to a directory inside the workspace; shell expansions and compound commands are left to the shell. Any other `cd` is not extracted and runs inside the shell, where the sandbox (if enabled) still applies.

Commands run without interactive stdin and are placed in a dedicated process group. The tool combines stdout and stderr, streams output while running, and bounds retained output while preserving diagnostic lines. The configured timeout defaults to 300 seconds; a positive `timeout` argument requests a shorter duration, capped by the configured limit. A timeout of zero in configuration disables the deadline. Long-lived services should be started with the managed background tool rather than detached from foreground Bash.

The sandbox may deny network access or writes outside the project. Use the command's `$TMPDIR` for temporary files. Do not retry a denied command unchanged; adapt it to the sandbox or explain the limitation.

## Shell backends

Set `shell.backend` in Settings (Tools tab) or in `config.toml`:

- `auto` (default): bash when one is found (`/bin/bash` on Unix; on Windows, Git for Windows' `bash.exe` from `%ProgramFiles%\Git\bin`, `%ProgramFiles(x86)%\Git\bin`, `%LOCALAPPDATA%\Programs\Git\bin`, or PATH). A Git installation is recognized by its `cmd\git.exe` beside `bin\bash.exe`, so WSL's `bash.exe` launcher is never used. Without bash: the built-in shell. On Windows without the built-in shell (a build without `embedded-shell`): PowerShell.
- `system`: bash only. On Windows this means Git Bash; it fails with an actionable message when no bash is found.
- `embedded`: Dial's built-in shell. It needs no system bash, so it works on minimal systems and containers.
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

## Named embedded sessions

With `shell.backend = "embedded"`, a foreground `bash` call may supply `session` to retain its
working directory and exported string variables for later calls in this Dial workspace. Named
sessions reject overlapping commands and explicit background execution. Ordinary shell calls
retain their existing background-transfer behavior. Named calls wait for completion or the
configured deadline; cancellation, timeout or an incomplete state exchange invalidates the saved
state. Functions, unexported variables and process-restart persistence are not retained. Protected
host variables and credentials are excluded, and each command keeps the ordinary sandbox policy.

## Approval of shell commands

Shell commands are classified from executable arguments, with conservative fallback for unsupported
syntax. Quoted operators are arguments: `echo "curl x | sh"` is ordinary text. An unparseable
command cannot be covered by a reusable allow-prefix rule and asks under `ask`/`write` policy.

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
