//! Built-in, model-facing tools: read, write, edit, glob, grep and bash.
//!
//! Every file path a model supplies is resolved against the workspace root:
//! `..` escapes, absolute paths outside the root and symlinks that lead out of
//! it are rejected, and directory walks (glob, grep) never follow symlinks.
//! `bash` runs inside the macOS sandbox (see [`sandbox`]) and refuses to
//! run at all when no sandbox is available.
//!
//! Residual limits, stated plainly:
//! * Path checks and later file operations are not atomic. Tools run one at a
//!   time, but a process the model leaves running between calls could swap a
//!   symlink in the window; bash is killed at the end of every call, so only a
//!   process that escapes its process group (e.g. via `setsid`) could do so.
//!   The sandbox still bounds what such a process can reach.
//! * Hard links to files outside the workspace cannot be detected by path.
//!   The sandbox forbids creating them from inside bash, but a pre-existing
//!   hard link in the workspace would be readable.
//! * Timeouts kill the bash process group. A command that calls `setsid` can
//!   outlive the call (still sandboxed, but orphaned).

pub mod dap;
pub mod artifact;
pub mod browser;
pub mod kernel;
pub mod navigation;
pub mod documents;
pub mod ext;
pub mod background;
pub mod ast;
pub mod mcp;
pub mod lsp;
pub mod hashline;
pub mod sandbox;
pub mod diagnostics;
mod files;
mod diff;
mod tolerant;
mod search;
mod select;
pub(crate) mod shell;
pub(crate) mod walk;
pub mod web;
#[cfg(test)]
mod testkit;

use crate::config::types::{EditModeConfig, SandboxConfig};
use sandbox::Sandbox;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub const BUILTIN: &[&str] = &["read", "write", "edit", "glob", "grep", "bash"];

/// dial's own tools beyond the file/shell built-ins.
pub const EXTRA_TOOLS: &[&str] = &["todo", "finish", "task", "solve", "escalate", "skill", "skill_manage", "session_search", "tool_search", "tool_load", "artifact_read", "working_state", "task_message", "recall", "retain", "memory_update", "forget", "memory_history", "memory_import"];

/// Optional integration tools (see [`ext`]). Offered unless `integrations` in the config omits them.
pub const INTEGRATION_TOOLS: &[&str] = &["bg", "web_fetch", "web_search", "lsp", "ast_search", "ast_edit", "dap", "find", "browser", "kernel"];

/// Every tool dial enables by default: the built-ins plus [`EXTRA_TOOLS`].
pub fn default_tools() -> Vec<String> {
    BUILTIN.iter().chain(["todo", "tool_search", "tool_load", "artifact_read", "working_state"].iter()).map(|s| s.to_string()).collect()
}

const MAX_LIST: usize = 500;

/// Most bytes of one tool result the agent keeps in the conversation. The built-in tools cap their own
/// output near this and say how to fetch the rest; the agent applies [`model_output_limit`] on top.
pub const MODEL_OUTPUT_LIMIT: usize = 24 * 1024;
/// Floor for [`model_output_limit`]: below this a compiler error with its location no longer fits.
const MIN_MODEL_OUTPUT: usize = 4 * 1024;

/// The per-result byte limit for a model with `context_window` tokens: about an eighth of the window
/// (at ~4 bytes per token) between 4 KiB and [`MODEL_OUTPUT_LIMIT`]. A fixed 24 KiB result is about
/// 6k tokens, most of an 8k local model's context; full text stays retrievable from artifacts.
pub fn model_output_limit(context_window: u32) -> usize {
    (context_window as usize / 2).clamp(MIN_MODEL_OUTPUT, MODEL_OUTPUT_LIMIT)
}

/// Per-workspace scratch directory exported as TMPDIR for bash; walks skip it.
pub const SCRATCH_DIR: &str = ".tmp";

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(text: String) -> Self {
        Self { text, is_error: false }
    }

    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }
}

