# brush-core 0.5.0 (patched for Dial)

Upstream: https://github.com/reubeno/brush, crate `brush-core` 0.5.0 (git sha in
`.cargo_vcs_info.json`), MIT licensed (`LICENSE`). Wired in through `[patch.crates-io]` in the
root `Cargo.toml`.

Local changes (drop this copy once upstream ships equivalent behaviour):

- `src/shell/fs.rs` `find_first_executable_in_path` returns the path from
  `sys::fs::resolve_executable`, so on Windows a bare name found through `PATHEXT` keeps its
  extension (`npx` -> `...\npx.cmd`). Upstream returned the extensionless join, which
  `std::process::Command` then resolved as `npx.exe` or the extensionless POSIX shim.
- `src/shell/fs.rs` adds `resolve_command_path`; `src/commands.rs` uses it for command names that
  contain a path separator, so `./tool` on Windows runs `tool.cmd`/`tool.exe` resolved against the
  shell's working directory. Unix behaviour is unchanged (the name is kept as written).
- `$!` and job PIDs (`src/jobs.rs` `BackgroundPid`, `JobManager::signal_job`, `JobManager::wait_next`,
  `JobManager::job_pid`; `src/interp.rs` `spawn_async_ao_list_in_task` and
  `spawn_pipeline_processes`; `src/expansion.rs` `expand_last_background_process_id`). Upstream
  always expanded `$!` to empty because it only looked at External tasks, and a `&` job runs as an
  internal Tokio task. Now each `&` job gets a shared slot. The clone the job runs in carries it
  (`Shell::background_pid`, inherited by subshells and pipeline members), and every external process
  any of them spawns is recorded there. `$!` is the last external member of the first pipeline
  that starts one (`a | b &` gives `b`, or `a` when `b` is a builtin). A job that starts no
  external process names itself by a **virtual PID**: `0x4000_0000 | n` for `n` in `0..2^30`
  (`0x4000_0000..=0x7FFF_FFFF`), allocated from a process-wide counter when the job starts. Linux
  caps real PIDs at 2^22, macOS at 99999, and Windows PIDs stay far below 2^30 in practice, so no
  live process has one. `$!` waits up to `BACKGROUND_PID_WAIT` (1 s) for an external PID, then
  reports the virtual PID; it is never empty after `&`. `kill` of a job (`%N`, or its `$!` PID,
  real or virtual) first waits up to the same bound for an external PID, then signals every
  external process the job recorded and, for a signal that ends the job, ends the job's task with
  status `128 + signal` (the task is selected against the slot's kill request; nothing is aborted
  mid-await). `wait -n` completes the next finished job (`select_all`). Job PIDs are visible to
  `kill %N`, `wait` and `jobs`. `tokio` gains the `time` feature for the bounded wait.
- Loops yield cooperatively (`src/interp.rs`: `for`, `while`/`until` and arithmetic `for` call
  `tokio::task::coop::consume_budget` each iteration). Without it a background loop of builtins
  (`while :; do :; done &`) never returns `Pending`, so the job's kill request is never observed.
- `/dev/tcp/HOST/PORT` and `/dev/udp/HOST/PORT` redirections (`src/devnet.rs`, hooked from
  `Shell::open_file` in `src/shell/fs.rs`; `src/openfiles.rs` `OpenFile::into_child_stdio`, used by
  `src/commands.rs`). Each resolved address is tried in turn; errors read as Bash's. Unix turns the
  connected socket into a file descriptor, so every redirect mode and external children work.
  Windows keeps the socket as a stream for builtins and relays it through an anonymous pipe per
  direction for external children (unread relayed bytes are lost; UDP datagram boundaries are not
  kept). No connect timeout, as in Bash.
- `src/extensions.rs` `DefaultErrorFormatter` reports a missing command as Bash does,
  `NAME: CMD: command not found` (`NAME` is `$0`, `bash` for Dial's shells), instead of
  `error: command not found: CMD`. Tools that recognise Bash's diagnostic (Dial's verification
  classifier, missing-command hints) then treat both shells alike.
