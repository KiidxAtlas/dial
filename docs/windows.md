# Windows

Dial ships native Windows releases for `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`; the installer picks the one matching your machine. Hosted providers, Ollama, LM Studio and GGUF through llama.cpp are supported; MLX is macOS-only.

## Install

Works in Windows PowerShell 5.1 and PowerShell 7. No OpenSSL or other prerequisite is needed. The installer verifies an Ed25519-signed checksum manifest before it installs anything. It installs to `$HOME\.local\bin` (override with `DIAL_INSTALL_DIR`) and adds that directory to your user PATH and to the current session.

Windows release builds statically link the C runtime, so the release executable does not require
the Visual C++ Redistributable or Visual Studio. Release checks inspect both normal and
delay-loaded imports for C-runtime DLL dependencies. Windows system DLLs are still required;
this does not bypass Windows application-control policies.

Open PowerShell (no admin needed) and run:

    irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex

Then open a new terminal (or keep using the same one; its PATH is updated too) and run `dial`.

Pin a version by running the script as a block:

    & ([scriptblock]::Create((irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1))) -Version v0.1.4

With GitHub CLI signed in, the installer downloads through `gh` instead; this is optional.

Dial finds its state directory from a nonempty `DIAL_HOME`, otherwise `HOME\.dial`, falling back to `USERPROFILE\.dial` on Windows when `HOME` is missing or empty. A typical default is `C:\Users\YourName\.dial`. To choose another location in PowerShell, set `$env:DIAL_HOME = 'D:\Dial'` before running `dial`. To keep it for future terminals too, run `[Environment]::SetEnvironmentVariable('DIAL_HOME', 'D:\Dial', 'User')` (this does not update terminals already open). `DIAL_INSTALL_DIR` controls the executable location, not this state directory.

### Publisher signatures and download warnings

Dial's installer verifies the Ed25519-signed checksum manifest and archive checksum. Releases
do not require an Azure account or paid signing certificate, and the release workflow does not
Authenticode-sign `dial.exe`. Windows may show an unknown-publisher warning, block it under Smart
App Control, or apply an enterprise policy block even when the installer has verified the
download. The one-command installer simplifies setup; it does not bypass Windows security controls.

After installing or extracting a release, inspect the executable in PowerShell:

```powershell
$signature = Get-AuthenticodeSignature -LiteralPath (Get-Command dial).Source
$signature | Format-List Status, StatusMessage, SignatureType
$signature.SignerCertificate | Format-List Subject, Issuer
$signature.TimeStamperCertificate | Format-List Subject, Issuer
```

For an extracted copy not on PATH, use its full `dial.exe` path instead. `NotSigned` is expected
for current releases; it is separate from the installer's manifest/checksum verification. Report
`HashMismatch`, `NotTrusted`, checksum-verification failures or Windows blocks with the release
version and exact warning, without private system data.

SmartScreen reputation warnings can persist for new signed software; signing does not guarantee
an immediate bypass. Antivirus detections and enterprise policy blocks are separate issues.
Do not disable Defender/SmartScreen or install an untrusted root certificate to run Dial.

### Smart App Control

Smart App Control (Windows 11) blocks any executable that is neither signed by a trusted
publisher nor already known to Microsoft's cloud service. Release builds of `dial.exe` are not
Authenticode-signed, so Smart App Control blocks them. It has no per-app allow list, and
`Unblock-File` does not help. The installer tests the downloaded executable before replacing an
existing installation. If startup fails, it reports the native exit code when available; an
enabled Smart App Control policy is a possible cause, not proof of the cause of every failure.
To check the state, open Windows Security > App & browser control > Smart App Control settings.

Options:

