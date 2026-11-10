# Background services

The `bg` tool starts process-group-owned jobs with a bounded live output ring and a content-addressed artifact. Artifact spools are capped at 16 MiB (no configuration key changes this); when a stream exceeds the cap, the spool preserves its head and newest tail with an explicit truncation marker. The artifact digest identifies the bytes actually stored. PTY artifacts preserve raw terminal bytes; model-facing output is rendered from the terminal screen.

## Ordinary session jobs (unchanged default)

Jobs are session-owned unless a call explicitly supplies `persist: true`: use `list`, `output`, `wait`, `input`, `resize`, and `kill` while the session is active. The `/bg` panel lists ordinary session jobs and their status, and shows a PTY job's rendered screen. Enter on a PTY job attaches the keyboard to it; Ctrl-] (or Ctrl-5) detaches. Pipe input and PTY input are explicit tool actions. Processes are stopped when their owning background tool is dropped.

Readers settle for at most 300 ms after leader exit. On cleanup they drain for at most another 300 ms, with a bounded join wait; a descendant retaining an output descriptor cannot hold the controller's readers indefinitely. Incomplete captures are marked partial. POSIX descendants that escape their process group remain a residual cleanup limitation (Linux's sandbox PID namespace adds containment).

PTY starts without explicit `ready` automatically observe the newly started job for up to one second after spawn, returning its rendered initial screen and controller-owned process/capture status. Observed exit ends the observation sooner; a nonzero or signal exit is reported as an error while retaining the job handle for inspection. The existing reader-settlement allowance can add up to 300 ms, plus spawn/scheduling overhead. The wait is interruptible and never sends input or starts a replacement process. A running screen is neither explicit readiness nor behavioral verification. Explicit readiness retains its existing wait path; ordinary pipe starts remain asynchronous. The same observation applies to `bash` with `pty: true`.

## Structured tool jobs

The main controller also accepts an explicitly selected finite tool operation instead of a shell
command. Supply exactly one of `command` and `call`:

```json
{"action":"start","name":"stats","call":{"name":"data","arguments":{"path":"measurements.csv","action":"aggregate","columns":["duration"]}}}
{"action":"wait","name":"stats","timeout":30}
```

The nested tool must already be offered. Its schema, file handles, portable rules, pre-tool hooks
and exact-call approvals are checked before work starts; approval of `bg` never grants the nested
call permission. Initial eligible operations are `data`, `web_fetch`, `web_search`, and `document`
operations without PDF process prerequisites. Eligibility is an explicit implementation opt-in,
not an MCP read-only hint. Workspace mutations, recursive jobs, controller/interactive operations,
and unknown tools are refused. Child-worker controllers do not yet admit tool-backed launches.
If a matching `post_tool_use` hook needs the completed nested result, run that operation in the
foreground: background admission refuses rather than skipping the hook. Newly discovered process
or confirmation prerequisites also fail closed, without a stored approval callback.

Use the existing `list`, `output`, `wait` and `kill` controls. These are completion-blocking finite
jobs, unlike shell services; a turn cannot claim completion while they are running. Tool jobs
have no process exit code. `wait`/`output` retain the original structured data, failure flag and
execution facts; results above the 32 KiB inline data limit point to a structured JSON artifact.
The job retains at most 8 MiB per serialized result. Larger results fail explicitly. Artifact
capture happens when reading the result; missing or partial storage is disclosed, and omitted
structured data without a complete artifact is a delivery error. Finished records remain subject
to the ordinary job-table retention limit.

`kill` requests cooperative cancellation. A running tool remains managed and keeps its name and
capacity slot until it actually returns; an ignored cancellation request is not a stopped thread.
Session shutdown requests cancellation but cannot forcibly terminate a native callback. Web jobs
use independent clients and job-owned cancellation so cancelling one cannot reset or cancel another.
Tool launches reject shell-only options, including `cwd`, PTY/stdin/dimensions, readiness and
persistence/lifetime. Tool waits support completion only, not regex/port readiness.

## Explicit restart-surviving pipe jobs (Unix only)

No saved setting is changed or enabled. On Unix, an explicitly approved start can instead use:

```json
{"action":"start","persist":true,"name":"build","command":"make check","lifetime":3600}
```

This starts a private, finite-lifetime owner process, not a system service installation. It owns the same background process table and bounded output capture used by ordinary jobs. The one-use launch contract is delivered through stdin; commands and environment credentials are not stored in a replay manifest. Shell backend, original strict/developer confinement, denied paths, additional writes, network policy, working directory and explicit operator sandbox opt-out are carried from the approved launch. A restart never grants new permissions or reruns a command.

After a controller exit/restart, use `persist: true` **on every call** to address the separate durable namespace:

```json
{"action":"list","persist":true}
{"action":"output","persist":true,"name":"build"}
{"action":"wait","persist":true,"name":"build","timeout":30,"until":"finished"}
{"action":"kill","persist":true,"name":"build"}
```

Control uses a Unix socket within an owner-only private directory, not HTTP or a network listener. Directory ownership/modes and the recorded supervisor PID **plus kernel start identity** are checked before reconnecting. Status/output reattachment does not launch anything. Missing, dead or unverifiable owners without a finished record are reported interrupted/unknown, never successful or automatically restarted. A completed job retains a bounded final observation; a subsequent named `kill` forgets that finished record. `kill` with `all: true` addresses only the selected namespace.

The kernel owner identity is recorded before the one-use launch contract is sent and before any command starts. Initialization failures retain a terminal error observation instead of an anonymous quota reservation. Inspection never prunes these records: after the owner's exit is confirmed, explicitly use named `kill` to forget a failed launch and release its slot. A timeout or cancellation before launch acknowledgment preserves the owner identity and reports uncertainty; inspect it rather than retrying the command blindly. A helper spawn failure before any contract is delivered releases its empty reservation immediately. An unknown/unverifiable owner is never automatically pruned or replayed.

If an owner is confirmed dead but left no final observation (for example, the controller was killed during contract transfer), an explicit named `kill` can still forget its record and release the slot. The result remains interrupted/partial-effects **unknown**: this does not imply the command never started or that surviving descendants were stopped. No stored PID is signalled and no command is replayed. Live or unverifiable owner identities remain fail-closed; inspection and admission never automatically remove unknown records.

Limits and differences:

- Explicit name required; at most eight retained jobs per workspace, including completed records. Forget completed records with a named `kill` before admitting more.
- Lifetime defaults to one hour and must be 1–86,400 seconds. Expiry stops the owned job and reports that effects may be partial. This is controller-restart survival, **not** reboot persistence, indefinite scheduling, or automatic recovery after owner failure. OS logout/session policies may terminate the owner. Abrupt supervisor death can prevent group cleanup, just as abrupt controller death can for ordinary jobs.
- Pipe-only with closed stdin. Durable PTY/input/resize and `ready` are not implemented; these requests fail closed. Use `wait.until` for log matching. Durable output is a repeatable bounded snapshot, not a per-session delta. The ordinary `/bg` panel and heartbeat do not attach durable jobs; inspect them explicitly.
- Durable wait is cancellable and capped at 300 seconds, including `timeout: 0`; cancelling a wait does not kill the surviving command. Explicit kill remains approval-requiring. A killed/expired job does not imply writes were rolled back.
- On Windows the owner listens on a loopback TCP port instead of a Unix socket. It publishes the port with a random token in an owner-only `control.json`, and drops connections that do not present the token first. Control authority is still being able to read your private job directory. The owner starts in a new process group without a console window, leaving the controller's job when the job allows breakaway.

No command in the examples is run automatically, and no persistent services are installed or started at application startup.
