//! How tools and commands run: operator tools, the shell, SSH hosts, approval and the sandbox.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Zero disables the execution deadline.
    #[serde(default)]
    pub timeout_secs: u64,
    /// What the tool does, shown to the model. Empty uses a generic description.
    #[serde(default)]
    pub description: String,
    /// JSON Schema (an object schema) for the arguments. When set, the model's arguments are
    /// validated against its required properties and passed to the program as one JSON object on
    /// stdin; when omitted the tool takes a single string `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

/// Which shell runs `bash` commands. `auto` uses bash when one is installed: `/bin/bash` on Unix, and
/// Git for Windows' `bash.exe` on Windows (WSL's launcher does not count). Without bash it uses the
/// built-in shell (`embedded`), and on Windows without an embedded build it uses PowerShell.
/// `system` requires bash. `inprocess` runs the built-in shell inside Dial itself and is refused unless
/// `sandbox.mode` is `off`. `powershell` runs PowerShell on Windows only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellBackend {
    #[default]
    Auto,
    System,
    Embedded,
    /// The built-in shell run inside the Dial process. Allowed only with `sandbox.mode = "off"`.
    #[serde(rename = "inprocess")]
    InProcess,
    PowerShell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Maximum bash timeout in seconds; zero disables the deadline.
    pub bash_timeout_secs: u64,
    /// Transfer a still-running foreground command to `bg` after this duration; zero disables.
    pub auto_background_secs: u64,
    /// `--no-pty`: background commands run without a pseudo-terminal.
    pub no_pty: bool,
    /// Shell that runs `bash` commands: `auto`, `system`, `embedded` (built-in POSIX shell), or `inprocess` (built-in shell inside Dial; sandbox off only).
    pub backend: ShellBackend,
    /// Shrink the output of recognised noisy commands (cargo, git, npm, pytest, go test) before the model sees it.
    /// The complete raw output stays in the command's output artifact.
    pub minimize_output: bool,
    /// When `bash` and `bg` refuse git commands that discard uncommitted work: `workers`, `always` or `off`.
    pub git_guard: GitGuard,
}
impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            bash_timeout_secs: 300,
            auto_background_secs: 15,
            no_pty: false,
            backend: ShellBackend::Auto,
            minimize_output: true,
            git_guard: GitGuard::Workers,
        }
    }
}

/// Where `bash` and `bg` refuse git commands that discard uncommitted work (`git reset --hard`,
/// `git stash`, `git checkout -- <path>`, `git clean -f`, ...). `workers` guards delegated workers that
/// share the parent's checkout; `always` guards every command; `off` never refuses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitGuard {
    #[default]
    Workers,
    Always,
    Off,
}

/// One host added with `/ssh add`. `~/.ssh/config` hosts are merged with these; an entry here wins on a name clash.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshHostConfig {
    /// Hostname or address to connect to.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Identity file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    /// Prompt before writes, edits and shell commands.
    Ask,
    /// Workspace edits, file writes and non-destructive shell commands run without a prompt;
    /// destructive commands (recursive deletes, history-rewriting git, privilege escalation,
    /// piping downloads into a shell) and tools that leave the workspace still ask.
    #[default]
    Write,
    Auto,
    /// Read-only tools only.
    Readonly,
}

impl Approval {
    /// Canonical policy names for command-line choices, completion and settings views.
    pub const NAMES: &'static [&'static str] = &["ask", "write", "auto", "readonly"];

    pub const fn description(self) -> &'static str {
        match self {
            Self::Ask => "asks before every edit and command",
            Self::Write => {
                "edits and safe commands run; destructive or outside-the-project actions ask"
            }
            Self::Auto => "runs everything without asking",
            Self::Readonly => "read-only tools only",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    Off,
    /// Shell can read most of the system but not secrets or dial state, and write only the project.
    #[default]
    Dev,
    /// Workspace-only reads and writes, no network.
    Strict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// Dev mode only; strict mode never has network.
    pub allow_network: bool,
    /// Extra writable directories in dev mode (for example `~/.cargo`, `~/.npm`).
    pub extra_write: Vec<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Dev,
            allow_network: true,
            extra_write: vec![],
        }
    }
}
