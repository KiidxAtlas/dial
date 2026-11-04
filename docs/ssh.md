# SSH integration

The `ssh` integration exposes approved remote commands for configured OpenSSH hosts, a local host-list action, and a `close` action that stops this process's shared master for a host. Remote commands require explicit tool approval and run outside the workspace sandbox, with `BatchMode`, bounded deadlines, capped streaming output, and a shared ControlMaster socket under Dial's user directory (or a private short-path fallback). The master persists for `ControlPersist=3600` seconds after last use and is closed at exit. Before the first command to a host, Dial probes its OS and shell once and caches the result under `<DIAL_HOME>/ssh/hosts`; `hosts` lists those labels (for example `linux/bash`) and the open masters. Hosts with an enabled POSIX compat layer run commands through that shell. Host aliases are interpreted by OpenSSH, so normal `~/.ssh/config` settings apply. `ssh://host/path` supports bounded remote reads, writes and grep; each remote operation requires approval and files are limited to 1 MiB. URL transfers are refused on Windows hosts without a POSIX shell. The `ssh://` scheme supports absolute paths and `~/` paths; URL query strings and fragments are rejected.

Enable or disable the `ssh` tool through the `integrations` configuration list. SSH host-key policy defaults to `accept-new`; pre-populate `known_hosts` and configure OpenSSH when stricter policy is required. The host-discovery parser lists concrete aliases from `~/.ssh/config` and included files; OpenSSH remains authoritative for effective connection configuration.

Remote scripts and file-transfer wrappers are encoded before transport and decoded into the
selected POSIX shell on the remote side. Paths are not interpolated into the remote login
shell's quoting grammar. This avoids treating a quoted path as executable syntax under a
different login shell; it does not change remote approval, host-key policy or filesystem authority.

URL writes stage their input in an unpredictable, exclusively created private sibling directory
using remote `mktemp -d`, with exclusive payload creation and exit/signal cleanup. A host lacking
that command fails closed. Existing destinations are written in place, retaining mode, ownership
and destination-symlink behavior; that overwrite is not atomic. New files are moved from the
staging directory after the complete input has arrived. Staging does not make remote filesystem
effects rollback-safe or remove the need for approval.