/// Which `edit` contract the model sees: hashline patches (default) or exact-text replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditMode {
    #[default]
    Hashline,
    Replace,
}

/// Called with the canonical absolute path BEFORE any file is created,
/// overwritten, moved or deleted by the file tools. An `Err` aborts the
/// mutation (and, for a multi-file patch, everything not yet written).
pub trait WriteObserver: Send + Sync {
    fn before_write(&self, abs: &Path) -> Result<()>;
    fn before_delete(&self, abs: &Path) -> Result<()> {
        self.before_write(abs)
    }
}

#[derive(Clone, Default)]
struct ObserverSlot(Option<Arc<dyn WriteObserver>>);

impl std::fmt::Debug for ObserverSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Some(WriteObserver)" } else { "None" })
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    sandbox: Option<Sandbox>,
    allow_network: bool,
    policy: Option<sandbox::Policy>,
    unsandboxed: bool,
    edit_mode: EditMode,
    observer: ObserverSlot,
    /// Session snapshots / seen lines / registers for hashline; shared by clones.
    edits: Arc<Mutex<hashline::EditStore>>,
    artifacts: Option<artifact::ArtifactStore>,
    operations: Arc<Mutex<()>>,
}

impl Workspace {
    pub fn new(root: &Path) -> Result<Workspace> {
        let root = root
            .canonicalize()
            .with_context(|| format!("workspace {} does not exist", root.display()))?;
        ensure!(root.is_dir(), "workspace {} is not a directory", root.display());
        ensure!(root.parent().is_some(), "the filesystem root cannot be a workspace");
        Ok(Workspace {
            root,
            sandbox: None,
            allow_network: false,
            policy: None,
            unsandboxed: false,
            edit_mode: EditMode::default(),
            observer: ObserverSlot::default(),
            edits: Arc::default(),
            artifacts: None,
            operations: Arc::default(),
        })
    }

    pub fn with_artifacts(mut self, store: artifact::ArtifactStore) -> Self { self.artifacts = Some(store); self }
    pub fn artifacts(&self) -> Option<&artifact::ArtifactStore> { self.artifacts.as_ref() }
    pub(crate) fn serial_operation(&self) -> MutexGuard<'_, ()> { self.operations.lock().unwrap_or_else(|e| e.into_inner()) }

    /// Override sandbox detection (tests, or an operator who pre-probed).
    pub fn with_sandbox(mut self, sandbox: Sandbox) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Permit network access from bash. Off by default.
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Run bash under an explicit sandbox policy (strict or developer).
    pub fn with_policy(mut self, policy: sandbox::Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Run bash with no sandbox, only a scrubbed environment. An explicit operator opt-out.
    pub fn without_sandbox(mut self) -> Self {
        self.unsandboxed = true;
        self
    }

    /// Choose the `edit`/`read` contract (default: hashline).
    pub fn with_edit_mode(mut self, mode: EditMode) -> Self {
        self.edit_mode = mode;
        self
    }

    /// Install the observer that sees every file mutation before it happens.
    pub fn with_observer(mut self, observer: Arc<dyn WriteObserver>) -> Self {
        self.observer = ObserverSlot(Some(observer));
        self
    }

    pub fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, hashline::EditStore> {
        self.edits.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn observe_write(&self, abs: &Path) -> Result<(), String> {
        match &self.observer.0 {
            Some(observer) => observer
                .before_write(abs)
                .map_err(|e| format!("write to {:?} blocked by the harness: {e:#}", self.rel(abs))),
            None => Ok(()),
        }
    }

    pub(crate) fn observe_delete(&self, abs: &Path) -> Result<(), String> {
        match &self.observer.0 {
            Some(observer) => observer
                .before_delete(abs)
                .map_err(|e| format!("delete of {:?} blocked by the harness: {e:#}", self.rel(abs))),
            None => Ok(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path to a canonical-prefixed absolute path that
    /// is guaranteed to lie inside the workspace. The target need not exist;
    /// existing components are resolved through symlinks and re-checked.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, String> {
        if input.contains('\0') {
            return Err("path contains a NUL byte".to_string());
        }
        let path = Path::new(input);
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root).map_err(|_| {
                format!(
                    "path {input:?} is outside the workspace; use a path relative to the workspace root"
                )
            })?
        } else {
            path
        };
        let mut current = self.root.clone();
        let mut missing = false;
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if missing || current == self.root {
                        return Err(format!("path {input:?} escapes the workspace"));
                    }
                    current.pop();
                }
                Component::Normal(name) => {
                    current.push(name);
                    if missing {
                        continue;
                    }
                    match fs::symlink_metadata(&current) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            current = fs::canonicalize(&current).map_err(|_| {
                                format!("path {input:?} goes through a dangling symlink")
                            })?;
                            if !current.starts_with(&self.root) {
                                return Err(format!(
                                    "path {input:?} follows a symlink out of the workspace"
                                ));
                            }
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                        Err(e) => return Err(format!("cannot access {input:?}: {e}")),
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("path {input:?} is not a valid workspace path"));
                }
            }
        }
        Ok(current)
    }

    pub(crate) fn rel(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

pub(crate) fn tool_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        },
    })
}

