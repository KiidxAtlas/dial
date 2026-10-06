//! Debug Adapter Protocol tools. Adapters start lazily on the first approved call.
use crate::tools::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace, tool_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_HEADER: usize = 8 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const DESCRIPTION: &str = "Approved Debug Adapter Protocol control for a configured adapter. Each call requires approval; launch program/cwd must remain inside the workspace.";

/// Operator-controlled command used to start one DAP adapter.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DapAdapterConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    60
}

/// Creates one lazy, single-session tool per configured adapter.
pub fn tools(adapters: &BTreeMap<String, DapAdapterConfig>) -> Vec<Arc<dyn Tool>> {
    adapters
        .iter()
        .map(|(id, config)| {
            Arc::new(DapTool {
                name: format!("dap_{id}"),
                config: config.clone(),
                session: Mutex::new(None),
            }) as Arc<dyn Tool>
        })
        .collect()
}

struct DapTool {
    name: String,
    config: DapAdapterConfig,
    session: Mutex<Option<Session>>,
}

impl Tool for DapTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        tool_schema(
            &self.name,
            DESCRIPTION,
            json!({
                "action": {"type":"string","enum":["initialize","launch","attach","setBreakpoints","continue","next","stepIn","stepOut","stackTrace","scopes","variables","evaluate","disconnect","terminate"]},
                "program": {"type":"string","description":"launch: program path inside the workspace"},
                "cwd": {"type":"string","description":"launch: working directory inside the workspace (defaults to workspace root)"},
                "args": {"type":"array","items":{"type":"string"},"description":"launch: program arguments"},
                "arguments": {"type":"object","description":"Action-specific DAP arguments (attach, breakpoints, threads, frames, variables, or expression)"},
                "source": {"type":"object","description":"setBreakpoints: DAP source descriptor"},
                "breakpoints": {"type":"array","items":{"type":"object"},"description":"setBreakpoints: requested breakpoints"},
                "threadId": {"type":"integer"}, "frameId": {"type":"integer"},
                "variablesReference": {"type":"integer"}, "expression": {"type":"string"},
                "terminateDebuggee": {"type":"boolean"}
            }),
            &["action"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match self.call_inner(ctx.workspace, args) {
            Ok(value) => {
                let full = value.to_string();
                let mut text = bounded(&full);
                if full.len() > text.len() {
                    if let Some(store) = ctx.workspace.artifacts() {
                        match store.retain(&full) {
                            Ok(uri) => {
                                text.push_str(&format!("\n[Complete debugger evidence: {uri}]"))
                            }
                            Err(e) => {
                                return ToolOutput::err(format!(
                                    "retaining debugger result: {e:#}"
                                ));
                            }
                        }
                    }
                }
                ToolOutput::ok(text)
            }
            Err(error) => {
                let mut text = bounded(&error);
                if error.len() > text.len() {
                    if let Some(store) = ctx.workspace.artifacts() {
                        match store.retain(&error) {
                            Ok(uri) => {
                                text.push_str(&format!("\n[Complete debugger error: {uri}]"))
                            }
                            Err(e) => {
                                return ToolOutput::err(format!("retaining debugger error: {e:#}"));
                            }
                        }
                    }
                }
                ToolOutput::err(text)
            }
        }
    }
}

