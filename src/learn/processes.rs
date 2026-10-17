//! Live Dial process inventory. Commands stay private; controls use a refreshed PID identity.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use crate::platform::process_record::{self as process_record, Log, Record};
use crate::platform::procinfo::ProcId;

/// How long a restart waits for the old process to exit before giving up.
const RESTART_EXIT_WAIT: Duration = Duration::from_secs(10);

#[cfg(windows)]
#[path = "processes_windows.rs"]
mod native;

#[derive(Debug)]
struct Process {
    pid: i32,
    parent: i32,
    #[cfg(unix)]
    group: i32,
    cpu: Option<f64>,
    memory: Option<u64>,
    started: String,
    command: String,
    /// Kernel identity; `None` when it cannot be verified, which makes the process uncontrollable.
    id: Option<ProcId>,
}
impl Process {
    fn identity(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("{}\n{}\n{}", self.pid, self.started, self.command))
        )
    }

    fn program(&self) -> &str {
        #[cfg(windows)]
        {
            &self.command
        }
        #[cfg(unix)]
        {
            self.command.split_whitespace().next().unwrap_or("")
        }
    }

    fn is_dial(&self) -> bool {
        Path::new(self.program()).file_name().is_some_and(|name| {
            name == "dial"
                || cfg!(windows)
                    && name
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("dial.exe"))
        })
    }
}

