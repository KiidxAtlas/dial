# Approved application monitoring

Dial can watch **new output** from a Dial-owned `bg` job, a named foreground `dial run`
wrapper, or an existing regular log file inside the workspace (including attached roots).
Node, Python, Rust and other applications use the same language-independent mechanism.

For a task involving a running development server or runtime errors, built-in agent guidance
asks Dial to proactively offer a relevant watch. This is model guidance, not a guarantee
that every model will suggest it. You can request it directly:

> Watch `logs/site.log` and diagnose and fix new runtime errors.

> Monitor the existing `site` background job for errors.

The agent calls `monitor` with `action: "start"`, a unique `name`, and exactly one of
`job`, `run` or `path`. **Every start/rearm asks for explicit approval**, even in automatic mode
or with a reusable tool grant. Read-only mode refuses starts; deny rules still apply.
There is no blanket approval of repairs: edits, commands, extra access and process control
continue to follow existing permissions. Approving observation does not authorize killing
or restarting an external application.

## Tool examples

For a job previously launched through `bg`:

```json
{"action":"start","name":"site-errors","job":"site","duration_secs":3600}
```

For an application already writing a project log file:

```json
{"action":"start","name":"api-errors","path":"logs/api.log","pattern":"(?i)error|exception|traceback|fatal","duration_secs":3600}
```

## Foreground apps across sessions

In your application terminal, from the same project directory used by Dial:

```sh
dial run --name web -- pnpm start
```

This executes the command directly, without a shell. Shell expansion/pipelines require an
explicit shell, for example `dial run --name web -- sh -c 'pnpm start'`. Arguments after `--`
belong to the child. The child runs in the selected project directory (`dial --dir ... run ...`).
The wrapper is a foreground command, not a daemon or a session-owned background job.
Omit `--name` to use the stable default name `app`: `dial run -- pnpm start`, then
watch with `"run":"app"`. Supply distinct names for concurrent wrappers in one project.

In a separate interactive Dial session, ask “Monitor the named run `web` for new errors.”
After explicit approval the agent can call:

```json
{"action":"start","name":"web-errors","run":"web","duration_secs":3600}
```

Names are project-directory scoped (use the same canonical directory and `DIAL_HOME`),
1–32 ASCII letters/digits/`-`/`_`/`.` other than `.` or `..`. Each watcher has its own
future-only cursor. Stopping a watch, closing Dial, or switching sessions **does not stop
the wrapper or its app**. A new session must ask for a fresh approved watch; it cannot
inherit an approval. Restarting the wrapper under the same name disarms existing watches.

On Unix, when both stdin and stdout are terminals, the child has a controlling PTY:
colors, interactive input, terminal resize and Ctrl-C reach the application. The wrapper
restores the original terminal mode on completion and handled SIGINT/SIGTERM/SIGHUP.
As with any terminal process, SIGKILL or a host crash cannot run restoration code (`stty sane`
can recover a terminal). Ctrl-Z/job-control suspension of the wrapper is not supported.
The wrapper returns the child's exit code, or 128 plus its terminating signal. Its own
launch/capture/I/O failures return an error rather than reporting complete monitoring.
On child exit or wrapper I/O failure, the wrapper cleans up its owned process group;
descendants retaining output after leader exit have a bounded two-second drain window.

On Unix, when either stream is not a terminal, stdout/stderr are merged into the wrapper's
stdout through a pipe (no forced color/TTY semantics). Nonterminal stdin is inherited;
terminal stdin is relayed in cooked mode so redirected stdout cannot background-stop the app.
Stream ordering is the merged OS pipe ordering;
the wrapper does not preserve separate stderr output. There is no capture of another
terminal or an already-running arbitrary PID: launch under `dial run` or provide a log file.
`dial run` currently refuses on Windows before creating capture state or starting any child;
use an application log file with `monitor` `path` instead. A Windows implementation requires
a job-aware relay with bounded descendant drain; no unbounded blocking fallback is used.

Capture is owner-private under the project's external Dial state, not in the checkout.
It keeps at most the latest 256 KiB per run, up to 32 live wrappers, plus bounded protocol
overhead. Each chunk atomically replaces a snapshot; lifetime locks, random generation and
kernel process-start identity detect stale records and restarts. Old inactive captures are
removed on a later launch, so this is not an archival log service. Overruns, missing/corrupt
storage, wrapper disappearance and publication failure stop observation explicitly. Slow
watchers may lose coverage during bursts or long agent turns; no lossless delivery is promised.

Inspect or revoke watches without reading new log content:

```json
{"action":"list"}
```

```json
{"action":"stop","name":"api-errors"}
```

Starting a watch reads from the source's current end; historical errors do not immediately
trigger it. The default regex matches error, exception, traceback, panic, fatal, failed or
failure, case-insensitively. It is a heuristic: use a project-specific pattern to avoid
false positives. Matching is line-oriented, including partial lines, not a language-specific
stack-trace parser.

## Wake-up and lifetime

While the **main interactive terminal is open and idle**, Dial polls approved sources
at most once per second, without model calls for ordinary output. Matching output reserves
bounded diagnostic context and disarms that watch. At a safe boundary, the agent resumes
the existing user task to investigate, repair where permitted, and verify. Logs are framed
as untrusted observations, never new user instructions or approval. Monitoring failures
are reported as monitoring problems, not proof the application is broken.

- Human input, open panels and pending reconfiguration take priority. A busy agent does
  not receive a competing repair turn; new output is checked once it becomes idle.
- Each watch is **one-shot**. Rearming requires another approved start.
- Up to eight watches; default lifetime one hour, configurable from 1 to 86,400 seconds.
- Automatic continuations are limited to eight per user task, with a 30-second cooldown.
- A new submitted user task, cancellation, a blocked/failed/interrupted turn or a turn
  waiting for user input revokes watches. Stopping observation never stops the app.
- Workspace-root changes and mutable workspace reconfiguration revoke watches; removing an
  attached root cannot leave its approved logs readable through a stale watch.
- Watches are process-local, not persisted across restart/session rebuild. There is no
  unattended daemon or automatic wake in print, RPC, ACP or child-worker execution.
- Forked tool sessions never observe the parent's background jobs; file and named-run
  watches remain available to integrations that explicitly poll them.

## Source limits

A PID alone cannot reliably expose an arbitrary running process's stdout. This implementation
does **not** capture another terminal, attach a debugger, discover containers, or restart an
app to redirect its output. An external application must write an accessible log file or be
explicitly launched with `dial run`. Dial-owned jobs reuse their existing output capture
without consuming the normal `bg output` cursor or changing process ownership.

Only existing regular files under approved workspace roots are accepted; private-state
restrictions still apply. File paths are revalidated on polling. Rotation/replacement or
observed truncation stops the watch and requires a fresh approved start rather than silently
following a new source. Truncate-and-regrow between polls may be indistinguishable from an
append. Background-ring overruns, oversized lines and source read errors stop the watch
explicitly instead of claiming complete coverage. Monitoring is not a lossless log service.

When a project artifact store is available, matched diagnostic windows are retained with immutable
evidence citations (see [evidence](evidence.md)). These are bounded terminal-sanitized, possibly
lossy/clipped windows, not raw logs, complete coverage, exact original line numbers or source
revision snapshots. Citations follow normal artifact retention/expiration. A retention error is
reported alongside the match; it does not suppress the diagnostic or rearm the watch. Watch
configuration remains process-local and approvals/wake eligibility do not change.

Logs may contain secrets. Only approve sources suitable for the current model/provider;
normal provider-bound secret masking is not a guarantee that arbitrary application data
contains no sensitive information.