const HASHLINE_READ_DESC: &str = "Read a text file (or list a directory). Output starts with a header `[PATH#TAG]` \
followed by `LINE:TEXT` rows; copy that header's TAG into `edit` sections. Select lines by appending to the path \
`:50-100`, `:50+20` (20 lines from 50), `:50-` (from 50), `:-30` (last 30 lines) or comma-joined ranges such as \
`:5-16,960-973`; or use offset/limit (not together with a selector). A footer states the lines returned and the \
file's total line count. \
`:raw` (e.g. `src/a.rs:raw:1-40`) returns verbatim text without anchors; raw output cannot be edited. \
Without a range, output is capped at 20 KiB and the footer names the range to read next, so read large files in \
ranges. `edit` may only touch lines you have been shown.";

const HASHLINE_WRITE_DESC: &str = "Create or overwrite a whole file (max 1 MiB); missing parent directories are created. \
Returns the fresh `[PATH#TAG]` header, usable for `edit` right away. Prefer `edit` for changing part of an existing file.";

const HASHLINE_EDIT_DESC: &str = r#"Edit existing files with a line-anchored patch (use `write` to create or overwrite whole files).
`input` holds one or more sections, each a header `[PATH#TAG]` followed by operations. TAG is the 4-hex tag from the latest `read`, `edit` or `write` result for that file. Line numbers always refer to THAT snapshot, never to earlier operations in the same call, and you may only touch lines a read has shown you.
Operations (N and M are line numbers):
PUT N.=M:   replace lines N..M with the `+` rows that follow (N.=N for one line)
PUT <N:     insert the `+` rows before line N   (PUT <1: = top of file)
PUT >N:     insert after line N                  (PUT >$: = end of file)
CUT N.=M    delete lines N..M
PUT N*:     replace the whole block that starts at line N (function, class, { } or [ ] body, indented body, markdown section)
PUT >N*:    insert after that block;   CUT N* deletes it
REM         delete the file;   MV new/path   move/rename it (after any edits)
Body rows: every row starts with `+` followed by the exact text including indentation. A lone `+` is a blank line. Text that itself starts with + or - is written `++x` or `+-x`. Rows are the final text, not a diff; never write `-` rows.
Moving code: `CUT 3.=5 @tmp` then `PUT >9 @tmp` (works across files in one call; `PUT N.=M @tmp` replaces with it; without @name one anonymous register is used).
Example:
[src/main.rs#3FA9]
PUT 4.=4:
+    let total = a + b;
CUT 10.=12
PUT >$:
+// end
Put all changes to one file in one section. On success you get the fresh [PATH#TAG] and the changed lines; use that tag next time. If the file changed since your read, the edit is re-anchored only when the lines it touches are unchanged; otherwise you receive the current lines so you can retry."#;

const REPLACE_READ_DESC: &str = "Read a text file in the workspace with line numbers (or list a directory). \
Select lines by appending to the path `:50-100`, `:50+150` (150 lines from 50), `:50-` (from 50), `:-60` (last 60 lines) \
or comma-joined ranges such as `:5-16,960-973`; or use offset/limit (not together with a selector). A footer states \
the lines returned and the file's total line count. Without a range, output is capped at 20 KiB and the footer names \
the range to read next, so read large files in ranges.";

fn schema(name: &str, mode: EditMode) -> Option<Value> {
    let hashline = mode == EditMode::Hashline;
    Some(match name {
        "read" => tool_schema(
            "read",
            if hashline { HASHLINE_READ_DESC } else { REPLACE_READ_DESC },
            json!({
                "path": {"type": "string", "description": "File or directory path, relative to the workspace root; may end in a line selector such as `:50-100`, `:50+150`, `:-60` (last 60 lines), `:5-16,960-973` or `:raw`."},
                "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start at (default 1); not together with a path selector."},
                "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines (default 2000)."},
            }),
            &["path"],
        ),
        "write" => tool_schema(
            "write",
            if hashline {
                HASHLINE_WRITE_DESC
            } else {
                "Create or overwrite a file in the workspace with the given content (max 1 MiB). \
                 Missing parent directories are created."
            },
            json!({
                "path": {"type": "string", "description": "File path, relative to the workspace root."},
                "content": {"type": "string", "description": "Full file content."},
            }),
            &["path", "content"],
        ),
        "edit" if hashline => tool_schema(
            "edit",
            HASHLINE_EDIT_DESC,
            json!({
                "input": {"type": "string", "description": "The patch: `[PATH#TAG]` sections of PUT/CUT/REM/MV operations with `+` body rows."},
            }),
            &["input"],
        ),
        "edit" => tool_schema(
            "edit",
            "Replace exact text in a file. `old` must match exactly once unless replace_all is true; \
             include surrounding lines to make it unique.",
            json!({
                "path": {"type": "string", "description": "File path, relative to the workspace root."},
                "old": {"type": "string", "description": "Exact text to replace (non-empty)."},
                "new": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."},
            }),
            &["path", "old", "new"],
        ),
        "glob" => tool_schema(
            "glob",
            "List workspace files matching a glob, sorted, max 500. Supports `*`, `?`, `[a-z]`, `{a,b}` \
             and `**`. A pattern without `/` matches file names at any depth. \
             Symlinks and the .git directory are skipped.",
            json!({
                "pattern": {"type": "string", "description": "Glob such as `src/**/*.rs` or `*.py`, relative to the workspace root."},
            }),
            &["pattern"],
        ),
        "grep" => tool_schema(
            "grep",
            "Search workspace files with a Rust regular expression (or literal text with `fixed_string`), matched \
             line by line. Returns `path:line:text`; with `context`, surrounding lines are shown as \
             `path-line-text` and non-adjacent groups are separated by `--`. Case-sensitive unless ignore_case is \
             true. Up to 200 matches by default (`max_results`, max 1000). Binary files, symlinks and the .git \
             directory are skipped.",
            json!({
                "pattern": {"type": "string", "description": "Regular expression (Rust `regex` syntax, e.g. `fn \\w+\\(` or `TODO|FIXME`; non-empty). Set fixed_string to search literal text instead."},
                "fixed_string": {"type": "boolean", "description": "Treat pattern as literal text, not a regex (default false)."},
                "path": {"type": "string", "description": "File or directory to search (default: workspace root)."},
                "glob": {"type": "string", "description": "Only search files whose workspace-relative path matches this glob."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)."},
                "context": {"type": "integer", "minimum": 0, "maximum": 10, "description": "Lines of context to show before and after each match (default 0)."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum number of matching lines (default 200)."},
            }),
            &["pattern"],
        ),
        "bash" => tool_schema(
            "bash",
            "Run a shell command with `/bin/bash -c` in the workspace root. It may run inside a sandbox: writes are \
             confined to the project, some paths outside it can be unreadable, and the network may be blocked. \
             `/tmp` is outside the project: put temporary files in `$TMPDIR`, a scratch directory inside it. \
             `Operation not permitted` or a network failure means the sandbox refused it, not that your command \
             is wrong: do not repeat it unchanged, work inside the project or tell the user. stdout and stderr are \
             merged; large output keeps its beginning and its end (the end holds build and test verdicts). The \
             process group is killed on timeout and when the command returns, so background jobs do not survive.",
            json!({
                "command": {"type": "string", "description": "Shell command to run."},
                "timeout": {"type": "integer", "minimum": 1, "description": "Timeout in seconds (capped by the harness limit)."},
            }),
            &["command"],
        ),
        _ => return None,
    })
}

pub fn schemas(names: &[&str], mode: EditMode) -> Result<Vec<Value>> {
    names
        .iter()
        .map(|name| match schema(name, mode) {
            Some(value) => Ok(value),
            None => bail!("unknown tool {name:?}; built-in tools are {BUILTIN:?}"),
        })
        .collect()
}

/// Run one tool call. Never panics: failures become `is_error` output.
pub fn execute(ws: &Workspace, name: &str, args: &Value, timeout_secs: u64) -> ToolOutput {
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(ws, name, args, timeout_secs)
    }));
    match run {
        Ok(Ok(output)) => output,
        Ok(Err(message)) => ToolOutput::err(message),
        Err(_) => ToolOutput::err(format!("internal error while running tool {name:?}")),
    }
}