#[cfg(any(unix, test))]
fn parse(output: &str) -> Vec<Process> {
    output
        .lines()
        .filter_map(|line| {
            let mut rest = line.trim();
            let mut next = || {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let token = &rest[..end];
                rest = rest[end..].trim_start();
                token
            };
            let pid = next().parse().ok()?;
            let parent = next().parse().ok()?;
            #[cfg(unix)]
            let group = next().parse().ok()?;
            #[cfg(not(unix))]
            next();
            let cpu = next().parse().ok()?;
            let memory = next().parse::<u64>().ok()?.saturating_mul(1024);
            let started = (0..5)
                .map(|_| next().to_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if rest.is_empty() || pid <= 1 {
                return None;
            }
            Some(Process {
                pid,
                parent,
                #[cfg(unix)]
                group,
                cpu: Some(cpu),
                memory: Some(memory),
                started,
                command: rest.to_owned(),
                id: ProcId::of(pid),
            })
        })
        .collect()
}
#[cfg(unix)]
fn read() -> Result<Vec<Process>> {
    let output = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-axo", "pid=,ppid=,pgid=,%cpu=,rss=,lstart=,args="])
        .output()
        .context("reading live processes")?;
    ensure!(
        output.status.success(),
        "live process inspection is unavailable"
    );
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(windows)]
fn read() -> Result<Vec<Process>> {
    use std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    static SAMPLES: LazyLock<Mutex<HashMap<(i32, u64), (u64, Instant)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let owner = native::current_user()?;
    let mut samples = SAMPLES.lock().unwrap_or_else(|poison| poison.into_inner());
    let scan_started = Instant::now();
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(
        raw != INVALID_HANDLE_VALUE,
        "reading live processes: {}",
        std::io::Error::last_os_error()
    );
    struct Snapshot(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let snapshot = Snapshot(raw);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    ensure!(
        unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0,
        "enumerating live processes: {}",
        std::io::Error::last_os_error()
    );
    let mut out = Vec::new();
    loop {
        let pid = entry.th32ProcessID as i32;
        if let Ok(handle) = native::NativeProcess::open(entry.th32ProcessID, false) {
            if handle.owned_by(&owner).unwrap_or(false) {
                if let Ok((mut process, cpu_ticks, born)) =
                    handle.inspect(pid, entry.th32ParentProcessID as i32)
                {
                    let now = Instant::now();
                    if let Some((previous, at)) = samples.insert((pid, born), (cpu_ticks, now)) {
                        let elapsed = now.duration_since(at).as_secs_f64();
                        if elapsed > 0.0 && cpu_ticks >= previous {
                            process.cpu =
                                Some((cpu_ticks - previous) as f64 / (elapsed * 100_000.0));
                        }
                    }
                    process.id = ProcId::of(pid);
                    out.push(process);
                }
            }
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            break;
        }
    }
    samples.retain(|_, (_, at)| *at >= scan_started);
    Ok(out)
}

/// Tests that inspect live processes cannot run where `ps` is denied (for example inside a sandbox).
#[cfg(test)]
pub(crate) fn live_inspection_unavailable() -> bool {
    let unavailable = read().is_err();
    if unavailable {
        eprintln!("skipping: live process inspection is unavailable in this environment");
    }
    unavailable
}

fn classify(user: &Path, process: &Process, all: &[Process]) -> Option<(&'static str, String)> {
    let command = &process.command;
    let program = process.program();
    let managed_python = Path::new(program).starts_with(user.join("python"));
    let laya_python = Path::new(program).starts_with(user.join("laya"));
    let model = command
        .split_whitespace()
        .find_map(|word| word.strip_prefix("--model="))
        .map(str::to_owned)
        .unwrap_or_default();
    if managed_python
        && (command.contains("mlx_lm.server")
            || command.contains("Dial's MLX server")
            || command.contains("dial_steering"))
    {
        return Some(("models", model));
    }
    if laya_python && command.contains("dial_laya_server:app") {
        return Some(("services", "Laya decisions".into()));
    }
    if managed_python && command.contains("dial_worker.py") {
        return Some(("models", "Model worker".into()));
    }
    if command.contains("__dial_kernel__") && command.contains("Trusted JSON-lines kernel driver") {
        return Some(("tools", "Python kernel".into()));
    }
    // Descendants of a live Dial session include kernels, MCP/LSP servers and shell tools.
    // Other Python programs need a Dial ancestor.
    let mut parent = process.parent;
    for _ in 0..16 {
        let ancestor = all.iter().find(|p| p.pid == parent)?;
        if ancestor.is_dial() {
            return Some((
                "tools",
                Path::new(program)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        parent = ancestor.parent;
    }
    None
}
fn inventory(user: &Path, all: &[Process]) -> Vec<Value> {
    let mut rows = Vec::new();
    for process in all {
        if process.pid == std::process::id() as i32 {
            continue;
        }
        // Registration supplies startup/activity and model identity even with a custom Python path.
        let record = fs::read(
            user.join("compute-processes")
                .join(format!("{}.json", process.pid)),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| {
            value["pid"] == process.pid
                && value["parent_pid"] == process.parent
                && process.id.is_some()
                && process_record::proc_id(value) == process.id
        });
        let classified = classify(user, process, all);
        if record.is_none() && classified.is_none() {
            continue;
        }
        // Dial sessions and detached schedulers have separate controls; don't terminate them as tools.
        if process.is_dial()
            || Path::new(process.program())
                .file_name()
                .is_some_and(|name| name == "ps")
        {
            continue;
        }
        let mut row = record.unwrap_or_else(|| {
            let (kind, model) = classified.unwrap();
            json!({"kind":kind,"model":model,"phase":"running","project":"","state":""})
        });
        row["pid"] = json!(process.pid);
        row["parent_pid"] = json!(process.parent);
        row["cpu_percent"] = process.cpu.map_or(Value::Null, |value| json!(value));
        row["memory_bytes"] = process.memory.map_or(Value::Null, |value| json!(value));
        row["identity"] = json!(process.identity());
        row["orphaned"] = json!(process.parent == 1 && row["kind"] != "services");
        rows.push(row);
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row["memory_bytes"].as_u64().unwrap_or(0)));
    rows
}
#[cfg(target_os = "macos")]
fn footprint(pid: i32) -> Option<u64> {
    // RSS misses compressed and Metal-backed allocations. Use Darwin's process footprint.
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
    (result == 0).then(|| unsafe { usage.assume_init().ri_phys_footprint })
}
#[cfg(not(target_os = "macos"))]
fn footprint(_pid: i32) -> Option<u64> {
    None
}

pub struct UsageSnapshot {
    pub processes: Vec<Value>,
    pub session_memory_bytes: u64,
    pub session_count: usize,
}

pub fn snapshot_with_usage(user: &Path) -> Result<UsageSnapshot> {
    let all = read()?;
    let mut processes = inventory(user, &all);
    for row in &mut processes {
        row["resident_memory_bytes"] = row["memory_bytes"].clone();
        if let Some(bytes) = row["pid"].as_i64().and_then(|pid| footprint(pid as i32)) {
            row["memory_bytes"] = json!(bytes);
            row["memory_metric"] = json!("footprint");
        } else {
            row["memory_metric"] = json!("rss");
        }
    }
    processes.sort_by_key(|row| std::cmp::Reverse(row["memory_bytes"].as_u64().unwrap_or(0)));
    let sessions: Vec<_> = all.iter().filter(|process| process.is_dial()).collect();
    let session_memory_bytes = sessions.iter().fold(0_u64, |sum, process| {
        sum.saturating_add(footprint(process.pid).or(process.memory).unwrap_or(0))
    });
    Ok(UsageSnapshot {
        processes,
        session_memory_bytes,
        session_count: sessions.len(),
    })
}

pub fn snapshot(user: &Path) -> Result<Vec<Value>> {
    Ok(snapshot_with_usage(user)?.processes)
}

#[derive(Clone, Copy)]
enum Control {
    Stop,
    Kill,
}

impl Control {
    fn verb(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Kill => "kill",
        }
    }
}

/// Finds live process `pid` and re-checks it against the identity the caller listed.
fn select<'a>(
    user: &Path,
    all: &'a [Process],
    pid: i32,
    identity: &str,
) -> Result<(&'a Process, Value)> {
    let process = all
        .iter()
        .find(|p| p.pid == pid)
        .context("process already exited")?;
    ensure!(
        process.identity() == identity,
        "process changed; refresh and select it again"
    );
    ensure!(
        process.id.is_some(),
        "process identity is unavailable; refresh and select it again"
    );
    let row = inventory(user, all)
        .into_iter()
        .find(|row| row["pid"] == pid)
        .context("process is no longer managed by Compute")?;
    Ok((process, row))
}

/// Sends the control signal to a selected process. Only a private child group is signalled as a
/// group; the foreground session's shared group is never targeted.
fn signal(process: &Process, pid: i32, control: Control) -> Result<()> {
    #[cfg(unix)]
    let result: Result<()> = {
        let id = process
            .id
            .context("process identity is unavailable; refresh and select it again")?;
        let sent = match (control, process.group == pid) {
            (Control::Stop, true) => id.signal_group(libc::SIGTERM),
            (Control::Stop, false) => id.signal(libc::SIGTERM),
            (Control::Kill, true) => id.signal_group(libc::SIGKILL),
            (Control::Kill, false) => id.signal(libc::SIGKILL),
        };
        ensure!(sent, "process is no longer running");
        Ok(())
    };
    #[cfg(windows)]
    let result = {
        let handle = native::NativeProcess::open(pid as u32, true)?;
        ensure!(
            handle.owned_by(&native::current_user()?)?,
            "process belongs to another user"
        );
        let (current, _, _) = handle.inspect(pid, process.parent)?;
        ensure!(
            current.identity() == process.identity(),
            "process changed; refresh and select it again"
        );
        crate::platform::process::kill_checked(handle.handle())
    };
    result.with_context(|| format!("could not {} process", control.verb()))
}

/// Shared request path for `stop` and `kill`: validates the pid, re-checks identity, then signals.
fn send_control(user: &Path, pid: i32, identity: &str, control: Control) -> Result<Value> {
    ensure!(
        pid > 1 && pid != std::process::id() as i32,
        "cannot {} this process",
        control.verb()
    );
    let all = read()?;
    let (process, row) = select(user, &all, pid, identity)?;
    signal(process, pid, control)?;
    Ok(row)
}

/// Graceful stop (SIGTERM on Unix). Models reload on their next use.
pub fn stop(user: &Path, pid: i32, identity: &str) -> Result<String> {
    send_control(user, pid, identity, Control::Stop)?;
    Ok(
        "Process stop requested. Models reload on their next use; saved model files are preserved."
            .into(),
    )
}

/// Forced termination (SIGKILL on Unix). Same identity and ownership checks as [`stop`].
pub fn kill(user: &Path, pid: i32, identity: &str) -> Result<String> {
    send_control(user, pid, identity, Control::Kill)?;
    Ok("Process killed. Models reload on their next use; saved model files are preserved.".into())
}

/// Stops a managed process, waits for it to exit, and starts the same program again from its
/// recorded spawn spec. The new process is detached and tracked under its new pid.
pub fn restart(user: &Path, pid: i32, identity: &str) -> Result<String> {
    ensure!(
        pid > 1 && pid != std::process::id() as i32,
        "cannot restart this process"
    );
    let all = read()?;
    let (process, _) = select(user, &all, pid, identity)?;
    let record = verified_record(user, pid, process)?;
    let spec = process_record::spawn_spec(&record)?.with_context(|| {
        format!("process {pid} has no recorded spawn spec, so it cannot be restarted")
    })?;
    signal(process, pid, Control::Stop)?;
    wait_until_gone(pid, RESTART_EXIT_WAIT)?;
    // The old process and its record/output are finished; clear what it left behind.
    let _ = fs::remove_file(user.join("compute-processes").join(format!("{pid}.json")));
    if let Some(old_log) = record["log"].as_str().map(Path::new)
        && log_is_managed(user, old_log)
    {
        let _ = fs::remove_file(old_log);
    }
    let log = Log::create(user)?;
    let (stdout, stderr) = log.stdio()?;
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).current_dir(&spec.cwd).env_clear();
    for name in &spec.env_allow {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .env("DIAL_PARENT_PID", std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    let mut child = spawn_detached(&mut command)
        .with_context(|| format!("restarting {}", spec.program.display()))?;
    let new_pid = child.id();
    let recorded_process = Record::detached(
        user,
        new_pid,
        &recorded(&record, "kind"),
        &recorded(&record, "model"),
        Path::new(&recorded(&record, "project")),
        Path::new(&recorded(&record, "state")),
        "loading",
    )
    .and_then(|mut restarted| {
        restarted.set_spawn(&spec, &log)?;
        Ok(restarted)
    });
    let restarted = match recorded_process {
        Ok(restarted) => restarted,
        Err(error) => {
            // Without a record the new process cannot be listed or controlled; do not leave it running.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    // Dropping a std `Child` neither waits nor kills, so the restarted process keeps running.
    drop(child);
    restarted.detach();
    Ok(format!(
        "Process restarted as pid {new_pid}. It is detached from this command; `dial ps` tracks it under the new pid."
    ))
}

/// Bounded tail of a managed process's captured stdout and stderr.
pub fn logs(user: &Path, pid: i32, identity: &str) -> Result<String> {
    let all = read()?;
    let (process, _) = select(user, &all, pid, identity)?;
    let record = verified_record(user, pid, process)?;
    let path = record["log"]
        .as_str()
        .with_context(|| format!("process {pid} has no captured output"))?;
    let path = Path::new(path);
    ensure!(
        log_is_managed(user, path),
        "process {pid} records a log outside Dial's compute log directory"
    );
    process_record::read_tail(path, process_record::LOG_TAIL_BYTES)
}

fn log_is_managed(user: &Path, path: &Path) -> bool {
    path.parent() == Some(user.join("compute-processes").join("logs").as_path())
}

fn recorded(record: &Value, key: &str) -> String {
    record[key].as_str().unwrap_or("").to_owned()
}

/// The stored record for `process`, only when it was written for this exact kernel identity.
fn verified_record(user: &Path, pid: i32, process: &Process) -> Result<Value> {
    let record = process_record::load(user, pid as u32)?
        .with_context(|| format!("process {pid} has no process record"))?;
    ensure!(
        process.id.is_some() && process_record::proc_id(&record) == process.id,
        "process {pid} record does not match the live process"
    );
    Ok(record)
}

fn wait_until_gone(pid: i32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while read()?.iter().any(|process| process.pid == pid) {
        ensure!(
            Instant::now() < deadline,
            "process {pid} did not exit after stop; it was not restarted"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Starts a child that outlives this command. Unix children get their own process group, as in
/// `spawn_group`. Windows job objects kill their members when the launcher exits, so none is used.
fn spawn_detached(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        crate::platform::process::spawn_group(command)
    }
    #[cfg(windows)]
    {
        command.spawn()
    }
}

#[cfg(test)]
mod tests;