- Run the Linux build in WSL2 ([WSL2 instructions](release-and-install.md#windows-through-wsl2--linux-x86-64)).
  Smart App Control does not check Linux binaries inside WSL. This is the recommended route.
- Turning Smart App Control off is a system-wide security change. Dial does not require or
  recommend it, and on some Windows builds it cannot be turned back on without resetting Windows.

Building from source does not avoid the block: a locally built unsigned `dial.exe` is also unknown
to Smart App Control.

### Application startup errors

A Windows dialog such as **0xc0000142** means a DLL failed to initialize before Dial could start;
it does not identify which DLL failed. It is not the same as an invalid model configuration or a
confirmed Smart App Control block. **0xc0000135** means a required DLL was not found, and
**0xc000007b** means an invalid executable/DLL image (which can include an architecture mismatch).

The installer reports these loader failures separately from a release version mismatch and
leaves an existing installation untouched when the downloaded executable fails its startup check.
If a current release still fails, report the Dial release version, Windows version, machine
architecture, and exact error code. A maintainer may need the faulting DLL from Windows Event
Viewer's Application log to identify the cause; omit private paths and unrelated log contents.
Do not download replacement DLLs from third-party sites or disable Windows security controls.

## First run and LM Studio

First run asks where the model should run: choose Local model to open Models without a hosted
account, or Hosted provider to sign in or add an API key. In LM Studio, load a model and start its local API server before selecting it in
Dial. Dial checks `http://127.0.0.1:1234/v1` by default; set `LM_STUDIO_BASE_URL` if your server
uses another address.

If you start the server after opening Dial, press F5 or Ctrl+R in Models to rescan. With an
empty model list, Enter opens Providers, where you can inspect connection details and run
Test connection. These navigation and test actions keep setup open; Esc from a setup subpage
returns to Models. Esc again from Models without choosing a model exits setup.

## Terminal and Administrator shells

Dial's interface uses virtual terminal sequences. Windows Terminal supports them by default, and
Dial turns them on in the classic console window (`powershell.exe`, `cmd.exe`). If the console is
set to "Use legacy console", Dial stops with a message instead of printing raw codes such as `←[2K`.
In that case, run Dial in Windows Terminal or clear that option in the window's Properties.

Run Dial from a normal, non-Administrator shell, inside a project folder. Files that an elevated
shell creates belong to the Administrators group, and Dial refuses to read private files it does
not own once it is no longer elevated. That refusal reads `private storage target must be an
owner-owned regular file without hard links`. To return the folder to your account, run once in
an Administrator PowerShell:

```powershell
takeown /F "$env:USERPROFILE\.dial" /R /D Y
```

## Security

Windows does not provide macOS Seatbelt or the Linux namespace sandbox. Instead, `sandbox.mode = "dev"` runs shell commands (PowerShell, Git Bash and the built-in shell, foreground and background) at **Low integrity**: Dial re-executes itself as a launcher that lowers its own token, and the command inherits it. Dial labels the workspace and each `sandbox.extra_write` directory Low so commands can create, edit and delete there; it labels existing protected paths (`~/.ssh`, `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker`, `~/.gnupg`, the gcloud and GitHub CLI AppData folders, and Dial's own state) Medium with no-read-up, so commands can neither read nor write them; and it labels each project's existing `.git` Medium, so hooks and config cannot be changed in place. These labels are stored in the folders' security settings and stay after Dial exits; ordinary programs are unaffected, but any other Low-integrity process can also write the labeled project. On first use a fixed PowerShell self-test checks every rule through the real launcher; if any check fails, sandboxed commands are refused, never run unconfined.

Limits of the Windows `dev` sandbox with network allowed: reads elsewhere on the disk and network access are not confined. A command can rename a protected `.git` away and create a new one (only in-place changes are blocked). Files moved, not copied, into the project keep their old label. The uv, pip, npm and Go caches are redirected to `%LOCALAPPDATA%\dial\sandbox`, which is labelled writable, so those installs work without extra configuration. Other folders outside the project, such as `~/.cargo` for cargo's registry, need `sandbox.extra_write`.

### AppContainer: `strict` and `allow_network = false`

`sandbox.mode = "strict"`, and `dev` with `sandbox.allow_network = false`, run shell commands in a Windows **AppContainer** instead. These policies were previously refused on Windows. Each project gets its own AppContainer profile (`dial.sandbox.<hash>`). Dial grants that profile full access to the workspace and each `sandbox.extra_write` folder (including the redirected toolchain caches), read and execute access to the Dial executable, and nothing else. It adds deny entries for existing protected paths and a deny-write entry for each existing `.git`. An AppContainer can read only folders Windows opens to all packages (the Windows and Program Files folders) and folders granted to it, so the rest of your user profile and your other projects are unreadable. With `allow_network = false` the token carries no network capability. `strict` with `allow_network = true` adds the `internetClient` and `privateNetworkClientServer` capabilities. Windows never lets an AppContainer reach loopback, so local servers (`localhost`) are unreachable in either case.

Dial's launcher starts the command suspended. It then checks that the new token is an AppContainer token with exactly the expected capabilities before letting the command run. On first use a fixed PowerShell self-test, run through the real launcher, checks that the container can write the workspace and that it cannot write or read outside it, read a protected path or change `.git`. If any check fails, these commands are refused, never run unconfined. The access entries name only this project's AppContainer and stay in the folders' security settings after Dial exits. Programs installed under your user profile (a per-user Node, Python or Rust toolchain) are not readable from the container: add their folder to `sandbox.extra_write`, or install them system-wide. `sandbox.network_allow`/`network_deny` domain filtering is still refused on Windows. The browser keeps refusing `strict`, because its driver talks to Chromium over loopback.

The browser, PDF parsing (`pdftotext`) and both `eval` kernels also run under `dev`. The browser's Node driver is written to its state folder, and that folder is made writable. The Python and JavaScript kernels (Node or the embedded QuickJS engine) start through Dial's launcher, which installs the native `ProcessChildProcessPolicy` no-child-process mitigation before user code runs. They also run at Low integrity when the sandbox is on. With `sandbox.mode = "off"`, the default, everything runs with your normal Windows privileges; the kernels keep the no-child-process mitigation.

Native AppContainer confinement is not equivalent to the Linux namespace sandbox. For namespace
isolation on Windows, run the Linux build inside
[WSL2](release-and-install.md#windows-through-wsl2--linux-x86-64), with bubblewrap and working
unprivileged user namespaces, and select the required sandbox policy there. Dial refuses unsafe
WSL Windows interop and failed enforcement probes; WSL2 alone is not proof of confinement.
Do not work around a native Windows policy refusal by switching to `off` or enabling network
access when the task requires it blocked.

Windows shell commands use Dial's embedded Bash-compatible engine by default, with bundled utilities resolved before PATH, just like macOS and Linux. `shell.backend = "system"` selects Git Bash; `"powershell"` explicitly selects Windows PowerShell source (POSIX scripts are not translated). Builds without `embedded-shell` fall back to Git Bash, then PowerShell. Process groups use Windows Job Objects, and background PTY jobs use ConPTY, sandboxed like other commands. Durable background jobs are supported; see [background services](background-services.md).

Python environment discovery uses Windows layouts: virtualenv packages come from `<venv>/Lib/site-packages`, while the trusted base interpreter comes from an absolute `executable`/`base-executable` or `home` in `pyvenv.cfg`, not the `Scripts/python.exe` redirector. Conda uses `<prefix>/python.exe`. Base interpreters and conda installations must still resolve outside the workspace and pass the normal interpreter preflight; environment discovery does not relax those checks.

## Diagnostics and foreground monitoring

`read` supports `sys://procs`, `sys://procs/<pid>` and `sys://ports` through native process
snapshots and socket tables. Command line and working directory visibility depend on process
access rights; elevated and other users' processes may reveal only their image name. Open files
are not listed on Windows. Optional changed-line coverage supports Python and Node checks with
PowerShell or a configured Bash-compatible shell.

`dial run` captures merged stdout/stderr through a bounded Job Object pipe relay with inherited
stdin and a two-second descendant drain window. It does not provide PTY/color semantics on
Windows. See [application monitoring](application-monitoring.md). SSH URL reads/writes to Windows
hosts without a POSIX shell use SFTP; see [SSH](ssh.md) for partial-write semantics.

These parity ports have macOS unit-test and Windows cross-compilation coverage; native Windows
runtime verification is still pending. They do not change the sandbox's security guarantees.

`bg` `ready.port` proves listener ownership from the IP Helper socket table: a listening TCP
socket on the port must belong to a process inside the job's Job Object. Program names in
`dial run`, `[[hooks]]` commands and `[mcp.*]` servers are resolved with `PATHEXT`, so `npm`,
`npx` and other `.cmd` shims start. The external draft editor (`app.editor.external`) falls back
to Notepad when neither `VISUAL` nor `EDITOR` is set, and copying an answer uses PowerShell's
`Set-Clipboard`.

**Computer use** (`/computer on`) uses the pinned Node runtime embedded in release binaries;
installing Node or changing `PATH` is not required. Dial verifies and extracts that runtime into
its private cache on first use, without a download. Source builds without a bundled payload
still use an operator-installed `node` outside the editable workspace. The worker gets the same
Windows system variables (`SystemRoot`, `SystemDrive`, `ProgramFiles`, ...) as the browser.
Dial suppresses modal Windows loader dialogs and reports startup failures as errors. A corrupt
bundled runtime fails closed instead of silently switching to a different Node installation;
repair/reinstall the Dial release. For an unbundled source build, verify `node --version` in a
new terminal and install the current Node.js LTS for your CPU if necessary.

`Computer pipe setup failed: Access is denied. (os error 5)` after Node starts can be a Dial
pipe-validation bug, not a Node installation or desktop permission problem. Affected builds
query the write-only worker stdin with `GetNamedPipeInfo`, which requires extra read-attribute
access. The fixed implementation validates both pipe ends without that requirement. Update to
a release containing this fix or rebuild from the fixed source; running as Administrator,
disabling the sandbox, or reinstalling Node is not the remedy for this pipe-check failure.

## File edits and checkpoints

An edit refused with `write to "…" blocked by the harness` and Windows error 24
("The program issued a command but the command length is incorrect") can come from
checkpoint creation, not a shell command or an oversized patch. Earlier builds supplied an
undersized native rename buffer for one-character checkpoint names such as `files/0`.
The current code includes the native structure's minimum size. If using an affected build,
update to a release containing this fix or rebuild from the fixed source; shortening the patch
or switching shells does not address this cause. Checkpoint failures still block the edit so
the original file is not changed without undo coverage.

## Known limitations

- MLX/Apple Silicon inference is unavailable. Use a local server such as LM Studio or Ollama, or a hosted provider instead.
- PTY terminal behavior differs from Unix. ConPTY resize is supported. ConPTY has no half-close, so `close_stdin` on a terminal job sends Ctrl+Z, Enter, which ends input only for programs reading the console in line mode.
- The Windows `dev` sandbox with network allowed (Low integrity) confines writes but not reads or network. `strict` and `allow_network = false` use an AppContainer, which also confines reads and network but blocks loopback and programs installed under your user profile. Neither is identical to the macOS and Linux sandboxes. Never run untrusted input with `sandbox.mode = "off"`.
- Atomically published private files flush staged file contents before replacement. Windows does not document an unprivileged parent-directory flush or write-through rename equivalent to POSIX `fsync` on a directory, so a successful replacement is atomic for concurrent readers but its directory entry is not guaranteed to survive sudden power loss.
- Dial replaces and deletes its own files with POSIX semantics on NTFS/ReFS, so a file another process has open (with delete sharing) can still be replaced, and a deleted name is freed at once. When a program opens a file without delete sharing (some editors, backup and sync tools), Dial retries for about 0.3 s and then reports Windows error 32, "being used by another process". Close that program, or move the project out of a synced folder such as OneDrive.
- `/autoresearch run` benchmarks and `dial learn` command verification run through the same shell as the `bash` tool (the embedded engine by default, or Git Bash with `shell.backend = "system"`) in the strict offline sandbox, with the same Windows sandbox limits as above. A verification command that cannot start is reported as an error, not as a failed check.
