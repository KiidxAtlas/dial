# Windows

Dial's Windows release is `x86_64-pc-windows-msvc`. ARM64 Windows runs that binary through built-in x64 emulation. Hosted providers are supported; MLX is macOS-only.

## Install

Works in Windows PowerShell 5.1 and PowerShell 7. No OpenSSL or other prerequisite is needed. The installer verifies an Ed25519-signed checksum manifest before it installs anything. It installs to `$HOME\.local\bin` (override with `DIAL_INSTALL_DIR`) and adds that directory to your user PATH and to the current session.

Open PowerShell (no admin needed) and run:

    irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex

Then open a new terminal (or keep using the same one; its PATH is updated too) and run `dial`.

Pin a version by running the script as a block:

    & ([scriptblock]::Create((irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1))) -Version v0.1.4

With GitHub CLI signed in, the installer downloads through `gh` instead; this is optional.

Dial finds its state directory from `DIAL_HOME`, or else from `HOME` plus `.dial`. It does not read `USERPROFILE`. If neither variable is set, set `DIAL_HOME` (for example, `setx DIAL_HOME "$env:USERPROFILE\.dial"`) before running Dial. The installer warns when this is needed.

## Security

Windows does not provide macOS Seatbelt or the Linux namespace sandbox used by Dial. Shell and tool execution therefore must not be treated as sandboxed on Windows. Windows shell, browser, PDF, and Python-kernel execution is fail-closed unless `sandbox.mode = "off"` is explicitly set in the configuration. Enabling this setting permits commands to run with the current user's normal Windows privileges; do so only for trusted workspaces and commands. It does not provide filesystem or network confinement.

Windows shell commands are PowerShell source executed by Windows PowerShell; POSIX shell scripts are not translated. Process groups use Windows Job Objects, and background PTY jobs use ConPTY. The Python kernel is available only with `sandbox.mode = "off"`: its trusted driver installs the native `ProcessChildProcessPolicy` no-child-process mitigation before evaluating user code, but Windows filesystem/network isolation is unavailable.

Python environment discovery uses Windows layouts: virtualenv packages come from `<venv>/Lib/site-packages`, while the trusted base interpreter comes from an absolute `executable`/`base-executable` or `home` in `pyvenv.cfg`, not the `Scripts/python.exe` redirector. Conda uses `<prefix>/python.exe`. Base interpreters and conda installations must still resolve outside the workspace and pass the normal interpreter preflight; environment discovery does not relax those checks.

## Known limitations

- MLX/local Apple Silicon inference is unavailable; configure a hosted provider.
- PTY terminal behavior and half-close differ from Unix; ConPTY resize is supported, but managed PTY stdin half-close is unavailable.
- The Windows security boundary is not equivalent to Unix sandboxing; never enable unsandboxed execution for untrusted input.
- Atomically published private files flush staged file contents before replacement. Windows does not document an unprivileged parent-directory flush or write-through rename equivalent to POSIX `fsync` on a directory, so a successful replacement is atomic for concurrent readers but its directory entry is not guaranteed to survive sudden power loss.
