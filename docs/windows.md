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

## First run and LM Studio

First run opens Providers. To use a local model without a hosted account, press Esc once to
open Models. In LM Studio, load a model and start its local API server before selecting it in
Dial. Dial checks `http://127.0.0.1:1234/v1` by default; set `LM_STUDIO_BASE_URL` if your server
uses another address.

If you start the server after opening Dial, press F5 or Ctrl+R in Models to rescan. With an
empty model list, Enter opens Providers, where you can inspect connection details and run
Test connection. These navigation and test actions keep setup open; Esc from a setup subpage
returns to Models. Esc again from Models without choosing a model exits setup.

## Security

Windows does not provide macOS Seatbelt or the Linux namespace sandbox used by Dial. Shell and tool execution therefore must not be treated as sandboxed on Windows. Windows shell, browser, PDF, and Python-kernel execution is fail-closed unless `sandbox.mode = "off"` is explicitly set in the configuration. Enabling this setting permits commands to run with the current user's normal Windows privileges; do so only for trusted workspaces and commands. It does not provide filesystem or network confinement.

Windows shell commands are PowerShell source executed by Windows PowerShell; POSIX shell scripts are not translated. Process groups use Windows Job Objects, and background PTY jobs use ConPTY. The Python kernel is available only with `sandbox.mode = "off"`: its trusted driver installs the native `ProcessChildProcessPolicy` no-child-process mitigation before evaluating user code, but Windows filesystem/network isolation is unavailable.

Python environment discovery uses Windows layouts: virtualenv packages come from `<venv>/Lib/site-packages`, while the trusted base interpreter comes from an absolute `executable`/`base-executable` or `home` in `pyvenv.cfg`, not the `Scripts/python.exe` redirector. Conda uses `<prefix>/python.exe`. Base interpreters and conda installations must still resolve outside the workspace and pass the normal interpreter preflight; environment discovery does not relax those checks.

## Known limitations

- MLX/Apple Silicon inference is unavailable. Use a local server such as LM Studio or Ollama, or a hosted provider instead.
- PTY terminal behavior and half-close differ from Unix; ConPTY resize is supported, but managed PTY stdin half-close is unavailable.
- The Windows security boundary is not equivalent to Unix sandboxing; never enable unsandboxed execution for untrusted input.
- Atomically published private files flush staged file contents before replacement. Windows does not document an unprivileged parent-directory flush or write-through rename equivalent to POSIX `fsync` on a directory, so a successful replacement is atomic for concurrent readers but its directory entry is not guaranteed to survive sudden power loss.