impl DapTool {
    fn call_inner(&self, workspace: &Workspace, args: &Value) -> Result<Value, String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("action must be a string")?;
        let command = match action {
            "initialize" => (
                "initialize",
                json!({"clientID":"dial","clientName":"dial","adapterID": self.name.trim_start_matches("dap_"),"linesStartAt1":true,"columnsStartAt1":true,"supportsVariableType":true,"supportsRunInTerminalRequest":false}),
            ),
            "launch" => ("launch", launch_arguments(workspace, args)?),
            "attach" => ("attach", action_arguments(args)?),
            "setBreakpoints" => ("setBreakpoints", breakpoint_arguments(args)?),
            "continue" => ("continue", action_arguments(args)?),
            "next" => ("next", action_arguments(args)?),
            "stepIn" => ("stepIn", action_arguments(args)?),
            "stepOut" => ("stepOut", action_arguments(args)?),
            "stackTrace" => ("stackTrace", action_arguments(args)?),
            "scopes" => ("scopes", action_arguments(args)?),
            "variables" => ("variables", action_arguments(args)?),
            "evaluate" => ("evaluate", action_arguments(args)?),
            "disconnect" => (
                "disconnect",
                json!({"terminateDebuggee":args.get("terminateDebuggee").and_then(Value::as_bool).unwrap_or(false)}),
            ),
            "terminate" => ("terminate", json!({})),
            other => return Err(format!("unsupported DAP action {other:?}")),
        };
        let mut slot = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(action, "disconnect" | "terminate") {
            let Some(mut session) = slot.take() else {
                return Err("no active DAP session".into());
            };
            let result = session.request(
                command.0,
                command.1,
                Duration::from_secs(self.config.timeout_secs.max(1)),
            );
            session.stop();
            return result;
        }
        if slot.is_none() {
            *slot = Some(Session::start(
                &self.config,
                self.name.trim_start_matches("dap_"),
            )?);
        }
        let session = slot.as_mut().expect("session just initialized");
        let result = if action == "initialize" {
            Ok(session.initialized.clone())
        } else {
            session.request(
                command.0,
                command.1,
                Duration::from_secs(self.config.timeout_secs.max(1)),
            )
        };
        if result.is_err() {
            slot.take();
        }
        result
    }
}

fn launch_arguments(workspace: &Workspace, args: &Value) -> Result<Value, String> {
    let program = args
        .get("program")
        .and_then(Value::as_str)
        .ok_or("launch requires program")?;
    let program = workspace.resolve(program)?;
    let program = canonical_inside(workspace, &program, "program")?;
    let cwd = match args.get("cwd").and_then(Value::as_str) {
        Some(path) => canonical_inside(workspace, &workspace.resolve(path)?, "cwd")?,
        None => workspace.root().to_path_buf(),
    };
    let mut result = args
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    result.insert("program".into(), json!(program));
    result.insert("cwd".into(), json!(cwd));
    if let Some(values) = args.get("args") {
        if !values
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
        {
            return Err("launch args must be an array of strings".into());
        }
        result.insert("args".into(), values.clone());
    }
    Ok(Value::Object(result))
}

fn canonical_inside(workspace: &Workspace, path: &Path, label: &str) -> Result<PathBuf, String> {
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve launch {label}: {error}"))?;
    if !resolved.starts_with(workspace.root()) {
        return Err(format!("launch {label} resolves outside the workspace"));
    }
    if label == "cwd" && !resolved.is_dir() {
        return Err("launch cwd must be a directory".into());
    }
    if label == "program" && !resolved.is_file() {
        return Err("launch program must be a file".into());
    }
    Ok(resolved)
}

fn action_arguments(args: &Value) -> Result<Value, String> {
    let value = args.get("arguments").cloned().unwrap_or_else(|| {
        let mut object = serde_json::Map::new();
        for key in ["threadId", "frameId", "variablesReference", "expression"] {
            if let Some(value) = args.get(key) {
                object.insert(key.into(), value.clone());
            }
        }
        Value::Object(object)
    });
    if !value.is_object() {
        return Err("arguments must be an object".into());
    }
    Ok(value)
}

fn breakpoint_arguments(args: &Value) -> Result<Value, String> {
    let mut value = action_arguments(args)?;
    let object = value.as_object_mut().expect("validated object");
    if let Some(source) = args.get("source") {
        object.insert("source".into(), source.clone());
    }
    if let Some(breakpoints) = args.get("breakpoints") {
        object.insert("breakpoints".into(), breakpoints.clone());
    }
    if !object.contains_key("source") || !object.get("breakpoints").is_some_and(Value::is_array) {
        return Err("setBreakpoints requires source and breakpoints".into());
    }
    Ok(value)
}

struct Session {
    child: Child,
    input: ChildStdin,
    responses: Receiver<Result<Value, String>>,
    next_seq: i64,
    initialized: Value,
}