fn dispatch(ws: &Workspace, name: &str, args: &Value, timeout_secs: u64) -> Result<ToolOutput, String> {
    if !args.is_object() {
        return Err("tool arguments must be a JSON object".to_string());
    }
    match name {
        "read" => files::read_tool(ws, args).map(ToolOutput::ok),
        "write" => files::write_tool(ws, args).map(ToolOutput::ok),
        "edit" => match ws.edit_mode {
            EditMode::Hashline => hashline::input_arg(args).and_then(|input| hashline::edit(ws, input)).map(ToolOutput::ok),
            EditMode::Replace => files::edit_tool(ws, args).map(ToolOutput::ok),
        },
        "glob" => search::glob_tool(ws, args).map(ToolOutput::ok),
        "grep" => search::grep_tool(ws, args).map(ToolOutput::ok),
        "bash" => shell::bash_tool(ws, args, timeout_secs),
        _ => Err(format!("unknown tool {name:?}; available tools: {}", BUILTIN.join(", "))),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(format!("argument {key:?} must be a string")),
        None => Err(format!("missing required argument {key:?}")),
    }
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("argument {key:?} must be a string")),
    }
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("argument {key:?} must be a non-negative integer")),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(format!("argument {key:?} must be a boolean")),
    }
}

fn io_message(action: &str, path: &str, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => format!("{action} {path:?}: no such file or directory"),
        io::ErrorKind::PermissionDenied => format!("{action} {path:?}: permission denied"),
        _ => format!("{action} {path:?}: {error}"),
    }
}

