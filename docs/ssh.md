# SSH integration

The `ssh` integration exposes approved remote commands for configured OpenSSH hosts, a local host-list action, and a `close` action that stops this process's shared master for a host. Remote commands require explicit tool approval and run outside the workspace sandbox, with `BatchMode`, bounded deadlines, capped streaming output, and a shared ControlMaster socket under Dial's user directory (or a private short-path fallback). The master persists for `ControlPersist=3600` seconds after last use and is closed at exit. Before the first command to a host, Dial probes its OS and shell once and caches the result under `<DIAL_HOME>/ssh/hosts`; `hosts` lists those labels (for example `linux/bash`) and the open masters. Hosts with an enabled POSIX compat layer run commands through that shell. `ssh://host/path` supports bounded remote reads, writes and grep; each remote operation requires approval and files are limited to 1 MiB. Windows hosts without a POSIX shell use the SFTP v3 subsystem for URL transfers. The `ssh://` scheme supports absolute paths and `~/` paths; URL query strings and fragments are rejected.

Enable or disable the `ssh` tool through the `integrations` configuration list. SSH host-key policy defaults to `accept-new`; pre-populate `known_hosts` and configure OpenSSH when stricter policy is required.

Host names resolve in one of two ways. A `[ssh_hosts]` entry supplies its own `host`, `user`, `port` and `key`, and wins over any `~/.ssh/config` alias with the same name. Every other name is passed to OpenSSH unchanged, so `HostName`, `ProxyJump`, `Match` and the rest of the effective configuration apply exactly as they do on the command line. The host-discovery parser lists concrete aliases from `~/.ssh/config` and included files only so they can be named; its parsed fields (for example `HostName`) are shown by `hosts` but are never used to build connection arguments. OpenSSH remains authoritative for effective connection configuration.

Remote scripts and file-transfer wrappers are encoded before transport and decoded into the
selected POSIX shell on the remote side. Paths are not interpolated into the remote login
shell's quoting grammar. This avoids treating a quoted path as executable syntax under a
different login shell; it does not change remote approval, host-key policy or filesystem authority.

On POSIX hosts, URL writes stage their input in an unpredictable, exclusively created private sibling directory
using remote `mktemp -d`, with exclusive payload creation and exit/signal cleanup. A host lacking
that command fails closed. Existing destinations are written in place, retaining mode, ownership
and destination-symlink behavior; that overwrite is not atomic. New files are moved from the
staging directory after the complete input has arrived. Staging does not make remote filesystem
effects rollback-safe or remove the need for approval.

On Windows hosts without a verified POSIX shell, transfers use SFTP v3 over the same OpenSSH
authentication, host-key and connection policy. No remote shell commands, path expansion or local
temporary payload files are involved. Use `ssh://host/C:/path/file` for a drive path, or
`ssh://host/~/file` relative to the SFTP login directory. Reads fail rather than truncate at 1 MiB;
transfers have a 30-second deadline and honor cancellation. SFTP writes open/create and truncate
the destination in place: unlike the POSIX path they do not stage first. A failed or cancelled
write can leave a partial file. The server must enable its SFTP subsystem. Live transfers to a
native Windows OpenSSH server have not yet been verified.
