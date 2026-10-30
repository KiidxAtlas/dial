# Agent-requested directory access

When a tool reports a path outside the workspace, the agent can call
`request_directory` with an existing `path` and a short `reason`. Dial shows a real
**Yes / No** approval popup; the user does not need to type `/add-dir`. After an
approval the agent retries the original tool. Relative directory requests resolve
against the primary workspace, not the shell's current directory. Use an absolute
path rather than a shell expression such as `~`.

## Normal access

The popup identifies the canonical directory, including the target of any symlink
used to name it. Approval adds exactly that directory to the workspace. Native
file tools can read and write there, and sandboxed commands receive the same
workspace-root access. Ordinary tool approvals, network rules, shell sandbox,
symlink confinement and protected Dial storage exclusions are unchanged. This is
not an approval to execute arbitrary tools. Roots are saved with the session in
the same way as `/add-dir` roots.

Directory capability requests always require an actual interactive decision,
including in auto approval modes. There is no “always allow” or “allow project”
option. Denial, cancellation, or a directory changing during the popup leaves the
workspace unchanged. Hermetic and unattended surfaces cannot grant access.
The tool is discoverable through `tool_search` / `tool_load` when not offered.
`/add-dir`, `--add-dir`, `/remove-dir`, and `/dirs` remain available.

## Separate sensitive access

Adding a normal directory (even a home directory) does **not** unlock protected
Dial storage. To request a directory inside that storage the agent must set
`sensitive: true`. Dial first asks for normal directory access, then shows a
**second, separate** sensitive access popup for the exact canonical directory.
Both approvals must be Yes before any change is applied. A protected storage
ancestor such as home is not a valid sensitive request. Request the smallest
needed subdirectory, not the whole Dial user directory.

The sensitive popup explicitly grants **native file-tool read and write access
only**. **Shell commands remain subject to protected-storage sandbox denies.**
The existing cross-platform sandbox policy cannot safely express a subtree
exception to a denied storage ancestor; removing that deny would expose sibling
storage. Dial does not do that. Full sensitive shell access is not implemented.
Native protected-path checks also cannot confine external language servers,
MCP servers, SSH tools, or explicitly unsandboxed commands; their existing separate
execution/approval boundaries still apply.

Credential-containing directories, including those holding Dial's `credentials.json`,
ChatGPT accounts, or Anthropic subscription credentials, cannot receive a sensitive grant.
Session subdirectories can receive grants; native grants do not remove credential exclusions
or scrubbed command environments.

Sensitive grants are process-local, not restored from the session on resume.
The attached root may be restored, but its protected contents are locked again.
Removing the root or moving the workspace revokes its sensitive grant. A denied
second popup does not leave behind a normal root. Filesystem path checking is not
atomic with later file operations; the existing symlink-swap/hard-link residual
limits described in `dial-tools` still apply.

## Regression check

After `cargo build --locked`, run
`python3 examples/benchmarks/directory_access_smoke.py --dial target/debug/dial`.
This uses an isolated temporary workspace, the real TUI, and a scripted local provider
to exercise normal denial/approval and both sensitive decisions, including file and shell
boundaries. `--hold` pauses at the first popup for manual visual inspection; terminal-text
captures alone are not pixel verification or evidence about live model quality.