impl From<EditModeConfig> for EditMode {
    fn from(mode: EditModeConfig) -> Self {
        match mode {
            EditModeConfig::Hashline => EditMode::Hashline,
            EditModeConfig::Replace => EditMode::Replace,
        }
    }
}

/// Apply the configured shell sandbox. `protected` is (dial state dir, dial user dir): in developer
/// mode bash may read most of the machine but never those or common credential directories.
pub fn apply_sandbox_config(
    ws: Workspace,
    config: &SandboxConfig,
    protected: Option<(&Path, &Path)>,
) -> Result<Workspace> {
    use crate::config::types::SandboxMode;
    use sandbox::Policy;
    Ok(match config.mode {
        SandboxMode::Off => ws.without_sandbox(),
        SandboxMode::Strict => ws.with_policy(Policy::Strict { allow_network: false }),
        SandboxMode::Dev => {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let mut deny_read: Vec<PathBuf> = Vec::new();
            if let Some((state, user)) = protected {
                deny_read.push(state.to_owned());
                deny_read.push(user.to_owned());
            }
            if let Some(home) = &home {
                for rel in [".ssh", ".aws", ".gnupg", ".config/gcloud", ".azure", ".kube", ".docker", "Library/Keychains"] {
                    deny_read.push(home.join(rel));
                }
            }
            let extra_write = config.extra_write.iter().map(|p| match (p.strip_prefix("~"), &home) {
                (Ok(rest), Some(home)) => home.join(rest),
                _ => p.clone(),
            }).collect();
            ws.with_policy(Policy::Dev { deny_read, extra_write, allow_network: config.allow_network })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testkit::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn schemas_cover_builtins_and_reject_unknown() {
        let all = schemas(BUILTIN, EditMode::Replace).unwrap();
        assert_eq!(all.len(), BUILTIN.len());
        for (schema, name) in all.iter().zip(BUILTIN) {
            assert_eq!(schema["function"]["name"], *name);
            assert_eq!(schema["type"], "function");
        }
        assert!(schemas(&["read", "launch_missiles"], EditMode::Replace).unwrap_err().to_string().contains("launch_missiles"));
        assert!(Workspace::new(Path::new("/definitely/not/here")).is_err());
    }

    #[test]
    fn unknown_tool_and_bad_args_are_errors_not_panics() {
        let (_dir, ws) = workspace();
        assert!(run(&ws, "nope", json!({})).is_error);
        assert!(run(&ws, "read", json!("text")).is_error);
        let out = run(&ws, "read", json!({"path": 5}));
        assert!(out.is_error && out.text.contains("must be a string"));
        assert!(run(&ws, "read", json!({})).text.contains("missing"));
    }

    #[test]
    fn paths_cannot_escape_workspace() {
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), "secret").unwrap();
        fs::write(dir.path().join("inside.txt"), "hello\n").unwrap();
        symlink(outside.path().join("canary"), dir.path().join("file_link")).unwrap();
        symlink(outside.path(), dir.path().join("dir_link")).unwrap();
        symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        symlink(dir.path().join("inside.txt"), dir.path().join("sub/inner_link")).unwrap();

        let canary = outside.path().join("canary");
        let escapes = [
            "../x".to_string(),
            "sub/../../x".to_string(),
            canary.to_string_lossy().into_owned(),
            "file_link".to_string(),
            "dir_link/canary".to_string(),
            "dir_link/new.txt".to_string(),
            "dangling".to_string(),
        ];
        for path in &escapes {
            let read = run(&ws, "read", json!({"path": path}));
            assert!(read.is_error && !read.text.contains("secret"), "read {path}: {}", read.text);
            let write = run(&ws, "write", json!({"path": path, "content": "pwned"}));
            assert!(write.is_error, "write {path}");
            let edit = run(&ws, "edit", json!({"path": path, "old": "secret", "new": "x"}));
            assert!(edit.is_error, "edit {path}");
        }
        assert_eq!(fs::read_to_string(&canary).unwrap(), "secret");
        assert!(!outside.path().join("new.txt").exists());
        assert!(!dir.path().join("nowhere").exists());

        // Symlinks that stay inside the workspace are fine.
        let ok = run(&ws, "read", json!({"path": "sub/inner_link"}));
        assert!(!ok.is_error && ok.text.contains("hello"), "{}", ok.text);
        // Absolute paths inside the workspace are fine too.
        let abs = dir.path().canonicalize().unwrap().join("inside.txt");
        assert!(!run(&ws, "read", json!({"path": abs})).is_error);
    }

}
