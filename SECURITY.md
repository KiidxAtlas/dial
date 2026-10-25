# Security policy

## Supported versions

Only the latest release receives security fixes. Update with your installer or `dial update`
before reporting.

## Reporting a vulnerability

Please report privately through GitHub: open the repository's **Security** tab and choose
**Report a vulnerability** (a private security advisory). Do not open a public issue or pull
request for an unfixed vulnerability.

Include the Dial version (`dial --version`), platform, sandbox mode, and a minimal reproduction.
Never include real credentials; redact tokens from logs and session files.

## Scope

In scope are weaknesses in Dial's own enforcement, for example:

- escaping the shell sandbox (Seatbelt on macOS, bubblewrap on Linux) or workspace confinement;
- a tool running, or a file changing, without the approval the active policy requires;
- reading private state or credentials stored outside the workspace;
- leaking secrets into model requests, shared transcripts or exports beyond what the documented
  settings allow;
- tampering with release artifacts or the update signature check.

Out of scope: behaviour of third-party model providers, MCP servers, operator tools and
extensions you choose to trust (these run with the permissions you grant), and anything that
requires the explicit unsandboxed mode.
