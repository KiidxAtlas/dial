//! Persistent, sandboxed Python execution. Deadlines/cancellation discard the entire kernel.
use super::ext::{Tool, ToolContext};
use super::shell::{ProcessGroup, shell_command};
use super::{ToolOutput, tool_schema};
use crate::config::types::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, ChildStdout, ChildStderr, Command, Stdio};
use parking_lot::Mutex;
use std::sync::{Arc, atomic::{AtomicBool, AtomicI32, Ordering}};
use std::time::{Duration, Instant};

pub(crate) const PYTHON_DRIVER: &str = include_str!("kernel_driver.py");
const MAX_CODE: usize = 64 * 1024;
pub(crate) const MAX_FRAME: usize = 12 * 1024 * 1024;
// No descendant can detach: even with the operator's filesystem sandbox opt-out, fork/spawn is denied.
const FORK_FENCE: &str = "(version 1)(allow default)(deny process-fork)";
const PYTHON_PREFLIGHT: &str = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'executable':sys.executable}}),flush=True)";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig { pub python: String, pub timeout_secs: u64 }
impl Default for KernelConfig {
    fn default() -> Self { Self { python: "python3".into(), timeout_secs: 30 } }
}

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(KernelTool { config: config.kernel.clone(), session: Mutex::new(None), pid: Arc::new(AtomicI32::new(0)), interrupt:AtomicBool::new(false) })]
}
struct KernelTool { config: KernelConfig, session: Mutex<Option<JsonProcess>>, pid: Arc<AtomicI32>, interrupt:AtomicBool }
impl Tool for KernelTool {
    fn name(&self) -> &str { "kernel" }
    fn schema(&self) -> Value {
        tool_schema("kernel", "Persistent Python namespace. Every call requires approval. eval executes code under the workspace sandbox plus an OS process-fork fence: subprocess creation/fork/spawn is disabled so detached descendants cannot survive reset. The fork fence is required even if filesystem sandboxing is disabled. stdout, stderr, final expression and tracebacks are returned. Timeout/cancel destroys the namespace; reset starts a fresh namespace lazily.", json!({
            "action":{"type":"string","enum":["eval","reset","cancel","status"]},
            "code":{"type":"string"}, "timeout":{"type":"integer","minimum":1,"maximum":3600}
        }), &["action"])
    }
    fn needs_approval(&self, _: &Value) -> bool { true }
    fn cancel_operation(&self) {
        self.interrupt.store(true,Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 { unsafe { libc::killpg(pid, libc::SIGKILL); } }
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match self.run(ctx, args) {
            Ok(v) if v.get("error").is_some_and(|e| !e.is_null()) => ToolOutput::err(v.to_string()),
            Ok(v) => ToolOutput::ok(v.to_string()), Err(e) => ToolOutput::err(e)
        }
    }
}
impl KernelTool {
    fn run(&self, ctx: &ToolContext, args: &Value) -> Result<Value, String> {
        let action = args.get("action").and_then(Value::as_str).ok_or("action is required")?;
        if action == "cancel" {
            self.interrupt.store(true,Ordering::Release);
            let pid = self.pid.load(Ordering::Acquire);
            if pid > 0 { unsafe { libc::killpg(pid, libc::SIGKILL); } }
            // Do not take the session mutex: cancellation must interrupt an in-flight eval.
            return Ok(json!({"cancelled":pid > 0,"namespace_discarded":true}));
        }
        let mut slot = self.session.lock();
        if action == "reset" { slot.take(); return Ok(json!({"reset":true})); }
        if action == "status" { return Ok(json!({"running":slot.as_mut().is_some_and(JsonProcess::alive)})); }
        if action != "eval" { return Err(format!("unsupported kernel action {action:?}")); }
        let code = args.get("code").and_then(Value::as_str).ok_or("eval requires code")?;
        if code.len() > MAX_CODE { return Err(format!("code exceeds {MAX_CODE} bytes")); }
        let timeout = deadline(args, self.config.timeout_secs)?;
        self.interrupt.store(false,Ordering::Release);
        let end = Instant::now() + timeout;
        if slot.as_mut().is_some_and(|s| !s.alive()) { slot.take(); }
        if slot.is_none() {
            let python = resolve_python(ctx.workspace.root(), &self.config.python, timeout, self.pid.clone())?;
            if self.interrupt.load(Ordering::Acquire) { return Err("kernel startup cancelled".into()); }
            let script = format!("exec /usr/bin/sandbox-exec -p {} {} -u -c {}", shell_quote(FORK_FENCE), shell_quote(python.to_str().ok_or("Python executable path is not UTF-8")?), shell_quote(PYTHON_DRIVER));
            let command = shell_command(ctx.workspace, &script, ctx.workspace.root())?;
            super::sandbox::Sandbox::detect().available().map_err(|e| format!("kernel requires the OS process-fork containment fence even when filesystem sandboxing is disabled: {e}"))?;
            *slot = Some(JsonProcess::start(command, self.pid.clone()).map_err(|e| format!("starting Python kernel (install/configure kernel.python): {e}"))?);
        }
        if self.interrupt.load(Ordering::Acquire) { slot.take(); return Err("kernel startup cancelled".into()); }
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() { slot.take(); return Err("kernel startup exceeded deadline".into()); }
        let result = slot.as_mut().unwrap().request(json!({"code":code}), remaining, |_| Err("unexpected kernel callback".into())).map_err(|e| {
            if e.starts_with("execution process exited:") { format!("{e}\nIf Python could not start, install Python 3 and configure kernel.python.") } else { e }
        });
        if result.is_err() { slot.take(); }
        result
    }
}
pub(crate) fn resolve_operator_executable(workspace: &Path, program: &str, label: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(program);
    let candidate = if path.is_absolute() || path.components().count() > 1 {
        if path.is_absolute() { path.to_path_buf() } else { workspace.join(path) }
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
            .ok_or_else(|| format!("cannot find {label} {program:?}; install its operator-owned executable"))?
    };
    let resolved = candidate.canonicalize().map_err(|e| format!("{label} {program:?}: {e}"))?;
    if resolved.starts_with(workspace) { return Err(format!("{label} must be an operator-installed executable outside the editable workspace")); }
    Ok(resolved)
}

fn resolve_python(workspace: &Path, program: &str, timeout: Duration, pid: Arc<AtomicI32>) -> Result<PathBuf, String> {
    let launcher = resolve_operator_executable(workspace, program, "kernel.python")?;
    let scratch = tempfile::Builder::new().prefix("dial-kernel-resolve-").tempdir().map_err(|e| e.to_string())?;
    let mut command = Command::new(launcher);
    // Resolve pyenv/other operator shims before applying the fork fence, but never import workspace/site code.
    command.args(["-I","-S","-u","-c",PYTHON_PREFLIGHT]).env_clear().current_dir(scratch.path());
    for (key,value) in std::env::vars() {
        if !super::sandbox::is_secret_env(&key,&value) { command.env(key,value); }
    }
    command.env("PWD",scratch.path()).env("PYENV_DIR",scratch.path());
    let mut process = JsonProcess::start(command,pid)?;
    let result = process.request(json!({}),timeout,|_| Err("unexpected Python resolution callback".into()))?;
    let executable = result["executable"].as_str().ok_or("Python preflight did not report sys.executable")?;
    resolve_operator_executable(workspace,executable,"resolved kernel.python")
}


pub(crate) fn deadline(args: &Value, configured: u64) -> Result<Duration, String> {
    if !(1..=3600).contains(&configured) { return Err("configured timeout_secs must be in 1..=3600".into()); }
    let requested = match args.get("timeout") { None => configured, Some(v) => v.as_u64().filter(|n| (1..=3600).contains(n)).ok_or("timeout must be in 1..=3600")? };
    Ok(Duration::from_secs(requested.min(configured)))
}
pub(crate) fn shell_quote(text: &str) -> String { format!("'{}'", text.replace('\'', "'\\''")) }

/// Bounded JSON-lines protocol. Pipes are nonblocking so neither output nor input can defeat deadlines.
pub(crate) struct JsonProcess {
    group: ProcessGroup, input: ChildStdin, output: ChildStdout, errors: ChildStderr,
    pending: Vec<u8>, stderr: Vec<u8>, stderr_dropped: usize, pid: Arc<AtomicI32>, sequence: u64,
}
impl JsonProcess {
    pub(crate) fn start(mut command: Command, pid: Arc<AtomicI32>) -> Result<Self, String> {
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().map_err(|e| e.to_string())?;
        let input = child.stdin.take().ok_or("child stdin unavailable")?;
        let output = child.stdout.take().ok_or("child stdout unavailable")?;
        let errors = child.stderr.take().ok_or("child stderr unavailable")?;
        let group = ProcessGroup::new(child);
        for fd in [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 { return Err(std::io::Error::last_os_error().to_string()); }
        }
        pid.store(group.child.id() as i32, Ordering::Release);
        Ok(Self { group, input, output, errors, pending: Vec::new(), stderr: Vec::new(), stderr_dropped: 0, pid, sequence: 0 })
    }
    pub(crate) fn alive(&mut self) -> bool { self.group.exited().is_ok_and(|exited| !exited) }
    fn write_frame(&mut self, value: &Value, end: Instant) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME { return Err("request frame too large".into()); }
        bytes.push(b'\n');
        let mut offset = 0;
        while offset < bytes.len() {
            if Instant::now() >= end { return Err("execution deadline exceeded; namespace/session discarded".into()); }
            match self.input.write(&bytes[offset..]) {
                Ok(0) => return Err("child stdin closed".into()), Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll(libc::POLLOUT),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}, Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
    fn poll(&self, event: i16) {
        let fd = if event == libc::POLLOUT { self.input.as_raw_fd() } else { self.output.as_raw_fd() };
        let mut poll = libc::pollfd { fd, events:event, revents:0 };
        unsafe { libc::poll(&mut poll, 1, 10); }
    }
    pub(crate) fn request(&mut self, mut value: Value, timeout: Duration, mut callback: impl FnMut(&Value) -> Result<Value, String>) -> Result<Value, String> {
        let end = Instant::now() + timeout;
        self.sequence += 1;
        value["id"] = json!(self.sequence);
        self.write_frame(&value, end)?;
        loop {
            // Drain bounded stderr as well as stdout; verbose adapters cannot deadlock.
            let mut buffer = [0u8; 16384];
            for _ in 0..8 {
                match self.errors.read(&mut buffer) {
                    Ok(0) => break, Ok(n) => {
                        let remaining = (16 * 1024usize).saturating_sub(self.stderr.len());
                        self.stderr.extend_from_slice(&buffer[..n.min(remaining)]);
                        self.stderr_dropped = self.stderr_dropped.saturating_add(n.saturating_sub(remaining));
                    },
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue, Err(_) => break,
                }
            }
            if Instant::now() >= end { return Err("execution deadline exceeded; namespace/session discarded".into()); }
            if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                let line = self.pending.drain(..=index).collect::<Vec<_>>();
                let reply: Value = serde_json::from_slice(&line).map_err(|e| format!("invalid adapter response: {e}"))?;
                if reply.get("callback").is_some() {
                    let operation = reply.get("operation_id").and_then(Value::as_u64);
                    let outcome = if operation != Some(self.sequence) { Err("browser callback belongs to an expired operation".into()) } else { callback(&reply) };
                    let response = match outcome { Ok(v) => json!({"callback_id":reply["callback_id"],"result":v}), Err(e) => json!({"callback_id":reply["callback_id"],"error":e}) };
                    self.write_frame(&response, end)?;
                    continue;
                }
                if reply.get("id").and_then(Value::as_u64) != Some(self.sequence) { return Err("adapter response sequence mismatch".into()); }
                if let Some(error) = reply.get("error").and_then(Value::as_str) { return Err(error.into()); }
                let mut result = reply.get("result").cloned().unwrap_or(Value::Null);
                if !self.stderr.is_empty() {
                    if let Some(object) = result.as_object_mut() {
                        object.insert("process_output".into(), json!(String::from_utf8_lossy(&self.stderr)));
                        if self.stderr_dropped > 0 { object.insert("process_output_truncated_bytes".into(), json!(self.stderr_dropped)); }
                    }
                    self.stderr.clear();
                    self.stderr_dropped = 0;
                }
                return Ok(result);
            }
            match self.output.read(&mut buffer) {
                Ok(0) => return Err(format!("execution process exited: {}", String::from_utf8_lossy(&self.stderr))),
                Ok(n) => { if self.pending.len() + n > MAX_FRAME { return Err("adapter output exceeded frame limit".into()); } self.pending.extend_from_slice(&buffer[..n]); },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll(libc::POLLIN),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}, Err(e) => return Err(e.to_string()),
            }
        }
    }
}
impl Drop for JsonProcess {
    fn drop(&mut self) { self.group.terminate(); self.pid.store(0, Ordering::Release); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn quote_preserves_shell_metacharacters() { assert_eq!(shell_quote("a'b;$(x)"), "'a'\\''b;$(x)'"); }
    #[test] fn deadline_is_operator_bounded() {
        assert_eq!(deadline(&json!({"timeout":99}), 5).unwrap(), Duration::from_secs(5));
        assert!(deadline(&json!({"timeout":0}), 5).is_err());
    }
    fn process() -> JsonProcess {
        let mut command = Command::new("python3"); command.args(["-u", "-c", PYTHON_DRIVER]);
        JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap()
    }
    #[test] fn python_state_stdout_tracebacks_and_reset() {
        let mut p = process();
        let call = |p: &mut JsonProcess, code: &str| p.request(json!({"code":code}), Duration::from_secs(5), |_| unreachable!()).unwrap();
        assert_eq!(call(&mut p, "x = 40\nprint('hello')")["stdout"], "hello\n");
        assert_eq!(call(&mut p, "x + 2")["value"], "42");
        assert!(call(&mut p, "1 / 0")["error"].as_str().unwrap().contains("ZeroDivisionError"));
        assert_eq!(call(&mut p, "x")["value"], "40");
        drop(p);
        let mut fresh = process();
        assert!(call(&mut fresh, "x")["error"].as_str().unwrap().contains("NameError"));
    }
    #[test] fn endless_python_has_a_deadline() {
        let mut p = process();
        let result = p.request(json!({"code":"while True: pass"}), Duration::from_millis(100), |_| unreachable!());
        assert!(result.unwrap_err().contains("deadline"));
    }
    #[test] fn noisy_output_is_bounded() {
        let mut p = process();
        let v = p.request(json!({"code":"print('x' * 100000)"}), Duration::from_secs(5), |_| unreachable!()).unwrap();
        assert!(v["stdout"].as_str().unwrap().len() < 25000);
        assert!(v["stdout"].as_str().unwrap().contains("truncated"));
    }
    #[test] fn native_output_is_captured_without_protocol_corruption() {
        let mut p = process();
        let v = p.request(json!({"code":"import os\nos.write(1, b'native-output')"}), Duration::from_secs(5), |_| unreachable!()).unwrap();
        assert!(v["process_output"].as_str().unwrap().contains("native-output"));
    }
    #[test] fn drop_kills_and_reaps_the_process_group_leader() {
        let p = process();
        let pid = p.group.child.id() as libc::pid_t;
        drop(p);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[test] fn unavailable_sandbox_never_runs_model_code() {
        use super::super::{Workspace, sandbox::Sandbox};
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap().with_sandbox(Sandbox::Unavailable("test-denied".into()));
        let config = Config::default();
        let ctx = ToolContext { workspace:&workspace, config:&config };
        let tool = tools(&config).pop().unwrap();
        let out = tool.call(&ctx, &json!({"action":"eval","code":"open('escaped', 'w').write('no')"}));
        assert!(out.is_error && out.text.contains("test-denied"), "{}", out.text);
        assert!(!dir.path().join("escaped").exists());
    }
    #[cfg(target_os = "macos")]
    #[test] fn cancellation_interrupts_an_inflight_kernel_without_session_lock() {
        let dir = tempfile::tempdir().unwrap();
        // The transport/control test opts out explicitly; confinement has a separate fail-closed test.
        let workspace = super::super::Workspace::new(dir.path()).unwrap().without_sandbox();
        let config = Config::default();
        let tool = Arc::new(KernelTool {config:KernelConfig::default(), session:Mutex::new(None), pid:Arc::new(AtomicI32::new(0)), interrupt:AtomicBool::new(false)});
        let running = tool.clone();
        let thread = std::thread::spawn(move || {
            running.call(&ToolContext {workspace:&workspace,config:&config}, &json!({"action":"eval","code":"while True: pass"}))
        });
        let end = Instant::now() + Duration::from_secs(5);
        while tool.pid.load(Ordering::Acquire) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(tool.pid.load(Ordering::Acquire) > 0);
        tool.cancel_operation();
        assert!(thread.join().unwrap().is_error);
        assert_eq!(tool.pid.load(Ordering::Acquire), 0);
        assert!(tool.session.lock().is_none());
    }
    #[cfg(target_os = "macos")]
    #[test] fn kernel_denies_fork_spawn_and_detached_descendants_even_without_filesystem_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = super::super::Workspace::new(dir.path()).unwrap().without_sandbox();
        let config = Config::default();
        let tool = tools(&config).pop().unwrap();
        let ctx = ToolContext {workspace:&workspace,config:&config};
        for code in [
            "import subprocess, sys\np = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\np.pid",
            "import os, sys\nos.posix_spawn(sys.executable, [sys.executable, '-c', 'import time; time.sleep(60)'], os.environ)",
            "import os\nos.fork()",
        ] {
            let output = tool.call(&ctx, &json!({"action":"eval","code":code}));
            // Clean up a spawned PID even if containment regresses, before failing the test.
            if !output.is_error {
                let value:Value = serde_json::from_str(&output.text).unwrap();
                if let Some(pid) = value["value"].as_str().and_then(|s| s.parse::<i32>().ok()).filter(|pid| *pid > 0) {
                    unsafe { libc::kill(pid,libc::SIGKILL); }
                }
            }
            assert!(output.is_error && output.text.contains("PermissionError"),"fork containment failed: {}",output.text);
        }
        assert!(!tool.call(&ctx, &json!({"action":"reset"})).is_error);
    }
    #[test] fn expired_callback_operation_never_invokes_the_network_handler() {
        let driver = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'callback':'http','callback_id':1,'operation_id':0}),flush=True)\nreply=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':reply}),flush=True)";
        let mut command = Command::new("python3");
        command.args(["-u","-c",driver]);
        let mut process = JsonProcess::start(command,Arc::new(AtomicI32::new(0))).unwrap();
        let value = process.request(json!({}),Duration::from_secs(5),|_| panic!("expired callback performed networking")).unwrap();
        assert!(value["error"].as_str().unwrap().contains("expired operation"));
    }
    #[test] fn workspace_python_launcher_is_rejected_before_execution() {
        use std::os::unix::fs::PermissionsExt;
        let dir=tempfile::tempdir().unwrap();
        let outside=tempfile::tempdir().unwrap();
        let marker=outside.path().join("escaped");
        let launcher=dir.path().join("python3");
        std::fs::write(&launcher,format!("#!/bin/sh\nprintf escaped > {}\n",shell_quote(marker.to_str().unwrap()))).unwrap();
        std::fs::set_permissions(&launcher,std::fs::Permissions::from_mode(0o755)).unwrap();
        let root=dir.path().canonicalize().unwrap();
        let error=resolve_python(&root,launcher.to_str().unwrap(),Duration::from_secs(5),Arc::new(AtomicI32::new(0))).unwrap_err();
        assert!(error.contains("outside the editable workspace"));
        assert!(!marker.exists());
    }
}
