# Windows

Dial ships native Windows releases for `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`; the installer picks the one matching your machine. Hosted providers, Ollama, LM Studio and GGUF through llama.cpp are supported; MLX is macOS-only.

## Install

Works in Windows PowerShell 5.1 and PowerShell 7. No OpenSSL or other prerequisite is needed. The installer verifies an Ed25519-signed checksum manifest before it installs anything. It installs to `$HOME\.local\bin` (override with `DIAL_INSTALL_DIR`) and adds that directory to your user PATH and to the current session.

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
`Unblock-File` does not help. In that case the installer stops before installing anything. It
reports `Downloaded dial.exe could not start` and says that Smart App Control is on. To check
the state, open Windows Security > App & browser control > Smart App Control settings.

Options:

- Run the Linux build in WSL2 ([WSL2 instructions](release-and-install.md#windows-through-wsl2--linux-x86-64)).
  Smart App Control does not check Linux binaries inside WSL. This is the recommended route.
- Turning Smart App Control off is a system-wide security change. Dial does not require or
  recommend it, and on some Windows builds it cannot be turned back on without resetting Windows.

Building from source does not avoid the block: a locally built unsigned `dial.exe` is also unknown
to Smart App Control.

## First run and LM Studio

First run opens Providers. To use a local model without a hosted account, press Esc once to
open Models. In LM Studio, load a model and start its local API server before selecting it in
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

Windows does not provide macOS Seatbelt or the Linux namespace sandbox used by Dial. Shell and tool execution therefore must not be treated as sandboxed on Windows. Windows shell, browser, PDF, and Python-kernel execution is fail-closed unless `sandbox.mode = "off"` is explicitly set in the configuration. Enabling this setting permits commands to run with the current user's normal Windows privileges; do so only for trusted workspaces and commands. It does not provide filesystem or network confinement.

Windows shell commands are PowerShell source executed by Windows PowerShell; POSIX shell scripts are not translated. Process groups use Windows Job Objects, and background PTY jobs use ConPTY. The Python kernel is available only with `sandbox.mode = "off"`: its trusted driver installs the native `ProcessChildProcessPolicy` no-child-process mitigation before evaluating user code, but Windows filesystem/network isolation is unavailable.

Python environment discovery uses Windows layouts: virtualenv packages come from `<venv>/Lib/site-packages`, while the trusted base interpreter comes from an absolute `executable`/`base-executable` or `home` in `pyvenv.cfg`, not the `Scripts/python.exe` redirector. Conda uses `<prefix>/python.exe`. Base interpreters and conda installations must still resolve outside the workspace and pass the normal interpreter preflight; environment discovery does not relax those checks.

## Known limitations

- MLX/Apple Silicon inference is unavailable. Use a local server such as LM Studio or Ollama, or a hosted provider instead.
- PTY terminal behavior and half-close differ from Unix; ConPTY resize is supported, but managed PTY stdin half-close is unavailable.
- The Windows security boundary is not equivalent to Unix sandboxing; never enable unsandboxed execution for untrusted input.
- Atomically published private files flush staged file contents before replacement. Windows does not document an unprivileged parent-directory flush or write-through rename equivalent to POSIX `fsync` on a directory, so a successful replacement is atomic for concurrent readers but its directory entry is not guaranteed to survive sudden power loss.