impl Session {
    fn start(config: &DapAdapterConfig, adapter_id: &str) -> Result<Self, String> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start DAP adapter: {error}"))?;
        let Some(input) = child.stdin.take() else {
            let _ = child.kill();
            return Err("DAP adapter stdin unavailable".into());
        };
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            return Err("DAP adapter stdout unavailable".into());
        };
        let (sender, responses) = mpsc::sync_channel(32);
        if let Err(error) = thread::Builder::new()
            .name("dap-protocol-reader".into())
            .spawn(move || read_responses(output, sender))
        {
            let _ = child.kill();
            return Err(format!("failed to start DAP reader: {error}"));
        }
        let mut session = Self {
            child,
            input,
            responses,
            next_seq: 1,
            initialized: json!({}),
        };
        match session.request("initialize", json!({"clientID":"dial","clientName":"dial","adapterID":adapter_id,"linesStartAt1":true,"columnsStartAt1":true,"supportsVariableType":true,"supportsRunInTerminalRequest":false}), Duration::from_secs(config.timeout_secs.max(1))) {
            Ok(body) => session.initialized = body,
            Err(error) => {
                session.stop();
                return Err(format!("DAP initialize failed: {error}"));
            }
        }
        Ok(session)
    }

    fn request(
        &mut self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let seq = self.next_seq;
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or("DAP sequence number exhausted")?;
        let body =
            json!({"seq":seq,"type":"request","command":command,"arguments":arguments}).to_string();
        if body.len() > MAX_FRAME {
            return Err("DAP request exceeds frame limit".into());
        }
        write!(self.input, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|_| self.input.write_all(body.as_bytes()))
            .and_then(|_| self.input.flush())
            .map_err(|error| format!("writing DAP request: {error}"))?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("DAP {command} timed out"));
            }
            let response =
                self.responses
                    .recv_timeout(remaining)
                    .map_err(|error| match error {
                        mpsc::RecvTimeoutError::Timeout => format!("DAP {command} timed out"),
                        mpsc::RecvTimeoutError::Disconnected => {
                            "DAP adapter closed its output".into()
                        }
                    })??;
            if response.get("type").and_then(Value::as_str) != Some("response") {
                continue;
            }
            if response.get("request_seq").and_then(Value::as_i64) != Some(seq) {
                continue;
            }
            if response.get("command").and_then(Value::as_str) != Some(command) {
                return Err("DAP response command did not match request".into());
            }
            if response.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(format!(
                    "DAP {command} failed: {}",
                    response
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("adapter reported failure")
                ));
            }
            return Ok(response.get("body").cloned().unwrap_or_else(|| json!({})));
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_responses(output: impl Read, sender: SyncSender<Result<Value, String>>) {
    let mut reader = BufReader::new(output);
    loop {
        match read_frame(&mut reader) {
            Ok(Some(value)) => {
                if sender.send(Ok(value)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut length = None;
    let mut total = 0usize;
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader
            .read(&mut byte)
            .map_err(|error| format!("reading DAP header: {error}"))?;
        if read == 0 {
            if total == 0 {
                return Ok(None);
            }
            return Err("truncated DAP header".into());
        }
        total += 1;
        if total > MAX_HEADER {
            return Err("DAP header exceeds limit".into());
        }
        if byte[0] == b'\n' {
            if line == b"\r" {
                break;
            }
            let text = std::str::from_utf8(&line)
                .map_err(|_| "DAP header is not UTF-8")?
                .trim();
            if let Some((key, value)) = text.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    if length.is_some() {
                        return Err("duplicate DAP Content-Length".into());
                    }
                    length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| "invalid DAP Content-Length")?,
                    );
                }
            }
            line.clear();
        } else {
            line.push(byte[0]);
        }
    }
    let length = length.ok_or("DAP frame has no Content-Length")?;
    if length > MAX_FRAME {
        return Err("DAP frame exceeds limit".into());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("reading DAP body: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid DAP JSON: {error}"))
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_owned();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [DAP output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn adapter_config_deserializes_defaults_and_rejects_unknown_fields() {
        let config: DapAdapterConfig = toml::from_str("program = 'adapter'").unwrap();
        assert_eq!(config.program, PathBuf::from("adapter"));
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert_eq!(config.timeout_secs, 60);
        assert!(toml::from_str::<DapAdapterConfig>("program = 'adapter'\nunknown = true").is_err());
    }

    #[cfg(unix)]
    fn fake_adapter(dir: &Path, mode: &str) -> DapAdapterConfig {
        let script = dir.join("adapter.py");
        fs::write(&script, r#"#!/usr/bin/env python3
import json,os,sys,time
while True:
 h={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line in (b'\r\n',b'\n'): break
  k,v=line.decode().split(':',1); h[k.lower()]=v.strip()
 n=int(h['content-length']); req=json.loads(sys.stdin.buffer.read(n))
 mode=os.environ.get('DAP_FAKE_MODE','')
 if mode == 'invalid':
  sys.stdout.buffer.write(b'Content-Length: 1\r\n\r\n{');sys.stdout.buffer.flush();sys.exit(0)
 if mode == 'oversized':
  sys.stdout.buffer.write(b'Content-Length: 999999999\r\n\r\n');sys.stdout.buffer.flush();sys.exit(0)
 if mode == 'timeout': time.sleep(5)
 body={'seq':1,'type':'response','request_seq':req['seq'],'command':req['command'],'success':True,'body':{'received':req['command'],'args':req.get('arguments',{})}}
 out=json.dumps(body,separators=(',',':')).encode()
 sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(out)).encode()+out);sys.stdout.buffer.flush()
 if req['command'] in ('disconnect','terminate'): sys.exit(0)
"#).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        DapAdapterConfig {
            program: PathBuf::from("python3"),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([("DAP_FAKE_MODE".to_owned(), mode.to_owned())]),
            timeout_secs: if mode == "timeout" { 1 } else { 2 },
        }
    }

    #[cfg(unix)]
    fn context<'a>(
        workspace: &'a Workspace,
        config: &'a crate::config::types::Config,
    ) -> ToolContext<'a> {
        ToolContext { workspace, config }
    }

    #[cfg(unix)]
    #[test]
    fn fake_adapter_exercises_framing_lifecycle_and_workspace_validation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("program"), "").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let mut configs = BTreeMap::new();
        configs.insert("fake".to_owned(), fake_adapter(dir.path(), "valid"));
        let tool = tools(&configs).remove(0);
        let config = crate::config::types::Config::default();
        let ctx = context(&workspace, &config);
        assert_eq!(tool.name(), "dap_fake");
        assert!(tool.needs_approval(&json!({"action":"stackTrace"})));
        let init = tool.call(&ctx, &json!({"action":"initialize"}));
        assert!(!init.is_error, "{}", init.text);
        assert!(init.text.contains("\"adapterID\":\"fake\""));
        let launch = tool.call(&ctx, &json!({"action":"launch","program":"program"}));
        assert!(!launch.is_error, "{}", launch.text);
        assert!(launch.text.contains("\"cwd\":"));
        let bad = tool.call(&ctx, &json!({"action":"launch","program":"../outside"}));
        assert!(bad.is_error);
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("escape")).unwrap();
        let symlink_escape = tool.call(&ctx, &json!({"action":"launch","program":"escape"}));
        assert!(symlink_escape.is_error);
        let ended = tool.call(&ctx, &json!({"action":"disconnect"}));
        assert!(!ended.is_error, "{}", ended.text);
    }

    #[cfg(unix)]
    #[test]
    fn fake_adapter_protocol_failures_stop_the_session() {
        for mode in ["invalid", "oversized", "timeout"] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = Workspace::new(dir.path()).unwrap();
            let mut configs = BTreeMap::new();
            configs.insert("fake".to_owned(), fake_adapter(dir.path(), mode));
            let tool = tools(&configs).remove(0);
            let config = crate::config::types::Config::default();
            let ctx = context(&workspace, &config);
            let output = tool.call(&ctx, &json!({"action":"initialize"}));
            assert!(
                output.is_error,
                "{mode} adapter unexpectedly succeeded: {}",
                output.text
            );
        }
    }
}
